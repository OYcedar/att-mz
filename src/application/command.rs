//! 生产命令装配与最终结果呈现。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::application::arguments::{
    ExtractArguments, InitArguments, MzCommand, TranslateArguments, WriteBackArguments,
};
use crate::application::config::{
    ApplicationConfiguration, CompletionLimitParameter as ConfigCompletionLimitParameter,
    EventLogConfiguration, LlmRuntimeConfiguration, ProxyConfiguration,
    SqliteJournalMode as ConfigSqliteJournalMode, SqliteSynchronous as ConfigSqliteSynchronous,
    TranslationProfileConfiguration,
};
use crate::att_mz::MzWriteBackLayoutProfile;
use crate::att_mz::extract::builtin::{BuiltInExtractionConfig, BuiltInExtractionService};
use crate::att_mz::extract::document::{MzDocumentReadingConfig, MzProjectDocumentReadingService};
use crate::att_mz::extract::lua::LuaExtractionService;
use crate::att_mz::extract::rules::{RulesExtractionConfig, RulesExtractionService};
use crate::att_mz::extract::service::ExtractService;
use crate::att_mz::extract::store::asset_store::{
    MzExtractionAssetStore, MzExtractionAssetStoreConfig,
};
use crate::att_mz::extract::{ExtractInput, ExtractOutput, ExtractUseCase, ExtractionSelection};
use crate::att_mz::init::InitUseCase;
use crate::att_mz::init::{InitInput, InitOutput, InitService, ProjectWorkspaceCreationService};
use crate::att_mz::lua::hosting::TrustedLuaExecutionHostingService;
use crate::att_mz::project::{ExistingProjectOpeningService, OpenedProject};
use crate::att_mz::standard_asset::MzStandardAssetReadingConfig;
use crate::att_mz::translate::asset_reader::MzStandardTranslationAssetReadingService;
use crate::att_mz::translate::executor::{
    MzStandardTranslationTaskExecutionService, TranslationTaskResponseProcessingService,
};
use crate::att_mz::translate::lua::LuaTranslationService;
use crate::att_mz::translate::placeholder::Pcre2PlaceholderService;
use crate::att_mz::translate::planner::MzStandardTranslationTaskPlanningService;
use crate::att_mz::translate::planning_resource::JsonTranslationPlanningResourceReadingService;
use crate::att_mz::translate::profile::{
    InMemoryTranslationExecutionProfileResolver, MzTranslationExecutionConfiguration,
    MzTranslationExecutionPayload, MzTranslationPlanningConfiguration, TranslationExecutionProfile,
    TranslationProfileCatalog, TranslationProfileLanguagePair,
};
use crate::att_mz::translate::result_store::{
    MzStandardTranslationResultStorageConfig, MzStandardTranslationResultStorageService,
};
use crate::att_mz::translate::service::TranslateService;
use crate::att_mz::translate::standard::{
    StandardTranslation, StandardTranslationInput, StandardTranslationRunReport,
    StandardTranslationService,
};
use crate::att_mz::translate::{TranslateInput, TranslateOutput, TranslateUseCase};
use crate::att_mz::write_back::asset_reader::MzStandardWriteBackAssetReadingService;
use crate::att_mz::write_back::lua::LuaWriteBackService;
use crate::att_mz::write_back::publisher::StandardWriteBackPublishingService;
use crate::att_mz::write_back::rewriter::MzWriteBackDocumentRewritingService;
use crate::att_mz::write_back::standard::{
    ConservativeMzWriteBackTextLayouter, StandardWriteBackService,
};
use crate::att_mz::write_back::{
    StandardWriteBack, StandardWriteBackReport, WriteBackInput, WriteBackOutput, WriteBackService,
    WriteBackUseCase,
};
use crate::execution::CooperativeCancellation;
use crate::observability::RunIdGenerator;
use crate::project_database::{
    ProjectDatabaseCreationService, ProjectDatabaseRecordReadingService, StoredProjectRecord,
};
use crate::runtime::cpu::{BoundedCpuExecutor, CpuExecutorConfig};
use crate::runtime::delay::TokioAsyncDelay;
use crate::runtime::filesystem::{
    DirectoryPublisherConfig, SystemFileSystem, SystemFileSystemConfig,
};
use crate::runtime::json_lines::{
    JsonLinesEventLogFinalizer, JsonLinesStreamConfig, TranslationJsonLinesEventLog,
    TranslationRunLogContext, WriteBackJsonLinesEventLog, WriteBackRunLogContext,
};
use crate::runtime::llm::{
    CompletionLimit, CompletionLimitParameter, LlmProxyConfiguration,
    LlmTlsConfiguration as RuntimeLlmTlsConfiguration, OpenAiAuthentication,
    OpenAiChatCompletionError, OpenAiChatCompletionExecutor, OpenAiChatCompletionProfile,
    OpenAiExecutorConfiguration,
};
use crate::runtime::lua::{
    TrustedLua54Runtime, TrustedLua54RuntimeConfiguration, TrustedLua54RuntimeError,
};
use crate::runtime::run_id::WindowsRunIdGenerator;
use crate::runtime::sqlite::{
    RusqliteStorage, RusqliteStorageConfiguration, SqliteJournalMode, SqliteSynchronous,
};
use crate::runtime::windows::validate_local_case_insensitive_ntfs_directory;
use crate::storage::file_system::FileReader;

type BoxedError = Box<dyn Error + Send + Sync + 'static>;

/// 一个 MZ 命令成功完成后的类型化结果。
pub(crate) enum MzCommandOutput {
    Init(InitOutput),
    Extract(ExtractOutput),
    Translate(TranslateOutput),
    WriteBack(WriteBackOutput),
}

/// 按本次命令只构造实际需要的生产纵向切片。
pub(crate) struct ProductionMzCommandRunner {
    configuration: ApplicationConfiguration,
}

impl ProductionMzCommandRunner {
    pub(crate) const fn new(configuration: ApplicationConfiguration) -> Self {
        Self { configuration }
    }

    pub(crate) async fn run(self, command: MzCommand) -> ProductionCommandRunReport {
        match command {
            MzCommand::Init(arguments) => self.run_init(arguments).await,
            MzCommand::Extract(arguments) => self.run_extract(arguments).await,
            MzCommand::Translate(arguments) => self.run_translate(arguments).await,
            MzCommand::WriteBack(arguments) => self.run_write_back(arguments).await,
        }
    }

