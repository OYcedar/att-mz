//! 严格 TOML 配置边界。
//!
//! 原始 TOML 只在本模块存在。结构和字段类型全部通过后，本模块继续建立非零资源
//! 上限、路径基准、语言模块与 Profile 唯一性；业务和根适配器只接收受信配置。

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};
use url::{Host, Url};
use zeroize::Zeroizing;

use crate::language::{
    EnglishLanguageModule, EnglishResidualPolicy, EnglishTranslationDetectionPolicy,
    JapaneseLanguageModule, JapaneseQuoteRepairPolicy, JapaneseResidualPolicy, LanguageModule,
    LanguageModuleCatalog, QuotePair,
};

const MAX_CONFIGURATION_BYTES: u64 = 4 * 1024 * 1024;
const RESERVED_REQUEST_BODY_FIELDS: [&str; 3] = ["model", "messages", "stream"];

/// 根据命令行选择配置文件位置。
///
/// 显式相对路径以当前工作目录解析；没有显式路径时使用 APPDATA 下的固定位置。
pub(crate) fn resolve_configuration_path(
    explicit: Option<&Path>,
    current_directory: &Path,
    app_data: Option<&OsStr>,
) -> Result<PathBuf, ConfigurationPathError> {
    if !current_directory.is_absolute() {
        return Err(ConfigurationPathError::CurrentDirectoryNotAbsolute(
            current_directory.to_path_buf(),
        ));
    }

    if let Some(explicit) = explicit {
        if explicit.as_os_str().is_empty() {
            return Err(ConfigurationPathError::EmptyExplicitPath);
        }
        return Ok(resolve_path(current_directory, explicit));
    }

    let app_data = app_data.ok_or(ConfigurationPathError::MissingAppData)?;
    if app_data.is_empty() {
        return Err(ConfigurationPathError::EmptyAppData);
    }
    let app_data = Path::new(app_data);
    if !app_data.is_absolute() {
        return Err(ConfigurationPathError::AppDataNotAbsolute(
            app_data.to_path_buf(),
        ));
    }
    Ok(app_data.join("ATT").join("config.toml"))
}

/// 读取并建立完整受信配置。
pub(crate) fn load_configuration(
    requested_path: &Path,
) -> Result<ApplicationConfiguration, ConfigurationLoadError> {
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
    let raw: RawConfiguration = toml::from_str(source).map_err(|error| {
        let location = error.span().map(|span| source_location(source, span.start));
        ConfigurationLoadError::InvalidToml {
            path: configuration_path.clone(),
            location,
        }
    })?;
    let configuration_directory = configuration_path
        .parent()
        .expect("规范绝对文件路径必须拥有父目录")
        .to_path_buf();
    ApplicationConfiguration::build(&configuration_directory, raw)
        .map_err(ConfigurationLoadError::InvalidValue)
}

/// 进程共享的完整受信配置。
pub(crate) struct ApplicationConfiguration {
    projects_root: PathBuf,
    runtime: RuntimeConfiguration,
    observability: ObservabilityConfiguration,
    llm_clients: LlmClientCatalogConfiguration,
    mz: MzConfiguration,
}

impl ApplicationConfiguration {
    fn build(
        configuration_directory: &Path,
        raw: RawConfiguration,
    ) -> Result<Self, ConfigurationValueError> {
        let projects_root =
            checked_path("projects.root", configuration_directory, raw.projects.root)?;
        let runtime = RuntimeConfiguration::build(configuration_directory, raw.runtime)?;
        let observability =
            ObservabilityConfiguration::build(configuration_directory, raw.observability)?;
        let llm_clients = LlmClientCatalogConfiguration::build(raw.llm.clients)?;
        let mz = MzConfiguration::build(configuration_directory, raw.mz, &llm_clients.client_ids)?;
        Ok(Self {
            projects_root,
            runtime,
            observability,
            llm_clients,
            mz,
        })
    }

    pub(crate) fn projects_root(&self) -> &Path {
        &self.projects_root
    }

    pub(crate) const fn runtime(&self) -> &RuntimeConfiguration {
        &self.runtime
    }

    pub(crate) const fn observability(&self) -> &ObservabilityConfiguration {
        &self.observability
    }

    pub(crate) const fn llm_clients(&self) -> &LlmClientCatalogConfiguration {
        &self.llm_clients
    }

    pub(crate) const fn mz(&self) -> &MzConfiguration {
        &self.mz
    }
}

pub(crate) struct RuntimeConfiguration {
    async_runtime: AsyncRuntimeConfiguration,
    cpu: CpuRuntimeConfiguration,
    filesystem: FilesystemRuntimeConfiguration,
    sqlite: SqliteRuntimeConfiguration,
    llm: LlmRuntimeConfiguration,
    lua: LuaRuntimeConfiguration,
}

impl RuntimeConfiguration {
    fn build(
        configuration_directory: &Path,
        raw: RawRuntimeConfiguration,
    ) -> Result<Self, ConfigurationValueError> {
        Ok(Self {
            async_runtime: AsyncRuntimeConfiguration::build(raw.async_runtime)?,
            cpu: CpuRuntimeConfiguration::build(raw.cpu)?,
            filesystem: FilesystemRuntimeConfiguration::build(raw.filesystem)?,
            sqlite: SqliteRuntimeConfiguration::build(raw.sqlite)?,
            llm: LlmRuntimeConfiguration::build(configuration_directory, raw.llm)?,
            lua: LuaRuntimeConfiguration::build(raw.lua)?,
        })
    }

    pub(crate) const fn async_runtime(&self) -> &AsyncRuntimeConfiguration {
        &self.async_runtime
    }

    pub(crate) const fn cpu(&self) -> &CpuRuntimeConfiguration {
        &self.cpu
    }

    pub(crate) const fn filesystem(&self) -> &FilesystemRuntimeConfiguration {
        &self.filesystem
    }

    pub(crate) const fn sqlite(&self) -> &SqliteRuntimeConfiguration {
        &self.sqlite
    }

    pub(crate) const fn llm(&self) -> &LlmRuntimeConfiguration {
        &self.llm
    }