    async fn run_init(self, arguments: InitArguments) -> ProductionCommandRunReport {
        let cancellation = CooperativeCancellation::default();
        let sqlite = match build_sqlite(self.configuration.runtime().sqlite()) {
            Ok(sqlite) => sqlite,
            Err(error) => return ProductionCommandRunReport::construction_failed(error),
        };
        let file_system = match build_file_system(self.configuration.runtime().filesystem()) {
            Ok(file_system) => file_system,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(error) = sqlite.shutdown().await {
                    shutdown.push("SQLite", error);
                }
                return ProductionCommandRunReport::construction_failed_with_shutdown(
                    error, shutdown,
                );
            }
        };
        let projects_root = match validate_projects_root(self.configuration.projects_root()) {
            Ok(path) => path,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return ProductionCommandRunReport::construction_failed_with_shutdown(
                    error, shutdown,
                );
            }
        };

        let database = ProjectDatabaseCreationService::new(sqlite.clone());
        let workspace = ProjectWorkspaceCreationService::new(
            projects_root,
            database,
            file_system.clone(),
            cancellation.clone(),
        );
        let service = InitService::new(file_system.clone(), workspace, cancellation.clone());
        let input = InitInput {
            name: arguments.project.name,
            game_root: arguments.path,
            source_language: arguments.source_language,
            target_language: arguments.target_language,
            layout_profile: MzWriteBackLayoutProfile::new(
                arguments.dialogue_max_fullwidth_chars,
                arguments.scrolling_text_max_fullwidth_chars,
                arguments.help_description_max_fullwidth_chars,
            ),
        };

        let controlled = await_or_interrupt(service.execute(input), &cancellation).await;
        let mut shutdown = ShutdownFailures::default();
        if let Err(error) = sqlite.shutdown().await {
            shutdown.push("SQLite", error);
        }
        if let Err(error) = file_system.shutdown().await {
            shutdown.push("FileSystem", error);
        }
        ProductionCommandRunReport::from_controlled(
            controlled.map(|output| {
                output
                    .map(MzCommandOutput::Init)
                    .map_err(ProductionCommandError::execute)
            }),
            shutdown,
        )
    }

    async fn run_extract(self, arguments: ExtractArguments) -> ProductionCommandRunReport {
        let cancellation = CooperativeCancellation::default();
        let sqlite = match build_sqlite(self.configuration.runtime().sqlite()) {
            Ok(sqlite) => sqlite,
            Err(error) => return ProductionCommandRunReport::construction_failed(error),
        };
        let file_system = match build_file_system(self.configuration.runtime().filesystem()) {
            Ok(file_system) => file_system,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                return ProductionCommandRunReport::construction_failed_with_shutdown(
                    error, shutdown,
                );
            }
        };
        let projects_root = match validate_projects_root(self.configuration.projects_root()) {
            Ok(path) => path,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return ProductionCommandRunReport::construction_failed_with_shutdown(
                    error, shutdown,
                );
            }
        };
        let cpu = match build_cpu(self.configuration.runtime().cpu()) {
            Ok(cpu) => cpu,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return ProductionCommandRunReport::construction_failed_with_shutdown(
                    error, shutdown,
                );
            }
        };

        let lua_runtime = if arguments.lua.is_some() {
            match build_lua(self.configuration.runtime().lua()) {
                Ok(runtime) => Some(runtime),
                Err(error) => {
                    let mut shutdown = ShutdownFailures::default();
                    if let Err(source) = sqlite.shutdown().await {
                        shutdown.push("SQLite", source);
                    }
                    if let Err(source) = file_system.shutdown().await {
                        shutdown.push("FileSystem", source);
                    }
                    if let Err(source) = cpu.shutdown() {
                        shutdown.push("CPU", source);
                    }
                    return ProductionCommandRunReport::construction_failed_with_shutdown(
                        error, shutdown,
                    );
                }
            }
        } else {
            None
        };

        let project_reader =
            ProjectDatabaseRecordReadingService::new(projects_root, sqlite.clone());
        let opener = ExistingProjectOpeningService::new(project_reader, file_system.clone());
        let document = self.configuration.mz().document();
        let document_config =
            MzDocumentReadingConfig::new(document.read_concurrency(), document.parse_concurrency());
        let builtin_reader = MzProjectDocumentReadingService::new(
            file_system.clone(),
            file_system.clone(),
            cpu.clone(),
            document_config,
        );
        let rules_reader = MzProjectDocumentReadingService::new(
            file_system.clone(),
            file_system.clone(),
            cpu.clone(),
            document_config,
        );
        let store = self.configuration.mz().extract_store();
        let store_config = MzExtractionAssetStoreConfig::new(
            store.encode_concurrency(),
            store.groups_per_encode_job(),
        );
        let builtin_store = MzExtractionAssetStore::new(sqlite.clone(), cpu.clone(), store_config);
        let rules_store = MzExtractionAssetStore::new(sqlite.clone(), cpu.clone(), store_config);
        let builtin = BuiltInExtractionService::new(
            builtin_reader,
            builtin_store,
            cpu.clone(),
            BuiltInExtractionConfig::new(
                self.configuration.mz().extract_builtin().scan_concurrency(),
            ),
        );
        let rules = RulesExtractionService::new(
            file_system.clone(),
            rules_reader,
            rules_store,
            cpu.clone(),
            RulesExtractionConfig::new(self.configuration.mz().extract_rules().scan_concurrency()),
        );
        let lua = lua_runtime.as_ref().map(|runtime| {
            let host = TrustedLuaExecutionHostingService::<
                _,
                OpenAiChatCompletionExecutor,
                _,
                _,
            >::without_llm(
                file_system.clone(), runtime.clone(), sqlite.clone()
            );
            LuaExtractionService::new(host)
        });
        let service = ExtractService::new(opener, builtin, rules, lua, cancellation.clone());
        let input = ExtractInput {
            name: arguments.project.name,
            selection: ExtractionSelection::new(arguments.builtin, arguments.rules, arguments.lua)
                .expect("Clap 必须保证至少选择一个提取阶段"),
        };

        let (controlled, lua_cancel_error) = await_or_interrupt_with_lua(
            service.execute(input),
            &cancellation,
            lua_runtime.as_ref(),
        )
        .await;
        let mut shutdown = ShutdownFailures::default();
        if let Some(error) = lua_cancel_error {
            shutdown.push("Lua", error);
        } else if let Some(runtime) = lua_runtime.as_ref()
            && let Err(error) = runtime.shutdown().await
        {
            shutdown.push("Lua", error);
        }
        if let Err(error) = sqlite.shutdown().await {
            shutdown.push("SQLite", error);
        }
        if let Err(error) = file_system.shutdown().await {
            shutdown.push("FileSystem", error);
        }
        if let Err(error) = cpu.shutdown() {
            shutdown.push("CPU", error);
        }
        ProductionCommandRunReport::from_controlled(
            controlled.map(|output| {
                output
                    .map(MzCommandOutput::Extract)
                    .map_err(ProductionCommandError::execute)
            }),
            shutdown,
        )
    }

    async fn run_translate(self, arguments: TranslateArguments) -> ProductionCommandRunReport {
        let cancellation = CooperativeCancellation::default();
        let Some(profile_configuration) = self
            .configuration
            .mz()
            .translation_profile(&arguments.profile_id)
            .cloned()
        else {
            return ProductionCommandRunReport::construction_failed(
                ProductionCommandError::construct(
                    "TranslationProfile",
                    UnknownTranslationProfile {
                        requested_id: arguments.profile_id,
                        available_ids: self
                            .configuration
                            .mz()
                            .translation_profile_ids()
                            .map(str::to_owned)
                            .collect(),
                    },
                ),
            );
        };
        let authentication = match profile_configuration.llm().auth().resolve() {
            Ok(Some(secret)) => OpenAiAuthentication::bearer(secret),
            Ok(None) => OpenAiAuthentication::None,
            Err(error) => {
                return ProductionCommandRunReport::construction_failed(
                    ProductionCommandError::construct("LLM Credential", error),
                );
            }
        };
        let event_log_configuration =
            match build_json_lines_config(self.configuration.observability().translation()) {
                Ok(configuration) => configuration,
                Err(error) => return ProductionCommandRunReport::construction_failed(error),
            };
        let placeholders = match Pcre2PlaceholderService::new() {
            Ok(placeholders) => placeholders,
            Err(error) => {
                return ProductionCommandRunReport::construction_failed(
                    ProductionCommandError::construct("Placeholder", error),
                );
            }
        };

        let file_system = match build_file_system(self.configuration.runtime().filesystem()) {
            Ok(file_system) => file_system,
            Err(error) => return ProductionCommandRunReport::construction_failed(error),
        };
        let projects_root = match validate_projects_root(self.configuration.projects_root()) {
            Ok(path) => path,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return ProductionCommandRunReport::construction_failed_with_shutdown(
                    error, shutdown,
                );
            }
        };
        let loaded = match load_translation_materials(
            &file_system,
            &profile_configuration,
            self.configuration.runtime().llm(),
        )
        .await
        {
            Ok(loaded) => loaded,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return ProductionCommandRunReport::construction_failed_with_shutdown(
                    error, shutdown,
                );
            }
        };
        let planning_configuration = match MzTranslationPlanningConfiguration::new(
            profile_configuration.planning().scope_concurrency(),
            profile_configuration.planning().max_message_characters(),
            loaded.system_markdown,
        ) {
            Ok(configuration) => configuration,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return ProductionCommandRunReport::construction_failed_with_shutdown(
                    ProductionCommandError::construct("TranslationProfile", error),
                    shutdown,
                );
            }
        };
        let llm_profile = match build_llm_profile(&profile_configuration, authentication) {
            Ok(profile) => profile,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return ProductionCommandRunReport::construction_failed_with_shutdown(
                    error, shutdown,
                );
            }
        };
        let llm = match build_llm(
            self.configuration.runtime().llm(),
            loaded.additional_pem_roots,
        ) {
            Ok(llm) => llm,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return ProductionCommandRunReport::construction_failed_with_shutdown(
                    error, shutdown,
                );
            }
        };
        let sqlite = match build_sqlite(self.configuration.runtime().sqlite()) {
            Ok(sqlite) => sqlite,
            Err(error) => {
                llm.shutdown().await;
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return ProductionCommandRunReport::construction_failed_with_shutdown(
                    error, shutdown,
                );
            }
        };
        let cpu = match build_cpu(self.configuration.runtime().cpu()) {
            Ok(cpu) => cpu,
            Err(error) => {
                llm.shutdown().await;
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return ProductionCommandRunReport::construction_failed_with_shutdown(
                    error, shutdown,
                );
            }
        };
        let lua_runtime = if arguments.lua.is_some() {
            match build_lua(self.configuration.runtime().lua()) {
                Ok(runtime) => Some(runtime),
                Err(error) => {
                    llm.shutdown().await;
                    let mut shutdown = ShutdownFailures::default();
                    if let Err(source) = sqlite.shutdown().await {
                        shutdown.push("SQLite", source);
                    }
                    if let Err(source) = file_system.shutdown().await {
                        shutdown.push("FileSystem", source);
                    }
                    if let Err(source) = cpu.shutdown() {
                        shutdown.push("CPU", source);
                    }
                    return ProductionCommandRunReport::construction_failed_with_shutdown(
                        error, shutdown,
                    );
                }
            }
        } else {
            None
        };

        let payload = MzTranslationExecutionPayload::new(
            planning_configuration,
            MzTranslationExecutionConfiguration::new(
                profile_configuration
                    .execution()
                    .network_retry_delays()
                    .to_vec(),
                profile_configuration.execution().max_network_retry_after(),
            ),
            llm_profile,
        );
        let selected_profile = TranslationExecutionProfile::new(
            profile_configuration.id(),
            profile_configuration.max_in_flight_tasks(),
            payload,
        );
        let resolver = InMemoryTranslationExecutionProfileResolver::new(
            TranslationProfileCatalog::new([selected_profile])
                .expect("严格配置必须建立唯一非空 Profile 目录"),
        );
        let project_reader =
            ProjectDatabaseRecordReadingService::new(projects_root, sqlite.clone());
        let asset_configuration = self.configuration.mz().standard_asset();
        let asset_reader = MzStandardTranslationAssetReadingService::new(
            sqlite.clone(),
            cpu.clone(),
            MzStandardAssetReadingConfig::new(
                asset_configuration.decode_concurrency(),
                asset_configuration.leaves_per_decode_job(),
            ),
        );
        let languages = self.configuration.mz().language_modules();
        let resources =
            JsonTranslationPlanningResourceReadingService::new(file_system.clone(), cpu.clone());
        let planner =
            MzStandardTranslationTaskPlanningService::<_, _, OpenAiChatCompletionProfile>::new(
                resources,
                languages.clone(),
                placeholders,
                cpu.clone(),
            );
        let response_processor =
            TranslationTaskResponseProcessingService::new(cpu.clone(), languages);
        let executor = MzStandardTranslationTaskExecutionService::<
            _,
            _,
            _,
            ProductionTranslationProfile,
        >::new(llm.clone(), TokioAsyncDelay, response_processor);
        let store_configuration = self.configuration.mz().translate_store();
        let result_store = MzStandardTranslationResultStorageService::new(
            sqlite.clone(),
            cpu.clone(),
            MzStandardTranslationResultStorageConfig::new(
                store_configuration.encode_concurrency(),
                store_configuration.leaves_per_encode_job(),
            ),
        );
        let log_finalizer = Arc::new(Mutex::new(None));
        let standard = ProductionLoggedTranslation::new(
            (asset_reader, planner, executor, result_store),
            ProductionRunLogResources::new(
                self.configuration.observability().root().to_path_buf(),
                event_log_configuration,
                Arc::clone(&log_finalizer),
                cancellation.clone(),
            ),
        );
        let lua = lua_runtime.as_ref().map(|runtime| {
            let host = TrustedLuaExecutionHostingService::with_llm(
                file_system.clone(),
                llm.clone(),
                runtime.clone(),
                sqlite.clone(),
            );
            LuaTranslationService::new(host)
        });
        let service = TranslateService::new(
            resolver,
            project_reader,
            standard,
            lua,
            cancellation.clone(),
        );
        let input = TranslateInput {
            name: arguments.project.name,
            profile_id: profile_configuration.id().to_owned(),
            terminology_path: arguments.terms,
            placeholder_rules_path: arguments.placeholders,
            lua_script: arguments.lua,
        };

        let (controlled, lua_cancel_error) = await_or_interrupt_with_translation_roots(
            service.execute(input),
            &cancellation,
            lua_runtime.as_ref(),
            &llm,
        )
        .await;
        let mut shutdown = ShutdownFailures::default();
        if let Some(error) = lua_cancel_error {
            shutdown.push("Lua", error);
        } else if let Some(runtime) = lua_runtime.as_ref()
            && let Err(error) = runtime.shutdown().await
        {
            shutdown.push("Lua", error);
        }
        llm.shutdown().await;
        if let Err(error) = sqlite.shutdown().await {
            shutdown.push("SQLite", error);
        }
        if let Err(error) = file_system.shutdown().await {
            shutdown.push("FileSystem", error);
        }
        if let Err(error) = cpu.shutdown() {
            shutdown.push("CPU", error);
        }
        finalize_log(&log_finalizer, "TranslationLog", &mut shutdown).await;
        ProductionCommandRunReport::from_controlled(
            controlled.map(|output| {
                output
                    .map(MzCommandOutput::Translate)
                    .map_err(ProductionCommandError::execute)
            }),
            shutdown,
        )
    }

    async fn run_write_back(self, arguments: WriteBackArguments) -> ProductionCommandRunReport {
        let cancellation = CooperativeCancellation::default();
        let event_log_configuration =
            match build_json_lines_config(self.configuration.observability().write_back()) {
                Ok(configuration) => configuration,
                Err(error) => return ProductionCommandRunReport::construction_failed(error),
            };
        let file_system = match build_file_system(self.configuration.runtime().filesystem()) {
            Ok(file_system) => file_system,
            Err(error) => return ProductionCommandRunReport::construction_failed(error),
        };
        let projects_root = match validate_projects_root(self.configuration.projects_root()) {
            Ok(path) => path,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return ProductionCommandRunReport::construction_failed_with_shutdown(
                    error, shutdown,
                );
            }
        };
        let sqlite = match build_sqlite(self.configuration.runtime().sqlite()) {
            Ok(sqlite) => sqlite,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return ProductionCommandRunReport::construction_failed_with_shutdown(
                    error, shutdown,
                );
            }
        };
        let cpu = match build_cpu(self.configuration.runtime().cpu()) {
            Ok(cpu) => cpu,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return ProductionCommandRunReport::construction_failed_with_shutdown(
                    error, shutdown,
                );
            }
        };
        let lua_runtime = if arguments.lua.is_some() {
            match build_lua(self.configuration.runtime().lua()) {
                Ok(runtime) => Some(runtime),
                Err(error) => {
                    let mut shutdown = ShutdownFailures::default();
                    if let Err(source) = sqlite.shutdown().await {
                        shutdown.push("SQLite", source);
                    }
                    if let Err(source) = file_system.shutdown().await {
                        shutdown.push("FileSystem", source);
                    }
                    if let Err(source) = cpu.shutdown() {
                        shutdown.push("CPU", source);
                    }
                    return ProductionCommandRunReport::construction_failed_with_shutdown(
                        error, shutdown,
                    );
                }
            }
        } else {
            None
        };

        let project_reader =
            ProjectDatabaseRecordReadingService::new(projects_root, sqlite.clone());
        let opener = ExistingProjectOpeningService::new(project_reader, file_system.clone());
        let asset_configuration = self.configuration.mz().standard_asset();
        let asset_reader = MzStandardWriteBackAssetReadingService::new(
            sqlite.clone(),
            cpu.clone(),
            MzStandardAssetReadingConfig::new(
                asset_configuration.decode_concurrency(),
                asset_configuration.leaves_per_decode_job(),
            ),
        );
        let document_configuration = self.configuration.mz().document();
        let document_reader = MzProjectDocumentReadingService::new(
            file_system.clone(),
            file_system.clone(),
            cpu.clone(),
            MzDocumentReadingConfig::new(
                document_configuration.read_concurrency(),
                document_configuration.parse_concurrency(),
            ),
        );
        let rewriter = MzWriteBackDocumentRewritingService::new(document_reader, cpu.clone());
        let publisher =
            StandardWriteBackPublishingService::new(file_system.clone(), cancellation.clone());
        let log_finalizer = Arc::new(Mutex::new(None));
        let standard = ProductionLoggedWriteBack::new(
            (
                asset_reader,
                ConservativeMzWriteBackTextLayouter,
                rewriter,
                publisher,
            ),
            ProductionRunLogResources::new(
                self.configuration.observability().root().to_path_buf(),
                event_log_configuration,
                Arc::clone(&log_finalizer),
                cancellation.clone(),
            ),
        );
        let lua = lua_runtime.as_ref().map(|runtime| {
            let host = TrustedLuaExecutionHostingService::<
                _,
                OpenAiChatCompletionExecutor,
                _,
                _,
            >::without_llm(
                file_system.clone(), runtime.clone(), sqlite.clone()
            );
            LuaWriteBackService::new(host)
        });
        let service = WriteBackService::new(opener, standard, lua, cancellation.clone());
        let input = WriteBackInput {
            name: arguments.project.name,
            lua_script: arguments.lua,
        };

        let (controlled, lua_cancel_error) = await_or_interrupt_with_lua(
            service.execute(input),
            &cancellation,
            lua_runtime.as_ref(),
        )
        .await;
        let mut shutdown = ShutdownFailures::default();
        if let Some(error) = lua_cancel_error {
            shutdown.push("Lua", error);
        } else if let Some(runtime) = lua_runtime.as_ref()
            && let Err(error) = runtime.shutdown().await
        {
            shutdown.push("Lua", error);
        }
        if let Err(error) = sqlite.shutdown().await {
            shutdown.push("SQLite", error);
        }
        if let Err(error) = file_system.shutdown().await {
            shutdown.push("FileSystem", error);
        }
        if let Err(error) = cpu.shutdown() {
            shutdown.push("CPU", error);
        }
        finalize_log(&log_finalizer, "WriteBackLog", &mut shutdown).await;
        ProductionCommandRunReport::from_controlled(
            controlled.map(|output| {
                output
                    .map(MzCommandOutput::WriteBack)
                    .map_err(ProductionCommandError::execute)
            }),
            shutdown,
        )
    }
}

type ProductionTranslationProfile =
    Arc<TranslationExecutionProfile<MzTranslationExecutionPayload<OpenAiChatCompletionProfile>>>;
type EventLogFinalizerSlot = Arc<Mutex<Option<JsonLinesEventLogFinalizer>>>;

struct ProductionRunLogResources {
    root: PathBuf,
    configuration: JsonLinesStreamConfig,
    finalizer: EventLogFinalizerSlot,
    cancellation: CooperativeCancellation,
}

impl ProductionRunLogResources {
    fn new(
        root: PathBuf,
        configuration: JsonLinesStreamConfig,
        finalizer: EventLogFinalizerSlot,
        cancellation: CooperativeCancellation,
    ) -> Self {
        Self {
            root,
            configuration,
            finalizer,
            cancellation,
        }
    }
}

struct LoadedTranslationMaterials {
    system_markdown: Vec<(TranslationProfileLanguagePair, String)>,
    additional_pem_roots: Vec<Vec<u8>>,
}

async fn load_translation_materials(
    file_system: &SystemFileSystem,
    profile: &TranslationProfileConfiguration,
    runtime: &LlmRuntimeConfiguration,
) -> Result<LoadedTranslationMaterials, ProductionCommandError> {
    let mut system_markdown = Vec::with_capacity(profile.planning().systems().len());
    for system in profile.planning().systems() {
        let path = system.markdown_path().to_path_buf();
        let file = file_system
            .read_file(path.clone())
            .await
            .map_err(|source| ProductionCommandError::construct("SystemPrompt", source))?;
        let markdown = String::from_utf8(file.into_bytes()).map_err(|source| {
            let utf8 = source.utf8_error();
            ProductionCommandError::construct(
                "SystemPrompt",
                Utf8ResourceError {
                    path,
                    resource: "系统提示词",
                    valid_up_to: utf8.valid_up_to(),
                    error_len: utf8.error_len(),
                },
            )
        })?;
        let language_pair =
            TranslationProfileLanguagePair::new(system.source_language(), system.target_language())
                .map_err(|source| {
                    ProductionCommandError::construct("TranslationProfile", source)
                })?;
        system_markdown.push((language_pair, markdown));
    }

    let mut additional_pem_roots = Vec::with_capacity(runtime.tls().additional_pem_files().len());
    for path in runtime.tls().additional_pem_files() {
        let file = file_system
            .read_file(path.to_path_buf())
            .await
            .map_err(|source| ProductionCommandError::construct("LLM TLS", source))?;
        additional_pem_roots.push(file.into_bytes());
    }
    Ok(LoadedTranslationMaterials {
        system_markdown,
        additional_pem_roots,
    })
}