    pub(crate) const fn lua(&self) -> &LuaRuntimeConfiguration {
        &self.lua
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

#[derive(Clone, Copy)]
pub(crate) struct CpuRuntimeConfiguration {
    worker_threads: NonZeroUsize,
    queue_capacity: NonZeroUsize,
}

impl CpuRuntimeConfiguration {
    fn build(raw: RawCpuRuntimeConfiguration) -> Result<Self, ConfigurationValueError> {
        Ok(Self {
            worker_threads: non_zero_usize("runtime.cpu.worker_threads", raw.worker_threads)?,
            queue_capacity: non_zero_usize("runtime.cpu.queue_capacity", raw.queue_capacity)?,
        })
    }

    pub(crate) const fn worker_threads(self) -> NonZeroUsize {
        self.worker_threads
    }

    pub(crate) const fn queue_capacity(self) -> NonZeroUsize {
        self.queue_capacity
    }
}

#[derive(Clone, Copy)]
pub(crate) struct DirectoryPublisherConfiguration {
    max_prepared_candidates: NonZeroUsize,
    max_candidate_entries: NonZeroUsize,
    max_candidate_depth: NonZeroUsize,
    max_candidate_bytes: NonZeroUsize,
    max_single_file_bytes: NonZeroUsize,
    max_recovery_artifacts_per_target: NonZeroUsize,
    target_lock_timeout: Duration,
}

impl DirectoryPublisherConfiguration {
    fn build(raw: RawDirectoryPublisherConfiguration) -> Result<Self, ConfigurationValueError> {
        let max_candidate_bytes = non_zero_usize(
            "runtime.filesystem.publisher.max_candidate_bytes",
            raw.max_candidate_bytes,
        )?;
        let max_single_file_bytes = non_zero_usize(
            "runtime.filesystem.publisher.max_single_file_bytes",
            raw.max_single_file_bytes,
        )?;
        if max_single_file_bytes > max_candidate_bytes {
            return Err(invalid(
                "runtime.filesystem.publisher.max_single_file_bytes",
                "单文件上限不得大于候选总字节上限",
            ));
        }
        Ok(Self {
            max_prepared_candidates: non_zero_usize(
                "runtime.filesystem.publisher.max_prepared_candidates",
                raw.max_prepared_candidates,
            )?,
            max_candidate_entries: non_zero_usize(
                "runtime.filesystem.publisher.max_candidate_entries",
                raw.max_candidate_entries,
            )?,
            max_candidate_depth: non_zero_usize(
                "runtime.filesystem.publisher.max_candidate_depth",
                raw.max_candidate_depth,
            )?,
            max_candidate_bytes,
            max_single_file_bytes,
            max_recovery_artifacts_per_target: non_zero_usize(
                "runtime.filesystem.publisher.max_recovery_artifacts_per_target",
                raw.max_recovery_artifacts_per_target,
            )?,
            target_lock_timeout: positive_duration(
                "runtime.filesystem.publisher.target_lock_timeout_ms",
                raw.target_lock_timeout_ms,
            )?,
        })
    }

    pub(crate) const fn max_prepared_candidates(self) -> NonZeroUsize {
        self.max_prepared_candidates
    }

    pub(crate) const fn max_candidate_entries(self) -> NonZeroUsize {
        self.max_candidate_entries
    }

    pub(crate) const fn max_candidate_depth(self) -> NonZeroUsize {
        self.max_candidate_depth
    }

    pub(crate) const fn max_candidate_bytes(self) -> NonZeroUsize {
        self.max_candidate_bytes
    }

    pub(crate) const fn max_single_file_bytes(self) -> NonZeroUsize {
        self.max_single_file_bytes
    }

    pub(crate) const fn max_recovery_artifacts_per_target(self) -> NonZeroUsize {
        self.max_recovery_artifacts_per_target
    }

    pub(crate) const fn target_lock_timeout(self) -> Duration {
        self.target_lock_timeout
    }
}

#[derive(Clone, Copy)]
pub(crate) struct FilesystemRuntimeConfiguration {
    worker_threads: NonZeroUsize,
    queue_capacity: NonZeroUsize,
    max_read_bytes: NonZeroUsize,
    max_directory_entries: NonZeroUsize,
    publisher: DirectoryPublisherConfiguration,
}

impl FilesystemRuntimeConfiguration {
    fn build(raw: RawFilesystemRuntimeConfiguration) -> Result<Self, ConfigurationValueError> {
        Ok(Self {
            worker_threads: non_zero_usize(
                "runtime.filesystem.worker_threads",
                raw.worker_threads,
            )?,
            queue_capacity: non_zero_usize(
                "runtime.filesystem.queue_capacity",
                raw.queue_capacity,
            )?,
            max_read_bytes: non_zero_usize(
                "runtime.filesystem.max_read_bytes",
                raw.max_read_bytes,
            )?,
            max_directory_entries: non_zero_usize(
                "runtime.filesystem.max_directory_entries",
                raw.max_directory_entries,
            )?,
            publisher: DirectoryPublisherConfiguration::build(raw.publisher)?,
        })
    }

    pub(crate) const fn worker_threads(self) -> NonZeroUsize {
        self.worker_threads
    }

    pub(crate) const fn queue_capacity(self) -> NonZeroUsize {
        self.queue_capacity
    }

    pub(crate) const fn max_read_bytes(self) -> NonZeroUsize {
        self.max_read_bytes
    }

    pub(crate) const fn max_directory_entries(self) -> NonZeroUsize {
        self.max_directory_entries
    }

    pub(crate) const fn publisher(self) -> DirectoryPublisherConfiguration {
        self.publisher
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SqliteJournalMode {
    Delete,
    Truncate,
    Persist,
    Wal,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SqliteSynchronous {
    Normal,
    Full,
    Extra,
}

#[derive(Clone, Copy)]
pub(crate) struct SqliteRuntimeConfiguration {
    short_worker_threads: NonZeroUsize,
    short_queue_capacity: NonZeroUsize,
    max_open_connections: NonZeroUsize,
    max_interactive_sessions: NonZeroUsize,
    interactive_open_queue_capacity: NonZeroUsize,
    interactive_command_queue_capacity: NonZeroUsize,
    worker_stack_bytes: NonZeroUsize,
    max_statement_bytes: NonZeroUsize,
    max_parameter_bytes: NonZeroUsize,
    max_rows_per_query: NonZeroUsize,
    max_result_bytes_per_query: NonZeroUsize,
    busy_timeout: Duration,
    journal_mode: SqliteJournalMode,
    synchronous: SqliteSynchronous,
}

impl SqliteRuntimeConfiguration {
    fn build(raw: RawSqliteRuntimeConfiguration) -> Result<Self, ConfigurationValueError> {
        let max_open_connections = non_zero_usize(
            "runtime.sqlite.max_open_connections",
            raw.max_open_connections,
        )?;
        let max_interactive_sessions = non_zero_usize(
            "runtime.sqlite.max_interactive_sessions",
            raw.max_interactive_sessions,
        )?;
        if max_interactive_sessions > max_open_connections {
            return Err(invalid(
                "runtime.sqlite.max_interactive_sessions",
                "交互会话上限不得大于连接总上限",
            ));
        }
        Ok(Self {
            short_worker_threads: non_zero_usize(
                "runtime.sqlite.short_worker_threads",
                raw.short_worker_threads,
            )?,
            short_queue_capacity: non_zero_usize(
                "runtime.sqlite.short_queue_capacity",
                raw.short_queue_capacity,
            )?,
            max_open_connections,
            max_interactive_sessions,
            interactive_open_queue_capacity: non_zero_usize(
                "runtime.sqlite.interactive_open_queue_capacity",
                raw.interactive_open_queue_capacity,
            )?,
            interactive_command_queue_capacity: non_zero_usize(
                "runtime.sqlite.interactive_command_queue_capacity",
                raw.interactive_command_queue_capacity,
            )?,
            worker_stack_bytes: non_zero_usize(
                "runtime.sqlite.worker_stack_bytes",
                raw.worker_stack_bytes,
            )?,
            max_statement_bytes: non_zero_usize(
                "runtime.sqlite.max_statement_bytes",
                raw.max_statement_bytes,
            )?,
            max_parameter_bytes: non_zero_usize(
                "runtime.sqlite.max_parameter_bytes",
                raw.max_parameter_bytes,
            )?,
            max_rows_per_query: non_zero_usize(
                "runtime.sqlite.max_rows_per_query",
                raw.max_rows_per_query,
            )?,
            max_result_bytes_per_query: non_zero_usize(
                "runtime.sqlite.max_result_bytes_per_query",
                raw.max_result_bytes_per_query,
            )?,
            busy_timeout: positive_duration("runtime.sqlite.busy_timeout_ms", raw.busy_timeout_ms)?,
            journal_mode: raw.journal_mode,
            synchronous: raw.synchronous,
        })
    }

    pub(crate) const fn short_worker_threads(self) -> NonZeroUsize {
        self.short_worker_threads
    }

    pub(crate) const fn short_queue_capacity(self) -> NonZeroUsize {
        self.short_queue_capacity
    }

    pub(crate) const fn max_open_connections(self) -> NonZeroUsize {
        self.max_open_connections
    }

    pub(crate) const fn max_interactive_sessions(self) -> NonZeroUsize {
        self.max_interactive_sessions
    }

    pub(crate) const fn interactive_open_queue_capacity(self) -> NonZeroUsize {
        self.interactive_open_queue_capacity
    }

    pub(crate) const fn interactive_command_queue_capacity(self) -> NonZeroUsize {
        self.interactive_command_queue_capacity
    }

    pub(crate) const fn worker_stack_bytes(self) -> NonZeroUsize {
        self.worker_stack_bytes
    }

    pub(crate) const fn max_statement_bytes(self) -> NonZeroUsize {
        self.max_statement_bytes
    }

    pub(crate) const fn max_parameter_bytes(self) -> NonZeroUsize {
        self.max_parameter_bytes
    }

    pub(crate) const fn max_rows_per_query(self) -> NonZeroUsize {
        self.max_rows_per_query
    }

    pub(crate) const fn max_result_bytes_per_query(self) -> NonZeroUsize {
        self.max_result_bytes_per_query
    }

    pub(crate) const fn busy_timeout(self) -> Duration {
        self.busy_timeout
    }

    pub(crate) const fn journal_mode(self) -> SqliteJournalMode {
        self.journal_mode
    }

    pub(crate) const fn synchronous(self) -> SqliteSynchronous {
        self.synchronous
    }
}

#[derive(Clone)]
pub(crate) enum ProxyConfiguration {
    Disabled,
    Url(Url),
}

#[derive(Clone)]
pub(crate) struct LlmTlsConfiguration {
    additional_pem_files: Vec<PathBuf>,
}

impl LlmTlsConfiguration {
    pub(crate) fn additional_pem_files(&self) -> &[PathBuf] {
        &self.additional_pem_files
    }
}

#[derive(Clone)]
pub(crate) struct LlmRuntimeConfiguration {
    max_active_requests: NonZeroUsize,
    queue_capacity: usize,
    admission_timeout: Duration,
    connect_timeout: Duration,
    read_timeout: Duration,
    pool_idle_timeout: Duration,
    pool_max_idle_per_host: usize,
    proxy: ProxyConfiguration,
    tls: LlmTlsConfiguration,
}

impl LlmRuntimeConfiguration {
    fn build(
        configuration_directory: &Path,
        raw: RawLlmRuntimeConfiguration,
    ) -> Result<Self, ConfigurationValueError> {
        let proxy = match raw.proxy {
            RawProxyConfiguration::Disabled(value) if !value => ProxyConfiguration::Disabled,
            RawProxyConfiguration::Disabled(_) => {
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
                ProxyConfiguration::Url(url)
            }
        };

        let mut additional_pem_files = Vec::with_capacity(raw.tls.additional_pem_files.len());
        let mut seen_pem_files = BTreeSet::new();
        for (index, path) in raw.tls.additional_pem_files.into_iter().enumerate() {
            let path = checked_path(
                format!("runtime.llm.tls.additional_pem_files[{index}]").as_str(),
                configuration_directory,
                path,
            )?;
            if !seen_pem_files.insert(path.clone()) {
                return Err(invalid(
                    format!("runtime.llm.tls.additional_pem_files[{index}]").as_str(),
                    "PEM 路径重复",
                ));
            }
            additional_pem_files.push(path);
        }

        Ok(Self {
            max_active_requests: non_zero_usize(
                "runtime.llm.max_active_requests",
                raw.max_active_requests,
            )?,
            queue_capacity: usize_value("runtime.llm.queue_capacity", raw.queue_capacity)?,
            admission_timeout: positive_duration(
                "runtime.llm.admission_timeout_ms",
                raw.admission_timeout_ms,
            )?,
            connect_timeout: positive_duration(
                "runtime.llm.connect_timeout_ms",
                raw.connect_timeout_ms,
            )?,
            read_timeout: positive_duration("runtime.llm.read_timeout_ms", raw.read_timeout_ms)?,
            pool_idle_timeout: positive_duration(
                "runtime.llm.pool_idle_timeout_ms",
                raw.pool_idle_timeout_ms,
            )?,
            pool_max_idle_per_host: usize_value(
                "runtime.llm.pool_max_idle_per_host",
                raw.pool_max_idle_per_host,
            )?,
            proxy,
            tls: LlmTlsConfiguration {
                additional_pem_files,
            },
        })
    }

    pub(crate) const fn max_active_requests(&self) -> NonZeroUsize {
        self.max_active_requests
    }

    pub(crate) const fn queue_capacity(&self) -> usize {
        self.queue_capacity
    }

    pub(crate) const fn admission_timeout(&self) -> Duration {
        self.admission_timeout
    }

    pub(crate) const fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    pub(crate) const fn read_timeout(&self) -> Duration {
        self.read_timeout
    }

    pub(crate) const fn pool_idle_timeout(&self) -> Duration {
        self.pool_idle_timeout
    }

    pub(crate) const fn pool_max_idle_per_host(&self) -> usize {
        self.pool_max_idle_per_host
    }

    pub(crate) const fn proxy(&self) -> &ProxyConfiguration {
        &self.proxy
    }

    pub(crate) const fn tls(&self) -> &LlmTlsConfiguration {
        &self.tls
    }
}

#[derive(Clone, Copy)]
pub(crate) struct LuaRuntimeConfiguration {
    worker_threads: NonZeroUsize,
    queue_capacity: NonZeroUsize,
    worker_stack_bytes: NonZeroUsize,
    memory_limit_bytes_per_vm: NonZeroUsize,
    cancel_check_instruction_interval: NonZeroU32,
    max_error_bytes: NonZeroUsize,
}

impl LuaRuntimeConfiguration {
    fn build(raw: RawLuaRuntimeConfiguration) -> Result<Self, ConfigurationValueError> {
        Ok(Self {
            worker_threads: non_zero_usize("runtime.lua.worker_threads", raw.worker_threads)?,
            queue_capacity: non_zero_usize("runtime.lua.queue_capacity", raw.queue_capacity)?,
            worker_stack_bytes: non_zero_usize(
                "runtime.lua.worker_stack_bytes",
                raw.worker_stack_bytes,
            )?,
            memory_limit_bytes_per_vm: non_zero_usize(
                "runtime.lua.memory_limit_bytes_per_vm",
                raw.memory_limit_bytes_per_vm,
            )?,
            cancel_check_instruction_interval: non_zero_u32(
                "runtime.lua.cancel_check_instruction_interval",
                raw.cancel_check_instruction_interval,
            )?,
            max_error_bytes: non_zero_usize("runtime.lua.max_error_bytes", raw.max_error_bytes)?,
        })
    }

    pub(crate) const fn worker_threads(self) -> NonZeroUsize {
        self.worker_threads
    }

    pub(crate) const fn queue_capacity(self) -> NonZeroUsize {
        self.queue_capacity
    }

    pub(crate) const fn worker_stack_bytes(self) -> NonZeroUsize {
        self.worker_stack_bytes
    }

    pub(crate) const fn memory_limit_bytes_per_vm(self) -> NonZeroUsize {
        self.memory_limit_bytes_per_vm
    }

    pub(crate) const fn cancel_check_instruction_interval(self) -> NonZeroU32 {
        self.cancel_check_instruction_interval
    }

    pub(crate) const fn max_error_bytes(self) -> NonZeroUsize {
        self.max_error_bytes
    }
}

#[derive(Clone, Copy)]
pub(crate) struct EventLogConfiguration {
    queue_capacity: NonZeroUsize,
    lock_timeout: Duration,
    max_record_bytes: NonZeroUsize,
    max_file_bytes: NonZeroUsize,
    retained_rotated_files: usize,
}

impl EventLogConfiguration {
    fn build(
        field_prefix: &str,
        raw: RawEventLogConfiguration,
    ) -> Result<Self, ConfigurationValueError> {
        let max_record_bytes = non_zero_usize(
            format!("{field_prefix}.max_record_bytes").as_str(),
            raw.max_record_bytes,
        )?;
        let max_file_bytes = non_zero_usize(
            format!("{field_prefix}.max_file_bytes").as_str(),
            raw.max_file_bytes,
        )?;
        if max_record_bytes > max_file_bytes {
            return Err(invalid(
                format!("{field_prefix}.max_record_bytes").as_str(),
                "单条记录上限不得大于活动文件上限",
            ));
        }
        Ok(Self {
            queue_capacity: non_zero_usize(
                format!("{field_prefix}.queue_capacity").as_str(),
                raw.queue_capacity,
            )?,
            lock_timeout: positive_duration(
                format!("{field_prefix}.lock_timeout_ms").as_str(),
                raw.lock_timeout_ms,
            )?,
            max_record_bytes,
            max_file_bytes,
            retained_rotated_files: usize_value(
                format!("{field_prefix}.retained_rotated_files").as_str(),
                raw.retained_rotated_files,
            )?,
        })
    }

    pub(crate) const fn queue_capacity(self) -> NonZeroUsize {
        self.queue_capacity
    }

    pub(crate) const fn lock_timeout(self) -> Duration {
        self.lock_timeout
    }

    pub(crate) const fn max_record_bytes(self) -> NonZeroUsize {
        self.max_record_bytes
    }

    pub(crate) const fn max_file_bytes(self) -> NonZeroUsize {
        self.max_file_bytes
    }

    pub(crate) const fn retained_rotated_files(self) -> usize {
        self.retained_rotated_files
    }
}

pub(crate) struct ObservabilityConfiguration {
    root: PathBuf,
    translation: EventLogConfiguration,
    write_back: EventLogConfiguration,
}

impl ObservabilityConfiguration {
    fn build(
        configuration_directory: &Path,
        raw: RawObservabilityConfiguration,
    ) -> Result<Self, ConfigurationValueError> {
        Ok(Self {
            root: checked_path("observability.root", configuration_directory, raw.root)?,
            translation: EventLogConfiguration::build(
                "observability.translation",
                raw.translation,
            )?,
            write_back: EventLogConfiguration::build("observability.write_back", raw.write_back)?,
        })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) const fn translation(&self) -> EventLogConfiguration {
        self.translation
    }

    pub(crate) const fn write_back(&self) -> EventLogConfiguration {
        self.write_back
    }
}

pub(crate) struct MzConfiguration {
    document: MzDocumentConfiguration,
    standard_asset: MzStandardAssetConfiguration,
    extract_builtin: MzScanConfiguration,
    extract_rules: MzScanConfiguration,
    extract_store: MzExtractionStoreConfiguration,
    translate_store: MzTranslationStoreConfiguration,
    language_modules: LanguageModuleCatalog,
    translation_profiles: BTreeMap<String, TranslationProfileConfiguration>,
}

impl MzConfiguration {
    fn build(
        configuration_directory: &Path,
        raw: RawMzConfiguration,
        llm_client_ids: &BTreeSet<LlmClientId>,
    ) -> Result<Self, ConfigurationValueError> {
        let (language_modules, language_ids) = build_language_modules(raw.languages)?;
        let translation_profiles = build_translation_profiles(
            configuration_directory,
            raw.translation_profiles,
            &language_ids,
            llm_client_ids,
        )?;
        Ok(Self {
            document: MzDocumentConfiguration {
                read_concurrency: non_zero_usize(
                    "mz.document.read_concurrency",
                    raw.document.read_concurrency,
                )?,
                parse_concurrency: non_zero_usize(
                    "mz.document.parse_concurrency",
                    raw.document.parse_concurrency,
                )?,
            },
            standard_asset: MzStandardAssetConfiguration {
                decode_concurrency: non_zero_usize(
                    "mz.standard_asset.decode_concurrency",
                    raw.standard_asset.decode_concurrency,
                )?,
                leaves_per_decode_job: non_zero_usize(
                    "mz.standard_asset.leaves_per_decode_job",
                    raw.standard_asset.leaves_per_decode_job,
                )?,
            },
            extract_builtin: MzScanConfiguration {
                scan_concurrency: non_zero_usize(
                    "mz.extract.builtin.scan_concurrency",
                    raw.extract.builtin.scan_concurrency,
                )?,
            },
            extract_rules: MzScanConfiguration {
                scan_concurrency: non_zero_usize(
                    "mz.extract.rules.scan_concurrency",
                    raw.extract.rules.scan_concurrency,
                )?,
            },
            extract_store: MzExtractionStoreConfiguration {
                encode_concurrency: non_zero_usize(
                    "mz.extract.store.encode_concurrency",
                    raw.extract.store.encode_concurrency,
                )?,
                groups_per_encode_job: non_zero_usize(
                    "mz.extract.store.groups_per_encode_job",
                    raw.extract.store.groups_per_encode_job,
                )?,
            },
            translate_store: MzTranslationStoreConfiguration {
                encode_concurrency: non_zero_usize(
                    "mz.translate.store.encode_concurrency",
                    raw.translate.store.encode_concurrency,
                )?,
                leaves_per_encode_job: non_zero_usize(
                    "mz.translate.store.leaves_per_encode_job",
                    raw.translate.store.leaves_per_encode_job,
                )?,
            },
            language_modules,
            translation_profiles,
        })
    }

    pub(crate) const fn document(&self) -> MzDocumentConfiguration {
        self.document
    }

    pub(crate) const fn standard_asset(&self) -> MzStandardAssetConfiguration {
        self.standard_asset
    }

    pub(crate) const fn extract_builtin(&self) -> MzScanConfiguration {
        self.extract_builtin
    }

    pub(crate) const fn extract_rules(&self) -> MzScanConfiguration {
        self.extract_rules
    }

    pub(crate) const fn extract_store(&self) -> MzExtractionStoreConfiguration {
        self.extract_store
    }

    pub(crate) const fn translate_store(&self) -> MzTranslationStoreConfiguration {
        self.translate_store
    }

    pub(crate) fn language_modules(&self) -> LanguageModuleCatalog {
        self.language_modules.clone()
    }

    pub(crate) fn translation_profile(&self, id: &str) -> Option<&TranslationProfileConfiguration> {
        self.translation_profiles.get(id)
    }

    pub(crate) fn translation_profile_ids(&self) -> impl Iterator<Item = &str> {
        self.translation_profiles.keys().map(String::as_str)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct MzDocumentConfiguration {
    read_concurrency: NonZeroUsize,
    parse_concurrency: NonZeroUsize,
}

impl MzDocumentConfiguration {
    pub(crate) const fn read_concurrency(self) -> NonZeroUsize {
        self.read_concurrency
    }

    pub(crate) const fn parse_concurrency(self) -> NonZeroUsize {
        self.parse_concurrency
    }
}

#[derive(Clone, Copy)]
pub(crate) struct MzStandardAssetConfiguration {
    decode_concurrency: NonZeroUsize,
    leaves_per_decode_job: NonZeroUsize,
}

impl MzStandardAssetConfiguration {
    pub(crate) const fn decode_concurrency(self) -> NonZeroUsize {
        self.decode_concurrency
    }

    pub(crate) const fn leaves_per_decode_job(self) -> NonZeroUsize {
        self.leaves_per_decode_job
    }
}

#[derive(Clone, Copy)]
pub(crate) struct MzScanConfiguration {
    scan_concurrency: NonZeroUsize,
}

impl MzScanConfiguration {
    pub(crate) const fn scan_concurrency(self) -> NonZeroUsize {
        self.scan_concurrency
    }
}

#[derive(Clone, Copy)]
pub(crate) struct MzExtractionStoreConfiguration {
    encode_concurrency: NonZeroUsize,
    groups_per_encode_job: NonZeroUsize,
}

impl MzExtractionStoreConfiguration {
    pub(crate) const fn encode_concurrency(self) -> NonZeroUsize {
        self.encode_concurrency
    }

    pub(crate) const fn groups_per_encode_job(self) -> NonZeroUsize {
        self.groups_per_encode_job
    }
}

#[derive(Clone, Copy)]
pub(crate) struct MzTranslationStoreConfiguration {
    encode_concurrency: NonZeroUsize,
    leaves_per_encode_job: NonZeroUsize,
}

impl MzTranslationStoreConfiguration {
    pub(crate) const fn encode_concurrency(self) -> NonZeroUsize {
        self.encode_concurrency
    }

    pub(crate) const fn leaves_per_encode_job(self) -> NonZeroUsize {
        self.leaves_per_encode_job
    }
}

#[derive(Clone)]
pub(crate) struct TranslationSystemConfiguration {
    source_language: String,
    target_language: String,
    markdown_path: PathBuf,
}

impl TranslationSystemConfiguration {
    pub(crate) fn source_language(&self) -> &str {
        &self.source_language
    }

    pub(crate) fn target_language(&self) -> &str {
        &self.target_language
    }

    pub(crate) fn markdown_path(&self) -> &Path {
        &self.markdown_path
    }
}

#[derive(Clone)]
pub(crate) struct TranslationPlanningConfiguration {
    scope_concurrency: NonZeroUsize,
    max_message_characters: NonZeroUsize,
    systems: Vec<TranslationSystemConfiguration>,
}

impl TranslationPlanningConfiguration {
    pub(crate) const fn scope_concurrency(&self) -> NonZeroUsize {
        self.scope_concurrency
    }

    pub(crate) const fn max_message_characters(&self) -> NonZeroUsize {
        self.max_message_characters
    }

    pub(crate) fn systems(&self) -> &[TranslationSystemConfiguration] {
        &self.systems
    }
}

#[derive(Clone)]
pub(crate) struct TranslationExecutionConfiguration {
    network_retry_delays: Vec<Duration>,
    max_network_retry_after: Duration,
}

impl TranslationExecutionConfiguration {
    pub(crate) fn network_retry_delays(&self) -> &[Duration] {
        &self.network_retry_delays
    }

    pub(crate) const fn max_network_retry_after(&self) -> Duration {
        self.max_network_retry_after
    }
}

#[derive(Clone)]
pub(crate) enum LlmAuthConfiguration {
    None,
    Bearer(SecretString),
}

impl fmt::Debug for LlmAuthConfiguration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => formatter.write_str("None"),
            Self::Bearer(_) => formatter.write_str("Bearer([REDACTED])"),
        }
    }
}

pub(crate) struct LlmClientConfiguration {
    id: LlmClientId,
    endpoint: Url,
    auth: LlmAuthConfiguration,
    model: String,
    request_timeout: Duration,
    max_request_bytes: NonZeroUsize,
    max_response_bytes: NonZeroUsize,
    max_error_response_bytes: NonZeroUsize,
    requests_per_minute: NonZeroU32,
    burst_requests: NonZeroU32,
    request_body_extra: JsonMap<String, JsonValue>,
}

/// 配置边界已经校验并确认存在的公共 LLM Client 身份。
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct LlmClientId(String);

impl LlmClientId {
    fn from_validated(value: String) -> Self {
        Self(value)
    }

    #[cfg(test)]
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl LlmClientConfiguration {
    pub(crate) const fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    pub(crate) const fn auth(&self) -> &LlmAuthConfiguration {
        &self.auth
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    pub(crate) const fn request_timeout(&self) -> Duration {
        self.request_timeout
    }

    pub(crate) const fn max_request_bytes(&self) -> NonZeroUsize {
        self.max_request_bytes
    }

    pub(crate) const fn max_response_bytes(&self) -> NonZeroUsize {
        self.max_response_bytes
    }

    pub(crate) const fn max_error_response_bytes(&self) -> NonZeroUsize {
        self.max_error_response_bytes
    }

    pub(crate) const fn requests_per_minute(&self) -> NonZeroU32 {
        self.requests_per_minute
    }

    pub(crate) const fn burst_requests(&self) -> NonZeroU32 {
        self.burst_requests
    }

    pub(crate) fn request_body_extra(&self) -> &JsonMap<String, JsonValue> {
        &self.request_body_extra
    }
}

impl fmt::Debug for LlmClientConfiguration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LlmClientConfiguration")
            .field("id", &self.id)
            .field("auth", &self.auth)
            .field("model", &self.model)
            .field("request_timeout", &self.request_timeout)
            .field("max_request_bytes", &self.max_request_bytes)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("max_error_response_bytes", &self.max_error_response_bytes)
            .field("requests_per_minute", &self.requests_per_minute)
            .field("burst_requests", &self.burst_requests)
            .field(
                "request_body_extra_keys",
                &self.request_body_extra.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

pub(crate) struct LlmClientCatalogConfiguration {
    clients: BTreeMap<LlmClientId, LlmClientConfiguration>,
    client_ids: BTreeSet<LlmClientId>,
}

impl LlmClientCatalogConfiguration {
    fn build(raw_clients: Vec<RawLlmClientConfiguration>) -> Result<Self, ConfigurationValueError> {
        if raw_clients.is_empty() {
            return Err(invalid("llm.clients", "至少需要一个 LLM 客户端"));
        }

        let mut clients = BTreeMap::new();
        let mut client_ids = BTreeSet::new();
        for (index, raw) in raw_clients.into_iter().enumerate() {
            let field = format!("llm.clients[{index}]");
            validate_exact_identifier(format!("{field}.id").as_str(), &raw.id)?;
            let id = LlmClientId::from_validated(raw.id.clone());
            if !client_ids.insert(id.clone()) {
                return Err(invalid(
                    format!("{field}.id").as_str(),
                    "LLM 客户端 ID 重复",
                ));
            }
            let client = build_llm_client(field.as_str(), raw)?;
            clients.insert(id, client);
        }

        Ok(Self {
            clients,
            client_ids,
        })
    }

    pub(crate) fn get(&self, id: &LlmClientId) -> Option<&LlmClientConfiguration> {
        self.clients.get(id)
    }
}

#[derive(Clone)]
pub(crate) struct TranslationProfileConfiguration {
    id: String,
    max_in_flight_tasks: NonZeroUsize,
    planning: TranslationPlanningConfiguration,
    execution: TranslationExecutionConfiguration,
    llm_client_id: LlmClientId,
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

    pub(crate) const fn execution(&self) -> &TranslationExecutionConfiguration {
        &self.execution
    }

    pub(crate) const fn llm_client_id(&self) -> &LlmClientId {
        &self.llm_client_id
    }
}

fn build_language_modules(
    raw_languages: Vec<RawLanguageConfiguration>,
) -> Result<(LanguageModuleCatalog, BTreeSet<String>), ConfigurationValueError> {
    let mut bindings = Vec::<(String, Arc<dyn LanguageModule>)>::new();
    let mut language_ids = BTreeSet::new();
    for (index, raw) in raw_languages.into_iter().enumerate() {
        let field = format!("mz.languages[{index}]");
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
        if !language_ids.insert(id.clone()) {
            return Err(invalid(field.as_str(), format!("源语言 ID 重复：{id}")));
        }
        bindings.push((id, module));
    }

    let catalog = LanguageModuleCatalog::new(bindings)
        .map_err(|source| invalid("mz.languages", source.to_string()))?;
    Ok((catalog, language_ids))
}

fn build_translation_profiles(
    configuration_directory: &Path,
    raw_profiles: Vec<RawTranslationProfileConfiguration>,
    language_ids: &BTreeSet<String>,
    llm_client_ids: &BTreeSet<LlmClientId>,
) -> Result<BTreeMap<String, TranslationProfileConfiguration>, ConfigurationValueError> {
    if raw_profiles.is_empty() {
        return Err(invalid(
            "mz.translation_profiles",
            "至少需要一个翻译 Profile",
        ));
    }

    let mut profiles = BTreeMap::new();
    for (index, raw) in raw_profiles.into_iter().enumerate() {
        let field = format!("mz.translation_profiles[{index}]");
        if raw.id.trim().is_empty() {
            return Err(invalid(format!("{field}.id").as_str(), "ID 不能为空白"));
        }
        if profiles.contains_key(&raw.id) {
            return Err(invalid(
                format!("{field}.id").as_str(),
                format!("翻译 Profile ID 重复：{}", raw.id),
            ));
        }

        let planning = build_translation_planning(
            configuration_directory,
            format!("{field}.planning").as_str(),
            raw.planning,
            language_ids,
        )?;
        let execution = TranslationExecutionConfiguration {
            network_retry_delays: raw
                .execution
                .network_retry_delays_ms
                .into_iter()
                .map(Duration::from_millis)
                .collect(),
            max_network_retry_after: Duration::from_millis(
                raw.execution.max_network_retry_after_ms,
            ),
        };
        validate_exact_identifier(format!("{field}.llm_client").as_str(), &raw.llm_client)?;
        let llm_client_id = LlmClientId::from_validated(raw.llm_client);
        if !llm_client_ids.contains(&llm_client_id) {
            return Err(invalid(
                format!("{field}.llm_client").as_str(),
                "没有同 ID 的公共 LLM 客户端",
            ));
        }
        let profile = TranslationProfileConfiguration {
            id: raw.id.clone(),
            max_in_flight_tasks: non_zero_usize(
                format!("{field}.max_in_flight_tasks").as_str(),
                raw.max_in_flight_tasks,
            )?,
            planning,
            execution,
            llm_client_id,
        };
        profiles.insert(raw.id, profile);
    }
    Ok(profiles)
}

fn build_translation_planning(
    configuration_directory: &Path,
    field: &str,
    raw: RawTranslationPlanningConfiguration,
    language_ids: &BTreeSet<String>,
) -> Result<TranslationPlanningConfiguration, ConfigurationValueError> {
    if raw.systems.is_empty() {
        return Err(invalid(
            format!("{field}.systems").as_str(),
            "系统提示词列表不能为空",
        ));
    }
    let mut seen_pairs = BTreeSet::new();
    let mut systems = Vec::with_capacity(raw.systems.len());
    for (index, system) in raw.systems.into_iter().enumerate() {
        let system_field = format!("{field}.systems[{index}]");
        validate_exact_identifier(
            format!("{system_field}.source_language").as_str(),
            &system.source_language,
        )?;
        validate_exact_identifier(
            format!("{system_field}.target_language").as_str(),
            &system.target_language,
        )?;
        if !language_ids.contains(&system.source_language) {
            return Err(invalid(
                format!("{system_field}.source_language").as_str(),
                format!("没有同 ID 的源语言模块：{}", system.source_language),
            ));
        }
        let pair = (
            system.source_language.clone(),
            system.target_language.clone(),
        );
        if !seen_pairs.insert(pair) {
            return Err(invalid(system_field.as_str(), "系统提示词语言对重复"));
        }
        systems.push(TranslationSystemConfiguration {
            source_language: system.source_language,
            target_language: system.target_language,
            markdown_path: checked_path(
                format!("{system_field}.path").as_str(),
                configuration_directory,
                system.path,
            )?,
        });
    }

    Ok(TranslationPlanningConfiguration {
        scope_concurrency: non_zero_usize(
            format!("{field}.scope_concurrency").as_str(),
            raw.scope_concurrency,
        )?,
        max_message_characters: non_zero_usize(
            format!("{field}.max_message_characters").as_str(),
            raw.max_message_characters,
        )?,
        systems,
    })
}

fn build_llm_client(
    field: &str,
    raw: RawLlmClientConfiguration,
) -> Result<LlmClientConfiguration, ConfigurationValueError> {
    let endpoint = Url::parse(&raw.endpoint)
        .map_err(|_| invalid(format!("{field}.endpoint").as_str(), "endpoint URL 无效"))?;
    validate_endpoint(
        format!("{field}.endpoint").as_str(),
        &endpoint,
        raw.allow_plain_http_loopback,
    )?;
    validate_exact_identifier(format!("{field}.model").as_str(), &raw.model)?;

    let auth = match raw.auth {
        RawLlmAuthConfiguration::Name(value) if value == "none" => LlmAuthConfiguration::None,
        RawLlmAuthConfiguration::Name(_) => {
            return Err(invalid(
                format!("{field}.auth").as_str(),
                "字符串形式只接受 none",
            ));
        }
        RawLlmAuthConfiguration::Bearer(RawBearerConfiguration { bearer }) => {
            let exposed = bearer.expose_secret();
            if exposed.trim().is_empty() {
                return Err(invalid(
                    format!("{field}.auth.bearer").as_str(),
                    "Bearer 密钥不能为空白",
                ));
            }
            if exposed.trim() != exposed {
                return Err(invalid(
                    format!("{field}.auth.bearer").as_str(),
                    "Bearer 密钥不能包含首尾空白",
                ));
            }
            if reqwest::header::HeaderValue::from_bytes(exposed.as_bytes()).is_err() {
                return Err(invalid(
                    format!("{field}.auth.bearer").as_str(),
                    "Bearer 密钥不能安全写入 HTTP Header",
                ));
            }
            LlmAuthConfiguration::Bearer(bearer)
        }
    };

    let request_body_value = serde_json::from_str::<StrictJsonValue>(&raw.request_body_extra)
        .map_err(|error| {
            invalid(
                format!("{field}.request_body_extra").as_str(),
                format!(
                    "不是有效的严格 JSON（第 {} 行，第 {} 列）",
                    error.line(),
                    error.column()
                ),
            )
        })?
        .0;
    let JsonValue::Object(request_body_extra) = request_body_value else {
        return Err(invalid(
            format!("{field}.request_body_extra").as_str(),
            "必须是 JSON 对象",
        ));
    };
    for reserved in RESERVED_REQUEST_BODY_FIELDS {
        if request_body_extra.contains_key(reserved) {
            return Err(invalid(
                format!("{field}.request_body_extra.{reserved}").as_str(),
                "该顶层字段由请求协议固定拥有，不能通过 request_body_extra 覆盖",
            ));
        }
    }

    Ok(LlmClientConfiguration {
        id: LlmClientId::from_validated(raw.id),
        endpoint,
        auth,
        model: raw.model,
        request_timeout: positive_duration(
            format!("{field}.request_timeout_ms").as_str(),
            raw.request_timeout_ms,
        )?,
        max_request_bytes: non_zero_usize(
            format!("{field}.max_request_bytes").as_str(),
            raw.max_request_bytes,
        )?,
        max_response_bytes: non_zero_usize(
            format!("{field}.max_response_bytes").as_str(),
            raw.max_response_bytes,
        )?,
        max_error_response_bytes: non_zero_usize(
            format!("{field}.max_error_response_bytes").as_str(),
            raw.max_error_response_bytes,
        )?,
        requests_per_minute: non_zero_u32(
            format!("{field}.requests_per_minute").as_str(),
            raw.requests_per_minute,
        )?,
        burst_requests: non_zero_u32(
            format!("{field}.burst_requests").as_str(),
            raw.burst_requests,
        )?,
        request_body_extra,
    })
}

fn validate_endpoint(
    field: &str,
    endpoint: &Url,
    allow_plain_http_loopback: bool,
) -> Result<(), ConfigurationValueError> {
    if endpoint.username() != "" || endpoint.password().is_some() {
        return Err(invalid(field, "endpoint 不得内嵌凭据"));
    }
    if endpoint.fragment().is_some() {
        return Err(invalid(field, "endpoint 不得包含 URL fragment"));
    }
    match endpoint.scheme() {
        "https" => Ok(()),
        "http" if allow_plain_http_loopback && is_loopback(endpoint.host()) => Ok(()),
        "http" => Err(invalid(
            field,
            "HTTP endpoint 只允许在显式开启后指向 loopback",
        )),
        _ => Err(invalid(field, "endpoint 只接受 https 或 loopback http")),
    }
}

fn is_loopback(host: Option<Host<&str>>) -> bool {
    match host {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
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
    MissingAppData,
    EmptyAppData,
    AppDataNotAbsolute(PathBuf),
}

impl fmt::Display for ConfigurationPathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentDirectoryNotAbsolute(path) => {
                write!(formatter, "当前工作目录不是绝对路径：{}", path.display())
            }
            Self::EmptyExplicitPath => formatter.write_str("--config 路径不能为空"),
            Self::MissingAppData => formatter.write_str("未设置 APPDATA，无法定位默认配置文件"),
            Self::EmptyAppData => formatter.write_str("APPDATA 为空，无法定位默认配置文件"),
            Self::AppDataNotAbsolute(path) => {
                write!(formatter, "APPDATA 不是绝对路径：{}", path.display())
            }
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
    },
    InvalidValue(ConfigurationValueError),
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
            Self::InvalidToml { path, location } => {
                write!(formatter, "配置文件 {} 不是有效 TOML", path.display())?;
                if let Some(location) = location {
                    write!(
                        formatter,
                        "（第 {} 行，第 {} 列）",
                        location.line, location.column
                    )?;
                }
                formatter.write_str("：语法、结构或字段类型不符合当前配置契约")
            }
            Self::InvalidValue(source) => write!(formatter, "配置值无效：{source}"),
        }
    }
}

impl Error for ConfigurationLoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Open { source, .. } | Self::Read { source, .. } => Some(source),
            Self::InvalidValue(source) => Some(source),
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

impl ConfigurationValueError {
    #[cfg(test)]
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfiguration {
    projects: RawProjectsConfiguration,
    runtime: RawRuntimeConfiguration,
    observability: RawObservabilityConfiguration,
    llm: RawLlmConfiguration,
    mz: RawMzConfiguration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLlmConfiguration {
    clients: Vec<RawLlmClientConfiguration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProjectsConfiguration {
    root: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRuntimeConfiguration {
    #[serde(rename = "async")]
    async_runtime: RawAsyncRuntimeConfiguration,
    cpu: RawCpuRuntimeConfiguration,
    filesystem: RawFilesystemRuntimeConfiguration,
    sqlite: RawSqliteRuntimeConfiguration,
    llm: RawLlmRuntimeConfiguration,
    lua: RawLuaRuntimeConfiguration,
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
struct RawFilesystemRuntimeConfiguration {
    worker_threads: u64,
    queue_capacity: u64,
    max_read_bytes: u64,
    max_directory_entries: u64,
    publisher: RawDirectoryPublisherConfiguration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDirectoryPublisherConfiguration {
    max_prepared_candidates: u64,
    max_candidate_entries: u64,
    max_candidate_depth: u64,
    max_candidate_bytes: u64,
    max_single_file_bytes: u64,
    max_recovery_artifacts_per_target: u64,
    target_lock_timeout_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSqliteRuntimeConfiguration {
    short_worker_threads: u64,
    short_queue_capacity: u64,
    max_open_connections: u64,
    max_interactive_sessions: u64,
    interactive_open_queue_capacity: u64,
    interactive_command_queue_capacity: u64,
    worker_stack_bytes: u64,
    max_statement_bytes: u64,
    max_parameter_bytes: u64,
    max_rows_per_query: u64,
    max_result_bytes_per_query: u64,
    busy_timeout_ms: u64,
    journal_mode: SqliteJournalMode,
    synchronous: SqliteSynchronous,
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
    worker_threads: u64,
    queue_capacity: u64,
    worker_stack_bytes: u64,
    memory_limit_bytes_per_vm: u64,
    cancel_check_instruction_interval: u64,
    max_error_bytes: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawObservabilityConfiguration {
    root: PathBuf,
    translation: RawEventLogConfiguration,
    write_back: RawEventLogConfiguration,
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
struct RawMzConfiguration {
    document: RawMzDocumentConfiguration,
    standard_asset: RawMzStandardAssetConfiguration,
    extract: RawMzExtractConfiguration,
    translate: RawMzTranslateConfiguration,
    languages: Vec<RawLanguageConfiguration>,
    translation_profiles: Vec<RawTranslationProfileConfiguration>,
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
struct RawMzExtractConfiguration {
    builtin: RawMzScanConfiguration,
    rules: RawMzScanConfiguration,
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
struct RawTranslationProfileConfiguration {
    id: String,
    llm_client: String,
    max_in_flight_tasks: u64,
    planning: RawTranslationPlanningConfiguration,
    execution: RawTranslationExecutionConfiguration,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTranslationPlanningConfiguration {
    scope_concurrency: u64,
    max_message_characters: u64,
    systems: Vec<RawTranslationSystemConfiguration>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTranslationSystemConfiguration {
    source_language: String,
    target_language: String,
    path: PathBuf,
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
    id: String,
    endpoint: String,
    auth: RawLlmAuthConfiguration,
    model: String,
    allow_plain_http_loopback: bool,
    request_timeout_ms: u64,
    max_request_bytes: u64,
    max_response_bytes: u64,
    max_error_response_bytes: u64,
    requests_per_minute: u64,
    burst_requests: u64,
    #[serde(deserialize_with = "deserialize_zeroizing_string")]
    request_body_extra: Zeroizing<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawLlmAuthConfiguration {
    Name(String),
    Bearer(RawBearerConfiguration),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawBearerConfiguration {
    #[serde(deserialize_with = "deserialize_secret_string")]
    bearer: SecretString,
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    use secrecy::ExposeSecret;

    use super::*;

    #[test]
    fn repository_example_is_the_current_complete_contract() {
        let directory = TestDirectory::new();
        let path = directory.path().join("config.toml");
        fs::write(&path, include_str!("../../config.example.toml")).expect("应写入示例配置");
        load_configuration(&path).expect("仓库示例必须符合当前完整契约");
    }

    #[test]
    fn explicit_and_default_configuration_paths_use_their_declared_bases() {
        let current = absolute_test_path("cwd");
        let app_data = absolute_test_path("appdata");
        assert_eq!(
            resolve_configuration_path(
                Some(Path::new("settings/att.toml")),
                &current,
                Some(app_data.as_os_str()),
            )
            .expect("显式配置路径应合法"),
            current.join("settings/att.toml")
        );
        assert_eq!(
            resolve_configuration_path(None, &current, Some(app_data.as_os_str()))
                .expect("默认配置路径应合法"),
            app_data.join("ATT/config.toml")
        );
    }

    #[test]
    fn strict_configuration_builds_all_current_sections_and_resolves_paths() {
        let directory = TestDirectory::new();
        let path = directory.write("config.toml", &valid_configuration());
        let configuration = load_configuration(&path).expect("完整配置应合法");
        let configuration_directory =
            fs::canonicalize(directory.path()).expect("测试配置目录应可规范化");

        assert_eq!(
            configuration.projects_root(),
            configuration_directory.join("projects")
        );
        assert_eq!(
            configuration.observability().root(),
            configuration_directory.join("logs")
        );
        assert_eq!(
            configuration.runtime().llm().tls().additional_pem_files(),
            &[configuration_directory.join("certificates/provider.pem")]
        );
        let profile = configuration
            .mz()
            .translation_profile("local")
            .expect("Profile 应存在");
        assert_eq!(
            profile.planning().systems()[0].markdown_path(),
            configuration_directory.join("prompts/ja-zh.md")
        );
        assert_eq!(profile.llm_client_id().as_str(), "local-client");
        let client = configuration
            .llm_clients()
            .get(profile.llm_client_id())
            .expect("Profile 引用的公共客户端应存在");
        assert_eq!(
            client.request_body_extra().get("temperature"),
            Some(&JsonValue::from(0.2))
        );
        assert!(matches!(client.auth(), LlmAuthConfiguration::None));
        assert_eq!(
            configuration
                .mz()
                .translation_profile_ids()
                .collect::<Vec<_>>(),
            vec!["local"]
        );
    }

    #[test]
    fn unknown_field_and_duplicate_key_are_rejected_by_toml_boundary() {
        let directory = TestDirectory::new();
        for (name, source) in [
            (
                "unknown.toml",
                valid_configuration().replacen(
                    "root = \"projects\"",
                    "root = \"projects\"\nunknown = true",
                    1,
                ),
            ),
            (
                "duplicate.toml",
                valid_configuration().replacen(
                    "root = \"projects\"",
                    "root = \"projects\"\nroot = \"other\"",
                    1,
                ),
            ),
        ] {
            let path = directory.write(name, &source);
            assert!(matches!(
                load_configuration(&path),
                Err(ConfigurationLoadError::InvalidToml { .. })
            ));
        }
    }

    #[test]
    fn every_policy_field_is_explicit_even_when_disabled() {
        let directory = TestDirectory::new();
        let source = valid_configuration().replace(
            "quote_repair_pairs = [[\"“\", \"”\"], [\"‘\", \"’\"]]\n",
            "",
        );
        let path = directory.write("missing-policy.toml", &source);
        assert!(matches!(
            load_configuration(&path),
            Err(ConfigurationLoadError::InvalidToml { .. })
        ));

        let disabled = valid_configuration().replace(
            "quote_repair_pairs = [[\"“\", \"”\"], [\"‘\", \"’\"]]",
            "quote_repair_pairs = []",
        );
        let path = directory.write("disabled-policy.toml", &disabled);
        load_configuration(&path).expect("显式空数组应表示关闭引号修复");
    }

    #[test]
    fn zero_resource_and_reserved_request_body_field_are_rejected() {
        let directory = TestDirectory::new();
        let zero = directory.write(
            "zero.toml",
            &valid_configuration().replacen("worker_threads = 2", "worker_threads = 0", 1),
        );
        let ConfigurationLoadError::InvalidValue(error) =
            load_configuration(&zero).err().expect("零线程必须拒绝")
        else {
            panic!("应返回配置值错误");
        };
        assert_eq!(error.field(), "runtime.async.worker_threads");

        let reserved = directory.write(
            "reserved.toml",
            &valid_configuration().replacen(
                "\"temperature\": 0.2",
                "\"temperature\": 0.2, \"model\": \"other\"",
                1,
            ),
        );
        let ConfigurationLoadError::InvalidValue(error) = load_configuration(&reserved)
            .err()
            .expect("保留请求字段必须拒绝")
        else {
            panic!("应返回配置值错误");
        };
        assert!(error.field().ends_with("request_body_extra.model"));
    }

    #[test]
    fn remote_plain_http_and_duplicate_profile_ids_are_rejected() {
        let directory = TestDirectory::new();
        let remote_http = directory.write(
            "http.toml",
            &valid_configuration().replace(
                "http://127.0.0.1:8080/v1/chat/completions",
                "http://example.com/v1/chat/completions",
            ),
        );
        let ConfigurationLoadError::InvalidValue(error) = load_configuration(&remote_http)
            .err()
            .expect("远程 HTTP 必须拒绝")
        else {
            panic!("应返回配置值错误");
        };
        assert!(error.field().ends_with(".endpoint"));

        let profile = profile_configuration();
        let duplicate = directory.write(
            "duplicate-profile.toml",
            &valid_configuration().replace(&profile, &format!("{profile}\n{profile}")),
        );
        let ConfigurationLoadError::InvalidValue(error) = load_configuration(&duplicate)
            .err()
            .expect("重复 Profile ID 必须拒绝")
        else {
            panic!("应返回配置值错误");
        };
        assert!(error.field().ends_with(".id"));
    }

    #[test]
    fn endpoint_fragment_model_whitespace_and_auth_unknown_field_are_rejected() {
        let directory = TestDirectory::new();
        for (name, source, expected_field) in [
            (
                "fragment.toml",
                valid_configuration().replace(
                    "http://127.0.0.1:8080/v1/chat/completions",
                    "http://127.0.0.1:8080/v1/chat/completions#fragment",
                ),
                ".endpoint",
            ),
            (
                "model-whitespace.toml",
                valid_configuration().replace("model = \"test-model\"", "model = \" test-model\""),
                ".model",
            ),
            (
                "auth-unknown.toml",
                valid_configuration().replace(
                    "auth = \"none\"",
                    "auth = { bearer = \"secret\", unknown = true }",
                ),
                "",
            ),
        ] {
            let path = directory.write(name, &source);
            let error = load_configuration(&path).err().expect("非法配置必须拒绝");
            if expected_field.is_empty() {
                assert!(matches!(error, ConfigurationLoadError::InvalidToml { .. }));
            } else {
                let ConfigurationLoadError::InvalidValue(error) = error else {
                    panic!("应返回配置值错误");
                };
                assert!(error.field().ends_with(expected_field));
            }
        }
    }

    #[test]
    fn bearer_is_loaded_directly_and_debug_is_redacted() {
        let directory = TestDirectory::new();
        let secret = "must-not-leak-bearer-7f91";
        let sensitive_extra = "must-not-leak-extra-4c28";
        let path = directory.write(
            "bearer.toml",
            &valid_configuration()
                .replace(
                    "auth = \"none\"",
                    &format!("auth = {{ bearer = \"{secret}\" }}"),
                )
                .replace(
                    "\"temperature\": 0.2",
                    &format!("\"sensitive\": \"{sensitive_extra}\""),
                ),
        );
        let configuration = load_configuration(&path).expect("配置内 Bearer 应合法");
        let client = configuration
            .llm_clients()
            .get(&LlmClientId::from_validated("local-client".to_owned()))
            .expect("客户端应存在");
        let LlmAuthConfiguration::Bearer(bearer) = client.auth() else {
            panic!("应建立 Bearer 认证");
        };
        assert_eq!(bearer.expose_secret(), secret);
        let debug = format!("{client:?}");
        assert!(!debug.contains(secret));
        assert!(!debug.contains(sensitive_extra));
        assert!(debug.contains("sensitive"));
    }

    #[test]
    fn public_client_catalog_supports_shared_references_and_rejects_invalid_catalogs() {
        let directory = TestDirectory::new();
        let primary_client = client_configuration();
        let secondary_client = primary_client
            .replace("id = \"local-client\"", "id = \"secondary-client\"")
            .replace("127.0.0.1:8080", "127.0.0.1:8081");
        let primary_profile = profile_configuration();
        let secondary_profile = primary_profile.replace("id = \"local\"", "id = \"secondary\"");
        let shared = valid_configuration()
            .replace(
                &primary_client,
                &format!("{primary_client}\n{secondary_client}"),
            )
            .replace(
                &primary_profile,
                &format!("{primary_profile}\n{secondary_profile}"),
            );
        let configuration = load_configuration(&directory.write("shared.toml", &shared))
            .expect("多个 Profile 应能引用同一公共客户端");
        assert!(
            configuration
                .llm_clients()
                .get(&LlmClientId::from_validated("secondary-client".to_owned()))
                .is_some()
        );
        assert_eq!(
            configuration
                .mz()
                .translation_profile("secondary")
                .expect("第二个 Profile 应存在")
                .llm_client_id()
                .as_str(),
            "local-client"
        );

        for (name, source, expected_field) in [
            (
                "empty-clients.toml",
                valid_configuration().replace(&primary_client, "[llm]\nclients = []"),
                "llm.clients",
            ),
            (
                "duplicate-client.toml",
                valid_configuration().replace(
                    &primary_client,
                    &format!("{primary_client}\n{primary_client}"),
                ),
                "llm.clients[1].id",
            ),
            (
                "unknown-reference.toml",
                valid_configuration().replace(
                    "llm_client = \"local-client\"",
                    "llm_client = \"missing-client\"",
                ),
                "mz.translation_profiles[0].llm_client",
            ),
            (
                "blank-reference.toml",
                valid_configuration()
                    .replace("llm_client = \"local-client\"", "llm_client = \"  \""),
                "mz.translation_profiles[0].llm_client",
            ),
        ] {
            let ConfigurationLoadError::InvalidValue(error) =
                load_configuration(&directory.write(name, &source))
                    .err()
                    .expect("非法公共客户端目录或引用必须拒绝")
            else {
                panic!("应返回配置值错误");
            };
            assert_eq!(error.field(), expected_field);
        }
    }

    #[test]
    fn request_body_extra_is_one_strict_json_object_contract() {
        let directory = TestDirectory::new();
        let accepted = replace_request_body_extra(
            &valid_configuration(),
            r#"{
  "n": 2,
  "max_tokens": 4096,
  "max_completion_tokens": 8192,
  "thinking": {"model": "nested", "values": [null, true, 1.5]}
}"#,
        );
        let configuration = load_configuration(&directory.write("accepted.toml", &accepted))
            .expect("用户拥有的任意非保留 JSON 字段应保留");
        let extra = configuration
            .llm_clients()
            .get(&LlmClientId::from_validated("local-client".to_owned()))
            .expect("客户端应存在")
            .request_body_extra();
        assert_eq!(extra.get("n"), Some(&JsonValue::from(2)));
        assert_eq!(extra.get("max_tokens"), Some(&JsonValue::from(4096)));
        assert_eq!(
            extra.get("thinking").and_then(|value| value.get("model")),
            Some(&JsonValue::from("nested"))
        );

        for (name, json, expected_suffix) in [
            ("non-object.toml", "[]", "request_body_extra"),
            (
                "trailing-comma.toml",
                r#"{"value": 1,}"#,
                "request_body_extra",
            ),
            (
                "comment.toml",
                "{\"value\": 1 // comment\n}",
                "request_body_extra",
            ),
            (
                "parallel-values.toml",
                r#"{"value": 1} {"other": 2}"#,
                "request_body_extra",
            ),
            (
                "truncated.toml",
                r#"{"value": "unfinished"#,
                "request_body_extra",
            ),
            (
                "top-duplicate.toml",
                r#"{"value": 1, "value": 2}"#,
                "request_body_extra",
            ),
            (
                "nested-duplicate.toml",
                r#"{"nested": {"value": 1, "value": 2}}"#,
                "request_body_extra",
            ),
            (
                "reserved-model.toml",
                r#"{"model": "other"}"#,
                "request_body_extra.model",
            ),
            (
                "reserved-messages.toml",
                r#"{"messages": []}"#,
                "request_body_extra.messages",
            ),
            (
                "reserved-stream.toml",
                r#"{"stream": true}"#,
                "request_body_extra.stream",
            ),
        ] {
            let source = replace_request_body_extra(&valid_configuration(), json);
            let ConfigurationLoadError::InvalidValue(error) =
                load_configuration(&directory.write(name, &source))
                    .err()
                    .expect("非法扩展正文必须拒绝")
            else {
                panic!("应返回配置值错误");
            };
            assert!(error.field().ends_with(expected_suffix));
        }

        let wrong_type = valid_configuration().replace(
            REQUEST_BODY_EXTRA_FIXTURE,
            "request_body_extra = { temperature = 0.2 }",
        );
        assert!(matches!(
            load_configuration(&directory.write("wrong-type.toml", &wrong_type)),
            Err(ConfigurationLoadError::InvalidToml { .. })
        ));
    }

    #[test]
    fn configuration_errors_never_retain_or_render_secret_source_text() {
        let directory = TestDirectory::new();
        let secret = "must-not-leak-config-source-a3d9";
        let invalid_toml = valid_configuration()
            .replace(
                "auth = \"none\"",
                &format!("auth = {{ bearer = \"{secret}\" }}"),
            )
            .replace(
                "root = \"projects\"",
                "root = \"projects\"\nroot = \"other\"",
            );
        assert_error_chain_does_not_contain(
            load_configuration(&directory.write("invalid-toml.toml", &invalid_toml))
                .err()
                .expect("重复 TOML key 必须拒绝"),
            secret,
        );

        let invalid_json = valid_configuration().replace(
            "auth = \"none\"",
            &format!("auth = {{ bearer = \"{secret}\" }}"),
        );
        let invalid_json = replace_request_body_extra(
            &invalid_json,
            format!(r#"{{"sensitive": "{secret}",}}"#).as_str(),
        );
        assert_error_chain_does_not_contain(
            load_configuration(&directory.write("invalid-json.toml", &invalid_json))
                .err()
                .expect("非法 JSON 必须拒绝"),
            secret,
        );

        let invalid_bearer = valid_configuration().replace(
            "auth = \"none\"",
            &format!("auth = {{ bearer = \" {secret} \" }}"),
        );
        assert_error_chain_does_not_contain(
            load_configuration(&directory.write("invalid-bearer.toml", &invalid_bearer))
                .err()
                .expect("带首尾空白的密钥必须拒绝"),
            secret,
        );
    }

    #[test]
    fn configuration_file_has_fixed_bootstrap_size_limit() {
        let directory = TestDirectory::new();
        let path = directory.path().join("large.toml");
        let file = File::create(&path).expect("应创建测试文件");
        file.set_len(MAX_CONFIGURATION_BYTES + 1)
            .expect("应扩展测试文件");
        assert!(matches!(
            load_configuration(&path),
            Err(ConfigurationLoadError::TooLarge { .. })
        ));
    }

    const REQUEST_BODY_EXTRA_FIXTURE: &str = r#"request_body_extra = '''
{
  "temperature": 0.2
}
'''"#;

    fn replace_request_body_extra(configuration: &str, json: &str) -> String {
        let replacement = format!("request_body_extra = '''\n{json}\n'''");
        let replaced = configuration.replace(REQUEST_BODY_EXTRA_FIXTURE, &replacement);
        assert_ne!(replaced, configuration, "测试夹具必须命中扩展正文");
        replaced
    }

    fn assert_error_chain_does_not_contain(error: ConfigurationLoadError, sentinel: &str) {
        let display = error.to_string();
        let debug = format!("{error:?}");
        assert!(!display.contains(sentinel));
        assert!(!debug.contains(sentinel));

        let mut source = error.source();
        while let Some(current) = source {
            assert!(!current.to_string().contains(sentinel));
            assert!(!format!("{current:?}").contains(sentinel));
            source = current.source();
        }
    }

    fn valid_configuration() -> String {
        format!(
            r#"[projects]
root = "projects"

[runtime.async]
worker_threads = 2
max_blocking_threads = 4
blocking_thread_keep_alive_ms = 1000

[runtime.cpu]
worker_threads = 2
queue_capacity = 16

[runtime.filesystem]
worker_threads = 2
queue_capacity = 16
max_read_bytes = 8388608
max_directory_entries = 10000

[runtime.filesystem.publisher]
max_prepared_candidates = 2
max_candidate_entries = 100000
max_candidate_depth = 64
max_candidate_bytes = 1073741824
max_single_file_bytes = 67108864
max_recovery_artifacts_per_target = 8
target_lock_timeout_ms = 5000

[runtime.sqlite]
short_worker_threads = 2
short_queue_capacity = 32
max_open_connections = 8
max_interactive_sessions = 2
interactive_open_queue_capacity = 8
interactive_command_queue_capacity = 32
worker_stack_bytes = 2097152
max_statement_bytes = 1048576
max_parameter_bytes = 8388608
max_rows_per_query = 100000
max_result_bytes_per_query = 67108864
busy_timeout_ms = 5000
journal_mode = "wal"
synchronous = "full"

[runtime.llm]
max_active_requests = 4
queue_capacity = 16
admission_timeout_ms = 5000
connect_timeout_ms = 10000
read_timeout_ms = 60000
pool_idle_timeout_ms = 30000
pool_max_idle_per_host = 4
proxy = false

[runtime.llm.tls]
additional_pem_files = ["certificates/provider.pem"]

[runtime.lua]
worker_threads = 2
queue_capacity = 8
worker_stack_bytes = 4194304
memory_limit_bytes_per_vm = 67108864
cancel_check_instruction_interval = 10000
max_error_bytes = 65536

[observability]
root = "logs"

[observability.translation]
queue_capacity = 128
lock_timeout_ms = 5000
max_record_bytes = 1048576
max_file_bytes = 67108864
retained_rotated_files = 5

[observability.write_back]
queue_capacity = 128
lock_timeout_ms = 5000
max_record_bytes = 1048576
max_file_bytes = 67108864
retained_rotated_files = 5

{}

[mz.document]
read_concurrency = 4
parse_concurrency = 2

[mz.standard_asset]
decode_concurrency = 2
leaves_per_decode_job = 256

[mz.extract.builtin]
scan_concurrency = 2

[mz.extract.rules]
scan_concurrency = 2

[mz.extract.store]
encode_concurrency = 2
groups_per_encode_job = 128

[mz.translate.store]
encode_concurrency = 2
leaves_per_encode_job = 256

[[mz.languages]]
type = "japanese"
id = "ja"
minimum_kana_characters = 1
allowed_terms = []
quote_repair_pairs = [["“", "”"], ["‘", "’"]]

[[mz.languages]]
type = "english"
id = "en"
minimum_word_count = 1
minimum_letter_count = 2
ignored_terms = []
minimum_copied_word_count = 2
minimum_copied_letter_count = 4
allowed_terms = []

{}"#,
            client_configuration(),
            profile_configuration()
        )
    }

    fn client_configuration() -> String {
        r#"[[llm.clients]]
id = "local-client"
endpoint = "http://127.0.0.1:8080/v1/chat/completions"
auth = "none"
model = "test-model"
allow_plain_http_loopback = true
request_timeout_ms = 60000
max_request_bytes = 1048576
max_response_bytes = 4194304
max_error_response_bytes = 65536
requests_per_minute = 60
burst_requests = 4
request_body_extra = '''
{
  "temperature": 0.2
}
'''
"#
        .to_owned()
    }

    fn profile_configuration() -> String {
        r#"[[mz.translation_profiles]]
id = "local"
llm_client = "local-client"
max_in_flight_tasks = 2

[mz.translation_profiles.planning]
scope_concurrency = 2
max_message_characters = 24000

[[mz.translation_profiles.planning.systems]]
source_language = "ja"
target_language = "zh-Hans"
path = "prompts/ja-zh.md"

[mz.translation_profiles.execution]
network_retry_delays_ms = [250, 1000]
max_network_retry_after_ms = 5000
"#
        .to_owned()
    }

    fn absolute_test_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "att-config-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("系统时间应有效")
                .as_nanos()
        ))
    }

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new() -> Self {
            let path = absolute_test_path("directory");
            fs::create_dir_all(&path).expect("应创建测试目录");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn write(&self, name: &str, source: &str) -> PathBuf {
            let path = self.path.join(name);
            fs::write(&path, source).expect("应写入测试配置");
            path
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