fn build_llm_profile(
    configuration: &TranslationProfileConfiguration,
    authentication: OpenAiAuthentication,
) -> Result<OpenAiChatCompletionProfile, ProductionCommandError> {
    let llm = configuration.llm();
    let parameter = match llm.completion_limit().parameter() {
        ConfigCompletionLimitParameter::MaxTokens => CompletionLimitParameter::MaxTokens,
        ConfigCompletionLimitParameter::MaxCompletionTokens => {
            CompletionLimitParameter::MaxCompletionTokens
        }
    };
    OpenAiChatCompletionProfile::new(
        llm.endpoint().clone(),
        authentication,
        llm.model(),
        llm.allow_plain_http_loopback(),
        llm.request_timeout(),
        llm.max_request_bytes(),
        llm.max_response_bytes(),
        llm.max_error_response_bytes(),
        llm.requests_per_minute(),
        llm.burst_requests(),
        CompletionLimit::new(parameter, llm.completion_limit().value()),
        llm.request_options().clone(),
    )
    .map_err(|source| ProductionCommandError::construct("LLM Profile", source))
}

fn build_llm(
    configuration: &LlmRuntimeConfiguration,
    additional_pem_roots: Vec<Vec<u8>>,
) -> Result<OpenAiChatCompletionExecutor, ProductionCommandError> {
    let proxy = match configuration.proxy() {
        ProxyConfiguration::Disabled => LlmProxyConfiguration::Disabled,
        ProxyConfiguration::Url(url) => LlmProxyConfiguration::Explicit(url.clone()),
    };
    let configuration = OpenAiExecutorConfiguration::new(
        configuration.max_active_requests(),
        configuration.queue_capacity(),
        configuration.admission_timeout(),
        configuration.connect_timeout(),
        configuration.read_timeout(),
        configuration.pool_idle_timeout(),
        configuration.pool_max_idle_per_host(),
        proxy,
        RuntimeLlmTlsConfiguration::new(additional_pem_roots),
    );
    OpenAiChatCompletionExecutor::new(configuration)
        .map_err(|source| ProductionCommandError::construct("LLM", source))
}

fn build_json_lines_config(
    configuration: EventLogConfiguration,
) -> Result<JsonLinesStreamConfig, ProductionCommandError> {
    JsonLinesStreamConfig::new(
        configuration.queue_capacity().get(),
        configuration.lock_timeout(),
        configuration.max_record_bytes().get(),
        u64::try_from(configuration.max_file_bytes().get()).expect("x86_64 usize 必须可表示为 u64"),
        configuration.retained_rotated_files(),
    )
    .map_err(|source| ProductionCommandError::construct("JsonLines", source))
}

fn validate_projects_root(path: &std::path::Path) -> Result<PathBuf, ProductionCommandError> {
    validate_local_case_insensitive_ntfs_directory(path)
        .map_err(|source| ProductionCommandError::construct("projects.root", source))
}

struct ProductionLoggedTranslation<R, P, E, S> {
    components: Mutex<Option<(R, P, E, S)>>,
    logging: ProductionRunLogResources,
}

impl<R, P, E, S> ProductionLoggedTranslation<R, P, E, S> {
    fn new(components: (R, P, E, S), logging: ProductionRunLogResources) -> Self {
        Self {
            components: Mutex::new(Some(components)),
            logging,
        }
    }
}

impl<R, P, E, S> StandardTranslation for ProductionLoggedTranslation<R, P, E, S>
where
    R: Send + 'static,
    P: Send + 'static,
    E: Send + 'static,
    S: Send + 'static,
    StandardTranslationService<R, P, E, S, TranslationJsonLinesEventLog>:
        StandardTranslation<Profile = ProductionTranslationProfile>,
{
    type Profile = ProductionTranslationProfile;
    type Error = ProductionLoggedOperationError;

    async fn run(
        &self,
        project: &StoredProjectRecord,
        profile: &Self::Profile,
        input: StandardTranslationInput,
    ) -> Result<StandardTranslationRunReport, Self::Error> {
        let (asset_reader, planner, executor, result_store) =
            self.components
                .lock()
                .await
                .take()
                .ok_or(ProductionLoggedOperationError::AlreadyExecuted)?;
        let run_id = WindowsRunIdGenerator
            .generate()
            .map_err(|source| ProductionLoggedOperationError::GenerateRunId(Box::new(source)))?;
        let context = TranslationRunLogContext::new(run_id, project.name().as_str(), profile.id());
        let (event_log, finalizer) = TranslationJsonLinesEventLog::start(
            self.logging.root.clone(),
            self.logging.configuration,
            context,
        )
        .map_err(|source| ProductionLoggedOperationError::StartLog(Box::new(source)))?;
        *self.logging.finalizer.lock().await = Some(finalizer);
        let service = StandardTranslationService::new(
            asset_reader,
            planner,
            executor,
            result_store,
            event_log,
            self.logging.cancellation.clone(),
        );
        service
            .run(project, profile, input)
            .await
            .map_err(|source| ProductionLoggedOperationError::Execute(Box::new(source)))
    }
}

struct ProductionLoggedWriteBack<R, L, D, P> {
    components: Mutex<Option<(R, L, D, P)>>,
    logging: ProductionRunLogResources,
}

impl<R, L, D, P> ProductionLoggedWriteBack<R, L, D, P> {
    fn new(components: (R, L, D, P), logging: ProductionRunLogResources) -> Self {
        Self {
            components: Mutex::new(Some(components)),
            logging,
        }
    }
}

impl<R, L, D, P> StandardWriteBack for ProductionLoggedWriteBack<R, L, D, P>
where
    R: Send + 'static,
    L: Send + 'static,
    D: Send + 'static,
    P: Send + 'static,
    StandardWriteBackService<R, L, D, P, WriteBackJsonLinesEventLog>: StandardWriteBack,
{
    type Error = ProductionLoggedOperationError;

    async fn run(
        &self,
        project: &OpenedProject,
        layout_profile: &MzWriteBackLayoutProfile,
    ) -> Result<StandardWriteBackReport, Self::Error> {
        let (asset_reader, layouter, rewriter, publisher) = self
            .components
            .lock()
            .await
            .take()
            .ok_or(ProductionLoggedOperationError::AlreadyExecuted)?;
        let run_id = WindowsRunIdGenerator
            .generate()
            .map_err(|source| ProductionLoggedOperationError::GenerateRunId(Box::new(source)))?;
        let context = WriteBackRunLogContext::new(run_id, project.name().as_str());
        let (event_log, finalizer) = WriteBackJsonLinesEventLog::start(
            self.logging.root.clone(),
            self.logging.configuration,
            context,
        )
        .map_err(|source| ProductionLoggedOperationError::StartLog(Box::new(source)))?;
        *self.logging.finalizer.lock().await = Some(finalizer);
        let service = StandardWriteBackService::new(
            asset_reader,
            layouter,
            rewriter,
            publisher,
            event_log,
            self.logging.cancellation.clone(),
        );
        service
            .run(project, layout_profile)
            .await
            .map_err(|source| ProductionLoggedOperationError::Execute(Box::new(source)))
    }
}

async fn finalize_log(
    slot: &EventLogFinalizerSlot,
    component: &'static str,
    shutdown: &mut ShutdownFailures,
) {
    if let Some(finalizer) = slot.lock().await.take()
        && let Err(error) = finalizer.finalize().await
    {
        shutdown.push(component, error);
    }
}

#[derive(Debug)]
enum ProductionLoggedOperationError {
    AlreadyExecuted,
    GenerateRunId(BoxedError),
    StartLog(BoxedError),
    Execute(BoxedError),
}

impl fmt::Display for ProductionLoggedOperationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyExecuted => formatter.write_str("生产 Standard 能力被重复执行"),
            Self::GenerateRunId(source) => write!(formatter, "无法生成运行身份：{source}"),
            Self::StartLog(source) => write!(formatter, "无法启动持久日志：{source}"),
            Self::Execute(source) => source.fmt(formatter),
        }
    }
}

impl Error for ProductionLoggedOperationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::AlreadyExecuted => None,
            Self::GenerateRunId(source) | Self::StartLog(source) | Self::Execute(source) => {
                Some(source.as_ref())
            }
        }
    }
}

#[derive(Debug)]
struct UnknownTranslationProfile {
    requested_id: String,
    available_ids: Vec<String>,
}

impl fmt::Display for UnknownTranslationProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "找不到 LLM Profile {}；可用 Profile：{}",
            self.requested_id,
            self.available_ids.join("、")
        )
    }
}

impl Error for UnknownTranslationProfile {}

struct Utf8ResourceError {
    path: PathBuf,
    resource: &'static str,
    valid_up_to: usize,
    error_len: Option<usize>,
}

impl fmt::Display for Utf8ResourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {} 不是有效 UTF-8",
            self.resource,
            self.path.display()
        )
    }
}

impl fmt::Debug for Utf8ResourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Utf8ResourceError")
            .field("path", &self.path)
            .field("resource", &self.resource)
            .field("valid_up_to", &self.valid_up_to)
            .field("error_len", &self.error_len)
            .finish()
    }
}

impl Error for Utf8ResourceError {}

fn build_cpu(
    configuration: &crate::application::config::CpuRuntimeConfiguration,
) -> Result<BoundedCpuExecutor, ProductionCommandError> {
    let configuration = CpuExecutorConfig::new(
        configuration.worker_threads().get(),
        configuration.queue_capacity().get(),
    )
    .map_err(|source| ProductionCommandError::construct("CPU", source))?;
    BoundedCpuExecutor::start(configuration)
        .map_err(|source| ProductionCommandError::construct("CPU", source))
}

fn build_lua(
    configuration: &crate::application::config::LuaRuntimeConfiguration,
) -> Result<TrustedLua54Runtime, ProductionCommandError> {
    TrustedLua54Runtime::new(
        TrustedLua54RuntimeConfiguration::new(
            configuration.worker_threads(),
            configuration.queue_capacity(),
            configuration.worker_stack_bytes(),
            configuration.memory_limit_bytes_per_vm(),
            configuration.cancel_check_instruction_interval(),
            configuration.max_error_bytes(),
        ),
        tokio::runtime::Handle::current(),
    )
    .map_err(|source| ProductionCommandError::construct("Lua", source))
}

fn build_file_system(
    configuration: &crate::application::config::FilesystemRuntimeConfiguration,
) -> Result<SystemFileSystem, ProductionCommandError> {
    let publisher = configuration.publisher();
    let publisher = DirectoryPublisherConfig::new(
        publisher.max_prepared_candidates().get(),
        publisher.max_candidate_entries().get(),
        publisher.max_candidate_depth().get(),
        u64::try_from(publisher.max_candidate_bytes().get())
            .expect("x86_64 usize 必须可表示为 u64"),
        u64::try_from(publisher.max_single_file_bytes().get())
            .expect("x86_64 usize 必须可表示为 u64"),
        publisher.max_recovery_artifacts_per_target().get(),
        publisher.target_lock_timeout(),
    )
    .map_err(|source| ProductionCommandError::construct("DirectoryPublisher", source))?;
    let configuration = SystemFileSystemConfig::new(
        configuration.worker_threads().get(),
        configuration.queue_capacity().get(),
        u64::try_from(configuration.max_read_bytes().get()).expect("x86_64 usize 必须可表示为 u64"),
        configuration.max_directory_entries().get(),
        publisher,
    )
    .map_err(|source| ProductionCommandError::construct("FileSystem", source))?;
    SystemFileSystem::new(configuration)
        .map_err(|source| ProductionCommandError::construct("FileSystem", source))
}

fn build_sqlite(
    configuration: &crate::application::config::SqliteRuntimeConfiguration,
) -> Result<RusqliteStorage, ProductionCommandError> {
    let journal_mode = match configuration.journal_mode() {
        ConfigSqliteJournalMode::Delete => SqliteJournalMode::Delete,
        ConfigSqliteJournalMode::Truncate => SqliteJournalMode::Truncate,
        ConfigSqliteJournalMode::Persist => SqliteJournalMode::Persist,
        ConfigSqliteJournalMode::Wal => SqliteJournalMode::Wal,
    };
    let synchronous = match configuration.synchronous() {
        ConfigSqliteSynchronous::Normal => SqliteSynchronous::Normal,
        ConfigSqliteSynchronous::Full => SqliteSynchronous::Full,
        ConfigSqliteSynchronous::Extra => SqliteSynchronous::Extra,
    };
    let configuration = RusqliteStorageConfiguration::new(
        configuration.short_worker_threads(),
        configuration.short_queue_capacity(),
        configuration.max_open_connections(),
        configuration.max_interactive_sessions(),
        configuration.interactive_open_queue_capacity(),
        configuration.interactive_command_queue_capacity(),
        configuration.worker_stack_bytes(),
        configuration.max_statement_bytes(),
        configuration.max_parameter_bytes(),
        configuration.max_rows_per_query(),
        configuration.max_result_bytes_per_query(),
        configuration.busy_timeout(),
        journal_mode,
        synchronous,
    )
    .map_err(|source| ProductionCommandError::construct("SQLite", source))?;
    RusqliteStorage::start(configuration)
        .map_err(|source| ProductionCommandError::construct("SQLite", source))
}

enum Controlled<T> {
    Completed(T),
    Interrupted(T),
    SignalFailed(io::Error),
}

impl<T> Controlled<T> {
    fn map<U>(self, map: impl FnOnce(T) -> U) -> Controlled<U> {
        match self {
            Self::Completed(value) => Controlled::Completed(map(value)),
            Self::Interrupted(value) => Controlled::Interrupted(map(value)),
            Self::SignalFailed(error) => Controlled::SignalFailed(error),
        }
    }
}

async fn await_termination_signal() -> io::Result<()> {
    let mut ctrl_c = tokio::signal::windows::ctrl_c()?;
    let mut ctrl_break = tokio::signal::windows::ctrl_break()?;
    select_termination_signal(ctrl_c.recv(), ctrl_break.recv()).await
}

async fn select_termination_signal<C, B>(ctrl_c: C, ctrl_break: B) -> io::Result<()>
where
    C: Future<Output = Option<()>>,
    B: Future<Output = Option<()>>,
{
    tokio::pin!(ctrl_c);
    tokio::pin!(ctrl_break);
    tokio::select! {
        biased;
        signal = &mut ctrl_c => signal.ok_or_else(|| io::Error::other("Ctrl-C 信号源意外关闭")),
        signal = &mut ctrl_break => signal.ok_or_else(|| io::Error::other("Ctrl-Break 信号源意外关闭")),
    }
}

async fn await_or_interrupt<T>(
    future: impl Future<Output = T>,
    cancellation: &CooperativeCancellation,
) -> Controlled<T> {
    tokio::pin!(future);
    tokio::select! {
        biased;
        signal = await_termination_signal() => match signal {
            Ok(()) => {
                cancellation.request();
                // 业务阶段边界停止派生新工作；已经接管的根操作和唯一候选终结仍需
                // 继续被驱动到明确终态。
                let result = future.await;
                Controlled::Interrupted(result)
            }
            Err(error) => {
                let _ = future.await;
                Controlled::SignalFailed(error)
            }
        },
        result = &mut future => Controlled::Completed(result),
    }
}

async fn await_or_interrupt_with_lua<T>(
    future: impl Future<Output = T>,
    cancellation: &CooperativeCancellation,
    lua: Option<&TrustedLua54Runtime>,
) -> (Controlled<T>, Option<TrustedLua54RuntimeError>) {
    tokio::pin!(future);
    tokio::select! {
        biased;
        signal = await_termination_signal() => match signal {
            Ok(()) => {
                cancellation.request();
                // Lua 根的 shutdown 会向 queued/running job 发出合作式取消，并等待
                // 唯一 finalizer。业务 Future 同时继续被驱动，使已接管的副作用能产生
                // 明确终态，又不会因高层 Future drop 丢失唯一终结权。
                let (result, cancel_error) = tokio::join!(
                    &mut future,
                    async {
                        match lua {
                            Some(lua) => lua.shutdown().await.err(),
                            None => None,
                        }
                    }
                );
                (Controlled::Interrupted(result), cancel_error)
            }
            Err(error) => {
                let _ = future.await;
                (Controlled::SignalFailed(error), None)
            }
        },
        result = &mut future => (Controlled::Completed(result), None),
    }
}

async fn await_or_interrupt_with_translation_roots<T>(
    future: impl Future<Output = T>,
    cancellation: &CooperativeCancellation,
    lua: Option<&TrustedLua54Runtime>,
    llm: &OpenAiChatCompletionExecutor,
) -> (Controlled<T>, Option<TrustedLua54RuntimeError>) {
    tokio::pin!(future);
    tokio::select! {
        biased;
        signal = await_termination_signal() => match signal {
            Ok(()) => {
                cancellation.request();
                // LLM 立即停止新准入；已活动 HTTP 与业务 Future 并发驱动到终态。
                // 因此 Ctrl-C 后不会继续发起新模型请求，也不会卡在等待一个
                // 已经因暂停 poll 而无法完成的活动请求上。
                let (result, cancel_error, ()) = tokio::join!(
                    &mut future,
                    async {
                        match lua {
                            Some(lua) => lua.shutdown().await.err(),
                            None => None,
                        }
                    },
                    llm.shutdown(),
                );
                (Controlled::Interrupted(result), cancel_error)
            }
            Err(error) => {
                let _ = future.await;
                (Controlled::SignalFailed(error), None)
            }
        },
        result = &mut future => (Controlled::Completed(result), None),
    }
}

pub(crate) struct ProductionCommandRunReport {
    pub(crate) command_result: Option<Result<MzCommandOutput, ProductionCommandError>>,
    pub(crate) shutdown_error: Option<ShutdownFailures>,
    pub(crate) interrupted: bool,
}

impl ProductionCommandRunReport {
    fn construction_failed(error: ProductionCommandError) -> Self {
        Self {
            command_result: Some(Err(error)),
            shutdown_error: None,
            interrupted: false,
        }
    }

    fn construction_failed_with_shutdown(
        error: ProductionCommandError,
        shutdown: ShutdownFailures,
    ) -> Self {
        Self {
            command_result: Some(Err(error)),
            shutdown_error: (!shutdown.is_empty()).then_some(shutdown),
            interrupted: false,
        }
    }

    fn from_controlled(
        controlled: Controlled<Result<MzCommandOutput, ProductionCommandError>>,
        shutdown: ShutdownFailures,
    ) -> Self {
        let shutdown_error = (!shutdown.is_empty()).then_some(shutdown);
        match controlled {
            Controlled::Completed(result) => Self {
                command_result: Some(result),
                shutdown_error,
                interrupted: false,
            },
            Controlled::Interrupted(result) => Self {
                command_result: match result {
                    Ok(_) => None,
                    Err(error) if error.is_expected_interruption() => None,
                    Err(error) => Some(Err(error)),
                },
                shutdown_error,
                interrupted: true,
            },
            Controlled::SignalFailed(source) => Self {
                command_result: Some(Err(ProductionCommandError::Signal(source))),
                shutdown_error,
                interrupted: false,
            },
        }
    }
}

#[derive(Debug)]
pub(crate) enum ProductionCommandError {
    Construct {
        component: &'static str,
        source: BoxedError,
    },
    Execute(BoxedError),
    Signal(io::Error),
}

impl ProductionCommandError {
    fn construct(component: &'static str, source: impl Error + Send + Sync + 'static) -> Self {
        Self::Construct {
            component,
            source: Box::new(source),
        }
    }

    fn execute(source: impl Error + Send + Sync + 'static) -> Self {
        Self::Execute(Box::new(source))
    }

    fn is_expected_interruption(&self) -> bool {
        let mut leaf: &(dyn Error + 'static) = self;
        while let Some(source) = leaf.source() {
            leaf = source;
        }
        leaf.downcast_ref::<crate::execution::OperationCancelled>()
            .is_some()
            || matches!(
                leaf.downcast_ref::<OpenAiChatCompletionError>(),
                Some(OpenAiChatCompletionError::ShuttingDown)
            )
    }
}

impl fmt::Display for ProductionCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Construct { component, source } => {
                write!(formatter, "无法构造 {component}：{source}")
            }
            Self::Execute(source) => source.fmt(formatter),
            Self::Signal(source) => write!(formatter, "无法监听 Ctrl-C：{source}"),
        }
    }
}

impl Error for ProductionCommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Construct { source, .. } | Self::Execute(source) => Some(source.as_ref()),
            Self::Signal(source) => Some(source),
        }
    }
}

#[derive(Default)]
pub(crate) struct ShutdownFailures {
    failures: Vec<ShutdownFailure>,
}

impl ShutdownFailures {
    fn push(&mut self, component: &'static str, error: impl fmt::Display) {
        self.failures.push(ShutdownFailure {
            component,
            message: error.to_string(),
        });
    }

    fn is_empty(&self) -> bool {
        self.failures.is_empty()
    }
}

struct ShutdownFailure {
    component: &'static str,
    message: String,
}

impl fmt::Display for ShutdownFailures {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, failure) in self.failures.iter().enumerate() {
            if index > 0 {
                formatter.write_str("；")?;
            }
            write!(formatter, "{}：{}", failure.component, failure.message)?;
        }
        Ok(())
    }
}

impl fmt::Debug for ShutdownFailures {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_list()
            .entries(
                self.failures
                    .iter()
                    .map(|failure| (failure.component, failure.message.as_str())),
            )
            .finish()
    }
}

impl Error for ShutdownFailures {}

/// 在命令和全部 shutdown 都成功后呈现最终业务结果。
pub(crate) struct CommandResultRenderer;

impl CommandResultRenderer {
    pub(crate) fn render_success(
        output: MzCommandOutput,
        stdout: &mut dyn Write,
    ) -> io::Result<()> {
        match output {
            MzCommandOutput::Init(output) => {
                writeln!(stdout, "初始化完成：{}", output.name)
            }
            MzCommandOutput::Extract(output) => {
                writeln!(stdout, "提取完成：{}", output.name)
            }
            MzCommandOutput::Translate(output) => {
                writeln!(
                    stdout,
                    "翻译执行完成：{}（Profile：{}）",
                    output.name, output.profile_id
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
                Ok(())
            }
            MzCommandOutput::WriteBack(output) => {
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
                )
            }
        }
    }

    /// 命令错误与 shutdown 错误分别呈现，避免后发生的清理失败覆盖业务首因。
    pub(crate) fn render_failure(
        command_error: Option<&dyn std::fmt::Display>,
        shutdown_error: Option<&dyn std::fmt::Display>,
        stderr: &mut dyn Write,
    ) -> io::Result<()> {
        if let Some(error) = command_error {
            writeln!(stderr, "命令失败：{error}")?;
        }
        if let Some(error) = shutdown_error {
            writeln!(stderr, "收尾失败：{error}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::att_mz::ProjectName;
    use crate::att_mz::translate::StandardTranslationSummary;
    use crate::att_mz::write_back::StandardWriteBackSummary;

    use super::*;

    #[test]
    fn normal_partial_translation_is_rendered_as_success() {
        let output = MzCommandOutput::Translate(TranslateOutput {
            name: project_name(),
            profile_id: "primary".to_owned(),
            standard: StandardTranslationSummary {
                total_tasks: 3,
                complete_tasks: 1,
                partial_tasks: 1,
                unavailable_tasks: 1,
                accepted_decisions: 4,
                written_locations: 5,
                remaining_decisions: 2,
                remaining_locations: 3,
                protocol_diagnostics: 1,
                recoverable_request_exhaustions: 1,
            },
            lua_executed: false,
        });
        let mut stdout = Vec::new();
        CommandResultRenderer::render_success(output, &mut stdout).expect("应呈现成功结果");
        let text = String::from_utf8(stdout).expect("输出应为 UTF-8");
        assert!(text.contains("部分 1，不可用 1"));
        assert!(!text.contains("失败"));
    }

    #[test]
    fn write_back_includes_manual_layout_diagnostic_without_failing() {
        let output = MzCommandOutput::WriteBack(WriteBackOutput {
            name: project_name(),
            output_root: PathBuf::from("C:/projects/demo/write_back"),
            standard: StandardWriteBackSummary {
                translated_locations: 4,
                original_locations: 2,
                auto_wrapped_units: 1,
                inserted_line_breaks: 2,
                inserted_fullwidth_indents: 3,
                manual_layout_units: 1,
            },
            lua_executed: true,
        });
        let mut stdout = Vec::new();
        CommandResultRenderer::render_success(output, &mut stdout).expect("应呈现成功结果");
        let text = String::from_utf8(stdout).expect("输出应为 UTF-8");
        assert!(text.contains("人工处理：1 段文本需要手动换行"));
        assert!(text.contains("Lua 写回：已执行"));
    }

    #[test]
    fn command_and_shutdown_failures_are_both_preserved() {
        let command = "数据库提交失败";
        let shutdown = "日志同步失败";
        let mut stderr = Vec::new();
        CommandResultRenderer::render_failure(Some(&command), Some(&shutdown), &mut stderr)
            .expect("应呈现两个错误");
        let text = String::from_utf8(stderr).expect("输出应为 UTF-8");
        assert!(text.contains(command));
        assert!(text.contains(shutdown));
    }

    #[test]
    fn interrupted_cancellation_is_suppressed_but_drained_technical_error_is_preserved() {
        let cancelled = ProductionCommandRunReport::from_controlled(
            Controlled::Interrupted(Err(ProductionCommandError::execute(
                crate::execution::OperationCancelled,
            ))),
            ShutdownFailures::default(),
        );
        assert!(cancelled.interrupted);
        assert!(cancelled.command_result.is_none());

        let technical = ProductionCommandRunReport::from_controlled(
            Controlled::Interrupted(Err(ProductionCommandError::execute(io::Error::other(
                "发布结果未知",
            )))),
            ShutdownFailures::default(),
        );
        assert!(technical.interrupted);
        let error = match technical.command_result.expect("技术终态必须保留") {
            Ok(_) => panic!("技术终态必须保持失败"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("发布结果未知"));

        let llm_shutdown = ProductionCommandRunReport::from_controlled(
            Controlled::Interrupted(Err(ProductionCommandError::execute(
                OpenAiChatCompletionError::ShuttingDown,
            ))),
            ShutdownFailures::default(),
        );
        assert!(
            llm_shutdown.command_result.is_none(),
            "Ctrl-C 主动关闭 LLM 的唯一叶子错误属于取消终态"
        );

        let other_llm_error = ProductionCommandRunReport::from_controlled(
            Controlled::Interrupted(Err(ProductionCommandError::execute(
                OpenAiChatCompletionError::AdmissionClosed,
            ))),
            ShutdownFailures::default(),
        );
        assert!(
            matches!(other_llm_error.command_result, Some(Err(_))),
            "其他 LLM 技术错误不得因同时收到 Ctrl-C 而吞掉"
        );
    }

    #[tokio::test]
    async fn either_windows_console_signal_requests_the_same_termination_path() {
        select_termination_signal(
            std::future::ready(Some(())),
            std::future::pending::<Option<()>>(),
        )
        .await
        .expect("Ctrl-C 应请求终止");

        select_termination_signal(
            std::future::pending::<Option<()>>(),
            std::future::ready(Some(())),
        )
        .await
        .expect("Ctrl-Break 应请求同一终止流程");
    }

    fn project_name() -> ProjectName {
        "demo".parse().expect("测试项目名应合法")
    }
}
