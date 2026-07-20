//! 生产命令装配与最终结果呈现。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::application::config::{
    ConfiguredExtractCommand, ConfiguredInitCommand, ConfiguredRpgMakerCommand,
    ConfiguredTranslateCommand, ConfiguredWriteBackCommand, TranslateConfiguration,
};
use crate::execution::{CooperativeCancellation, OperationCompletion};
use crate::rpg_maker::RpgMakerLayout;
use crate::rpg_maker::SelectedLua;
use crate::rpg_maker::audit::{
    AuditContext, AuditEvent, AuditFailureCategory, AuditLedger, AuditRunOutcome,
    JsonLinesAuditLedger, JsonLinesAuditRun,
};
use crate::rpg_maker::dialogue::{
    MvDialogueDefinition, MvDialogueDefinitionError, MvDialogueProjector,
};
use crate::rpg_maker::extract::builtin::{BuiltInExtractionService, MvDialogueDefinitionSelection};
use crate::rpg_maker::extract::document::RpgMakerProjectDocumentReadingService;
use crate::rpg_maker::extract::lua::LuaExtractionService;
use crate::rpg_maker::extract::rules::RulesExtractionService;
use crate::rpg_maker::extract::service::ExtractService;
use crate::rpg_maker::extract::service::ExtractServiceError;
use crate::rpg_maker::extract::store::asset_store::RpgMakerExtractionAssetStore;
use crate::rpg_maker::extract::{ExtractInput, ExtractOutput, SelectedRules};
use crate::rpg_maker::init::{
    InitInput, InitOutcome, InitOutput, InitService, InitServiceError, InitStaleOwner,
    ProjectWorkspaceConvergenceFailureImpact, ProjectWorkspaceConvergenceService,
};
use crate::rpg_maker::lua::hosting::TrustedLuaExecutionHostingService;
use crate::rpg_maker::lua::lua54::TrustedLua54Runtime;
use crate::rpg_maker::project::ExistingProjectOpeningService;
use crate::rpg_maker::project_database::{
    ProjectDatabaseCreationService, ProjectDatabaseRecordReadingService,
    ProjectDatabaseStateReconciliationService,
};
use crate::rpg_maker::project_lease::ProjectCommandLeaseService;
use crate::rpg_maker::translate::TranslateInput;
use crate::rpg_maker::translate::TranslateOutput;
use crate::rpg_maker::translate::asset_reader::RpgMakerStandardTranslationAssetReadingService;
use crate::rpg_maker::translate::executor::{
    AsyncDelay, RpgMakerStandardTranslationTaskExecutionService,
    TranslationTaskResponseProcessingService,
};
use crate::rpg_maker::translate::lua::LuaTranslationService;
use crate::rpg_maker::translate::placeholder::Pcre2PlaceholderService;
use crate::rpg_maker::translate::planner::RpgMakerStandardTranslationTaskPlanningService;
use crate::rpg_maker::translate::planning_resource::TranslationPlanningResourceReadingService;
use crate::rpg_maker::translate::profile::{
    ResolvedRpgMakerTranslationResources, RpgMakerSystemPrompt,
    RpgMakerTranslationPlanningConfiguration, RpgMakerTranslationProfile,
};
use crate::rpg_maker::translate::result_store::RpgMakerStandardTranslationResultStorageService;
use crate::rpg_maker::translate::service::{
    SelectedTranslationExecution, SelectedTranslationExecutionBuilder, TranslateService,
    TranslateServiceError,
};
use crate::rpg_maker::translate::standard::{
    StandardTranslationFailureImpact, StandardTranslationService,
};
use crate::rpg_maker::write_back::asset_reader::RpgMakerStandardWriteBackAssetReadingService;
use crate::rpg_maker::write_back::lua::LuaWriteBackService;
use crate::rpg_maker::write_back::publisher::StandardWriteBackPublishingService;
use crate::rpg_maker::write_back::rewriter::RpgMakerWriteBackDocumentRewritingService;
use crate::rpg_maker::write_back::standard::{
    ConservativeRpgMakerWriteBackTextLayouter, StandardWriteBackService,
};
use crate::rpg_maker::write_back::{
    WriteBackFailureImpact, WriteBackInput, WriteBackOutput, WriteBackService,
    WriteBackServiceError,
};
use crate::runtime::cpu::RayonCpuExecutor;
use crate::runtime::filesystem::{SystemFileSystem, SystemFileSystemError};
use crate::runtime::json_lines::{JsonLinesEventLogFinalizer, JsonLinesStreamConfig};
use crate::runtime::llm::{OpenAiChatCompletionClient, OpenAiChatCompletionExecutor};
use crate::runtime::run_id::generate_run_id;
use crate::runtime::sqlite::RusqliteStorage;
use crate::storage::file_system::{FileReader, ReadFileError};

type BoxedError = Box<dyn Error + Send + Sync + 'static>;
const RPG_MAKER_PROMPT_DIRECTORY_NAME: &str = "rpg_maker";
const MAX_MV_DIALOGUE_DEFINITION_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Default)]
struct TokioAsyncDelay;

impl AsyncDelay for TokioAsyncDelay {
    async fn wait(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

#[cfg(test)]
mod async_delay_tests {
    use std::time::Instant;

    use super::*;

    #[tokio::test]
    async fn waits_for_requested_duration() {
        let started = Instant::now();
        TokioAsyncDelay.wait(Duration::from_millis(5)).await;
        assert!(started.elapsed() >= Duration::from_millis(5));
    }
}

#[derive(Clone)]
struct ProductionLuaSelection {
    script_path: PathBuf,
    runtime: TrustedLua54Runtime,
}

/// 一个 RPG Maker 命令成功完成后的类型化结果。
pub(crate) enum RpgMakerCommandOutput {
    Init(InitOutput),
    Extract(ExtractOutput),
    Translate(TranslateOutput),
    WriteBack(WriteBackOutput),
}

/// 按本次命令只构造实际需要的 RPG Maker 生产纵向切片。
pub(crate) struct ProductionRpgMakerCommandRunner {
    layout: RpgMakerLayout,
}

impl ProductionRpgMakerCommandRunner {
    pub(crate) const fn new(layout: RpgMakerLayout) -> Self {
        Self { layout }
    }

    pub(crate) async fn run(
        self,
        command: ConfiguredRpgMakerCommand,
    ) -> ProductionCommandRunReport {
        match command {
            ConfiguredRpgMakerCommand::Init(command) => self.run_init(command).await,
            ConfiguredRpgMakerCommand::Extract(command) => self.run_extract(command).await,
            ConfiguredRpgMakerCommand::Translate(command) => self.run_translate(*command).await,
            ConfiguredRpgMakerCommand::WriteBack(command) => self.run_write_back(command).await,
        }
    }

    async fn run_init(self, command: ConfiguredInitCommand) -> ProductionCommandRunReport {
        let run_id = match generate_run_id() {
            Ok(value) => value,
            Err(source) => {
                return ProductionCommandRunReport::construction_failed(
                    ProductionCommandError::AuditLedger(Box::new(source)),
                );
            }
        };
        let audit = match start_audit(
            command.common().audit_root().to_path_buf(),
            command.common().audit(),
            AuditContext::init(
                run_id,
                self.layout.engine(),
                command.arguments.project.name.as_str(),
            ),
        )
        .await
        {
            Ok(value) => value,
            Err(report) => return report,
        };
        let cancellation = CooperativeCancellation::default();
        let projects_root = command.common().projects_root().to_path_buf();
        let sqlite = match RusqliteStorage::start(command.common().sqlite().clone())
            .map_err(ProductionCommandError::construct)
        {
            Ok(sqlite) => sqlite,
            Err(error) => {
                return audited_construction_failure(audit, error, ShutdownFailures::default())
                    .await;
            }
        };
        let file_system = match SystemFileSystem::new(command.common().filesystem().clone())
            .map_err(ProductionCommandError::construct)
        {
            Ok(file_system) => file_system,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                return audited_construction_failure(audit, error, shutdown).await;
            }
        };
        let directory_publisher = file_system.directory_publisher(command.publisher().clone());
        let database = ProjectDatabaseCreationService::new(sqlite.clone());
        let database_reconciler =
            ProjectDatabaseStateReconciliationService::new(sqlite.clone(), sqlite.clone());
        let workspace = ProjectWorkspaceConvergenceService::new(
            projects_root.clone(),
            self.layout,
            database,
            sqlite.clone(),
            database_reconciler,
            file_system.clone(),
            directory_publisher,
            cancellation.clone(),
        );
        let project_lease = ProjectCommandLeaseService::new(
            projects_root,
            self.layout.engine(),
            file_system.clone(),
        );
        let service = InitService::new(workspace, project_lease, cancellation.clone());
        let arguments = &command.arguments;
        let input = InitInput {
            name: arguments.project.name.clone(),
            game_root: arguments.path.clone(),
            source_language: arguments.source_language.clone(),
            target_language: arguments.target_language.clone(),
            dialogue_max_fullwidth_chars: arguments.dialogue_max_fullwidth_chars,
            scrolling_text_max_fullwidth_chars: arguments.scrolling_text_max_fullwidth_chars,
            help_description_max_fullwidth_chars: arguments.help_description_max_fullwidth_chars,
        };

        let execution = drive_command(service.execute(input), &cancellation)
            .await
            .map(|result| {
                result
                    .map_err(|error| map_init_error(error, |workspace| workspace.failure_impact()))
            });
        let mut shutdown = ShutdownFailures::default();
        if let Err(error) = sqlite.shutdown().await {
            shutdown.push("SQLite", error);
        }
        if let Err(error) = file_system.shutdown().await {
            shutdown.push("FileSystem", error);
        }
        let audit_outcome = audit_outcome(&execution, &shutdown);
        finish_audit(audit, audit_outcome, &mut shutdown).await;
        ProductionCommandRunReport::from_completion(
            execution.map(|result| {
                result.map(|completion| map_completion(completion, RpgMakerCommandOutput::Init))
            }),
            shutdown,
        )
    }

    async fn run_extract(self, command: ConfiguredExtractCommand) -> ProductionCommandRunReport {
        let run_id = match generate_run_id() {
            Ok(value) => value,
            Err(source) => {
                return ProductionCommandRunReport::construction_failed(
                    ProductionCommandError::AuditLedger(Box::new(source)),
                );
            }
        };
        let audit = match start_audit(
            command.common().audit_root().to_path_buf(),
            command.common().audit(),
            AuditContext::extract(
                run_id,
                self.layout.engine(),
                command.project_name().as_str(),
            ),
        )
        .await
        {
            Ok(value) => value,
            Err(report) => return report,
        };
        let cancellation = CooperativeCancellation::default();
        let projects_root = command.common().projects_root().to_path_buf();
        let sqlite = match RusqliteStorage::start(command.common().sqlite().clone())
            .map_err(ProductionCommandError::construct)
        {
            Ok(value) => value,
            Err(error) => {
                return audited_construction_failure(audit, error, ShutdownFailures::default())
                    .await;
            }
        };
        let file_system = match SystemFileSystem::new(command.common().filesystem().clone())
            .map_err(ProductionCommandError::construct)
        {
            Ok(value) => value,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                return audited_construction_failure(audit, error, shutdown).await;
            }
        };
        let mv_dialogue_selection = if self.layout == RpgMakerLayout::MV
            && command.rpg_maker().builtin()
        {
            match command.dialogue_rules_path() {
                Some(path) => match load_mv_dialogue_definition(&file_system, path).await {
                    Ok((definition, projector)) => Some(MvDialogueDefinitionSelection::Replace {
                        projector,
                        definition,
                    }),
                    Err(source) => {
                        let mut shutdown = ShutdownFailures::default();
                        if let Err(error) = sqlite.shutdown().await {
                            shutdown.push("SQLite", error);
                        }
                        if let Err(error) = file_system.shutdown().await {
                            shutdown.push("FileSystem", error);
                        }
                        return audited_construction_failure(
                            audit,
                            ProductionCommandError::ConfigurationOrInput(Box::new(source)),
                            shutdown,
                        )
                        .await;
                    }
                },
                None => Some(MvDialogueDefinitionSelection::ReuseProjectDefinition),
            }
        } else {
            None
        };
        let cpu = match RayonCpuExecutor::start(command.cpu())
            .map_err(ProductionCommandError::construct)
        {
            Ok(value) => value,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return audited_construction_failure(audit, error, shutdown).await;
            }
        };
        let lua = command.lua().map(|selected| ProductionLuaSelection {
            script_path: selected.script_path().to_path_buf(),
            runtime: TrustedLua54Runtime::new(
                selected.runtime(),
                tokio::runtime::Handle::current(),
            ),
        });

        let project_reader = ProjectDatabaseRecordReadingService::new(
            projects_root.clone(),
            self.layout,
            sqlite.clone(),
        );
        let opener = ExistingProjectOpeningService::new(
            project_reader,
            file_system.clone(),
            file_system.clone(),
        );
        let document_config = command.rpg_maker().document();
        let store_config = command.rpg_maker().extract_store();
        let builtin = command.rpg_maker().builtin().then(|| {
            let reader = RpgMakerProjectDocumentReadingService::new(
                file_system.clone(),
                file_system.clone(),
                cpu.clone(),
                document_config,
            );
            let store =
                RpgMakerExtractionAssetStore::new(sqlite.clone(), cpu.clone(), store_config);
            match mv_dialogue_selection {
                Some(selection) => {
                    BuiltInExtractionService::for_mv(reader, store, cpu.clone(), selection)
                }
                None => BuiltInExtractionService::new(reader, store, cpu.clone()),
            }
        });
        let selected_rules = command.rpg_maker().rules().map(|selected| {
            let reader = RpgMakerProjectDocumentReadingService::new(
                file_system.clone(),
                file_system.clone(),
                cpu.clone(),
                document_config,
            );
            let store =
                RpgMakerExtractionAssetStore::new(sqlite.clone(), cpu.clone(), store_config);
            SelectedRules::new(
                selected.rules_path().to_path_buf(),
                RulesExtractionService::new(file_system.clone(), reader, store, cpu.clone()),
            )
        });
        let selected_lua = lua.as_ref().map(|selected| {
                let host = TrustedLuaExecutionHostingService::<_, OpenAiChatCompletionExecutor, _, _>::without_llm(
                    file_system.clone(), selected.runtime.clone(), sqlite.clone(),
                );
                let store = RpgMakerExtractionAssetStore::new(sqlite.clone(), cpu.clone(), store_config);
                SelectedLua::new(
                    selected.script_path.clone(),
                    LuaExtractionService::new(host, store),
                )
        });
        let project_lease = ProjectCommandLeaseService::new(
            projects_root,
            self.layout.engine(),
            file_system.clone(),
        );
        let service = ExtractService::new(
            opener,
            builtin,
            selected_rules,
            selected_lua,
            project_lease,
            cancellation.clone(),
        );
        let input = ExtractInput {
            name: command.project_name().clone(),
        };
        let execution = drive_command(service.execute(input), &cancellation)
            .await
            .map(|result| result.map_err(map_extract_error));
        let mut shutdown = ShutdownFailures::default();
        if let Some(selected) = lua.as_ref()
            && let Err(source) = selected.runtime.shutdown().await
        {
            shutdown.push("Lua", source);
        }
        if let Err(source) = sqlite.shutdown().await {
            shutdown.push("SQLite", source);
        }
        if let Err(source) = file_system.shutdown().await {
            shutdown.push("FileSystem", source);
        }
        if let Err(source) = cpu.shutdown() {
            shutdown.push("CPU", source);
        }
        let audit_outcome = audit_outcome(&execution, &shutdown);
        finish_audit(audit, audit_outcome, &mut shutdown).await;
        ProductionCommandRunReport::from_completion(
            execution.map(|result| {
                result.map(|completion| map_completion(completion, RpgMakerCommandOutput::Extract))
            }),
            shutdown,
        )
    }

    async fn run_translate(
        self,
        command: ConfiguredTranslateCommand,
    ) -> ProductionCommandRunReport {
        let run_id = match generate_run_id() {
            Ok(value) => value,
            Err(source) => {
                return ProductionCommandRunReport::construction_failed(
                    ProductionCommandError::AuditLedger(Box::new(source)),
                );
            }
        };
        let audit = match start_audit(
            command.common().audit_root().to_path_buf(),
            command.common().audit(),
            AuditContext::translate(
                run_id,
                self.layout.engine(),
                command.project_name().as_str(),
                command.rpg_maker().profile().id(),
            ),
        )
        .await
        {
            Ok(value) => value,
            Err(report) => return report,
        };
        let cancellation = CooperativeCancellation::default();
        let projects_root = command.common().projects_root().to_path_buf();
        let file_system = match SystemFileSystem::new(command.common().filesystem().clone())
            .map_err(ProductionCommandError::construct)
        {
            Ok(value) => value,
            Err(error) => {
                return audited_construction_failure(audit, error, ShutdownFailures::default())
                    .await;
            }
        };
        let additional_pem_roots =
            match load_additional_pem_roots(&file_system, command.llm()).await {
                Ok(value) => value,
                Err(error) => {
                    let mut shutdown = ShutdownFailures::default();
                    if let Err(source) = file_system.shutdown().await {
                        shutdown.push("FileSystem", source);
                    }
                    return audited_construction_failure(audit, error, shutdown).await;
                }
            };
        let llm = match OpenAiChatCompletionExecutor::new(
            command.llm().with_pem_roots(additional_pem_roots),
        )
        .map_err(ProductionCommandError::construct)
        {
            Ok(value) => value,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return audited_construction_failure(audit, error, shutdown).await;
            }
        };
        let sqlite = match RusqliteStorage::start(command.common().sqlite().clone())
            .map_err(ProductionCommandError::construct)
        {
            Ok(value) => value,
            Err(error) => {
                llm.shutdown().await;
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return audited_construction_failure(audit, error, shutdown).await;
            }
        };
        let cpu = match RayonCpuExecutor::start(command.cpu())
            .map_err(ProductionCommandError::construct)
        {
            Ok(value) => value,
            Err(error) => {
                llm.shutdown().await;
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return audited_construction_failure(audit, error, shutdown).await;
            }
        };
        let lua = command.lua().map(|selected| ProductionLuaSelection {
            script_path: selected.script_path().to_path_buf(),
            runtime: TrustedLua54Runtime::new(
                selected.runtime(),
                tokio::runtime::Handle::current(),
            ),
        });
        let project_reader = ProjectDatabaseRecordReadingService::new(
            projects_root.clone(),
            self.layout,
            sqlite.clone(),
        );
        let opener = ExistingProjectOpeningService::new(
            project_reader,
            file_system.clone(),
            file_system.clone(),
        );
        let builder = ProductionSelectedTranslationExecutionBuilder {
            configuration: command.rpg_maker(),
            file_system: file_system.clone(),
            cpu: cpu.clone(),
            sqlite: sqlite.clone(),
            llm: llm.clone(),
            lua: lua.clone(),
            audit: audit.run.clone(),
            cancellation: cancellation.clone(),
        };
        let project_lease = ProjectCommandLeaseService::new(
            projects_root,
            self.layout.engine(),
            file_system.clone(),
        );
        let service = TranslateService::new(opener, builder, project_lease, cancellation.clone());
        let input = TranslateInput {
            name: command.project_name().clone(),
            terminology_path: command.terminology_path().map(Path::to_path_buf),
            placeholder_rules_path: command.placeholder_rules_path().map(Path::to_path_buf),
        };
        let execution = drive_command(service.execute(input), &cancellation)
            .await
            .map(|result| {
                result.map_err(|error| {
                    map_translate_error(
                        error,
                        ProductionTranslationExecutionBuildError::failure_impact,
                        |standard| standard.failure_impact(),
                    )
                })
            });
        let mut shutdown = ShutdownFailures::default();
        if let Some(selected) = lua.as_ref()
            && let Err(source) = selected.runtime.shutdown().await
        {
            shutdown.push("Lua", source);
        }
        llm.shutdown().await;
        if let Err(source) = sqlite.shutdown().await {
            shutdown.push("SQLite", source);
        }
        if let Err(source) = file_system.shutdown().await {
            shutdown.push("FileSystem", source);
        }
        if let Err(source) = cpu.shutdown() {
            shutdown.push("CPU", source);
        }
        let audit_outcome = audit_outcome(&execution, &shutdown);
        finish_audit(audit, audit_outcome, &mut shutdown).await;
        ProductionCommandRunReport::from_completion(
            execution.map(|result| {
                result
                    .map(|completion| map_completion(completion, RpgMakerCommandOutput::Translate))
            }),
            shutdown,
        )
    }

    async fn run_write_back(
        self,
        command: ConfiguredWriteBackCommand,
    ) -> ProductionCommandRunReport {
        let run_id = match generate_run_id() {
            Ok(value) => value,
            Err(source) => {
                return ProductionCommandRunReport::construction_failed(
                    ProductionCommandError::AuditLedger(Box::new(source)),
                );
            }
        };
        let audit = match start_audit(
            command.common().audit_root().to_path_buf(),
            command.common().audit(),
            AuditContext::write_back(
                run_id,
                self.layout.engine(),
                command.project_name().as_str(),
            ),
        )
        .await
        {
            Ok(value) => value,
            Err(report) => return report,
        };
        let cancellation = CooperativeCancellation::default();
        let projects_root = command.common().projects_root().to_path_buf();
        let file_system = match SystemFileSystem::new(command.common().filesystem().clone())
            .map_err(ProductionCommandError::construct)
        {
            Ok(value) => value,
            Err(error) => {
                return audited_construction_failure(audit, error, ShutdownFailures::default())
                    .await;
            }
        };
        let sqlite = match RusqliteStorage::start(command.common().sqlite().clone())
            .map_err(ProductionCommandError::construct)
        {
            Ok(value) => value,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return audited_construction_failure(audit, error, shutdown).await;
            }
        };
        let cpu = match RayonCpuExecutor::start(command.cpu())
            .map_err(ProductionCommandError::construct)
        {
            Ok(value) => value,
            Err(error) => {
                let mut shutdown = ShutdownFailures::default();
                if let Err(source) = sqlite.shutdown().await {
                    shutdown.push("SQLite", source);
                }
                if let Err(source) = file_system.shutdown().await {
                    shutdown.push("FileSystem", source);
                }
                return audited_construction_failure(audit, error, shutdown).await;
            }
        };
        let lua = command.lua().map(|selected| ProductionLuaSelection {
            script_path: selected.script_path().to_path_buf(),
            runtime: TrustedLua54Runtime::new(
                selected.runtime(),
                tokio::runtime::Handle::current(),
            ),
        });
        let directory_publisher = file_system.directory_publisher(command.publisher().clone());
        let project_reader = ProjectDatabaseRecordReadingService::new(
            projects_root.clone(),
            self.layout,
            sqlite.clone(),
        );
        let opener = ExistingProjectOpeningService::new(
            project_reader,
            file_system.clone(),
            file_system.clone(),
        );
        let asset_reader = RpgMakerStandardWriteBackAssetReadingService::new(
            sqlite.clone(),
            cpu.clone(),
            command.rpg_maker().standard_asset(),
        );
        let document_reader = RpgMakerProjectDocumentReadingService::new(
            file_system.clone(),
            file_system.clone(),
            cpu.clone(),
            command.rpg_maker().document(),
        );
        let rewriter = RpgMakerWriteBackDocumentRewritingService::new(document_reader, cpu.clone());
        let standard = StandardWriteBackService::new(
            asset_reader,
            ConservativeRpgMakerWriteBackTextLayouter,
            rewriter,
            cpu.clone(),
            cancellation.clone(),
        );
        let publisher = StandardWriteBackPublishingService::new(directory_publisher.clone());
        let selected_lua = lua.as_ref().map(|selected| {
                let host = TrustedLuaExecutionHostingService::<_, OpenAiChatCompletionExecutor, _, _>::without_llm(
                    file_system.clone(), selected.runtime.clone(), sqlite.clone(),
                );
                SelectedLua::new(
                    selected.script_path.clone(),
                    LuaWriteBackService::new(host, directory_publisher),
                )
        });
        let project_lease = ProjectCommandLeaseService::new(
            projects_root,
            self.layout.engine(),
            file_system.clone(),
        );
        let service = WriteBackService::new(
            opener,
            standard,
            publisher,
            selected_lua,
            audit.run.clone(),
            project_lease,
            cancellation.clone(),
        );
        let input = WriteBackInput {
            name: command.project_name().clone(),
        };
        let execution = drive_command(service.execute(input), &cancellation)
            .await
            .map(|result| result.map_err(map_write_back_error));
        let mut shutdown = ShutdownFailures::default();
        if let Some(selected) = lua.as_ref()
            && let Err(source) = selected.runtime.shutdown().await
        {
            shutdown.push("Lua", source);
        }
        if let Err(source) = sqlite.shutdown().await {
            shutdown.push("SQLite", source);
        }
        if let Err(source) = file_system.shutdown().await {
            shutdown.push("FileSystem", source);
        }
        if let Err(source) = cpu.shutdown() {
            shutdown.push("CPU", source);
        }
        let audit_outcome = audit_outcome(&execution, &shutdown);
        finish_audit(audit, audit_outcome, &mut shutdown).await;
        ProductionCommandRunReport::from_completion(
            execution.map(|result| {
                result
                    .map(|completion| map_completion(completion, RpgMakerCommandOutput::WriteBack))
            }),
            shutdown,
        )
    }
}

async fn load_mv_dialogue_definition(
    file_system: &SystemFileSystem,
    path: &Path,
) -> Result<(MvDialogueDefinition, MvDialogueProjector), MvDialogueDefinitionInputError> {
    let requested_path = path.to_path_buf();
    let file = file_system
        .read_file(requested_path.clone())
        .await
        .map_err(|source| MvDialogueDefinitionInputError::Read {
            path: requested_path.clone(),
            source,
        })?;
    let bytes = file.into_bytes();
    if bytes.len() > MAX_MV_DIALOGUE_DEFINITION_BYTES {
        return Err(MvDialogueDefinitionInputError::TooLarge {
            path: requested_path,
            actual: bytes.len(),
            maximum: MAX_MV_DIALOGUE_DEFINITION_BYTES,
        });
    }
    let source = std::str::from_utf8(&bytes).map_err(|source| {
        MvDialogueDefinitionInputError::InvalidUtf8 {
            path: requested_path.clone(),
            source,
        }
    })?;
    let definition = MvDialogueDefinition::parse_toml(source).map_err(|source| {
        MvDialogueDefinitionInputError::InvalidDefinition {
            path: requested_path.clone(),
            source,
        }
    })?;
    let projector = definition.compile().map_err(|source| {
        MvDialogueDefinitionInputError::InvalidDefinition {
            path: requested_path,
            source,
        }
    })?;
    Ok((definition, projector))
}

#[derive(Debug)]
enum MvDialogueDefinitionInputError {
    Read {
        path: PathBuf,
        source: ReadFileError<SystemFileSystemError>,
    },
    TooLarge {
        path: PathBuf,
        actual: usize,
        maximum: usize,
    },
    InvalidUtf8 {
        path: PathBuf,
        source: std::str::Utf8Error,
    },
    InvalidDefinition {
        path: PathBuf,
        source: MvDialogueDefinitionError,
    },
}

impl fmt::Display for MvDialogueDefinitionInputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(
                    formatter,
                    "无法读取 MV 对话定义 {}：{source}",
                    path.display()
                )
            }
            Self::TooLarge {
                path,
                actual,
                maximum,
            } => write!(
                formatter,
                "MV 对话定义 {} 过大：实际 {actual} 字节，上限 {maximum} 字节",
                path.display(),
            ),
            Self::InvalidUtf8 { path, source } => write!(
                formatter,
                "MV 对话定义 {} 不是有效 UTF-8：{source}",
                path.display(),
            ),
            Self::InvalidDefinition { path, source } => {
                write!(formatter, "MV 对话定义 {} 无效：{source}", path.display(),)
            }
        }
    }
}

impl Error for MvDialogueDefinitionInputError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::InvalidUtf8 { source, .. } => Some(source),
            Self::InvalidDefinition { source, .. } => Some(source),
            Self::TooLarge { .. } => None,
        }
    }
}

struct ActiveAudit {
    run: JsonLinesAuditRun,
    finalizer: JsonLinesEventLogFinalizer,
}

async fn start_audit(
    root: PathBuf,
    configuration: JsonLinesStreamConfig,
    context: AuditContext,
) -> Result<ActiveAudit, ProductionCommandRunReport> {
    let (ledger, finalizer) =
        JsonLinesAuditLedger::start(root, configuration).map_err(|source| {
            ProductionCommandRunReport::construction_failed(ProductionCommandError::AuditLedger(
                Box::new(source),
            ))
        })?;
    let run = ledger.bind(context);
    if let Err(source) = run.append(AuditEvent::RunStarted).await {
        let mut shutdown = ShutdownFailures::default();
        if let Err(finalization) = finalizer.finalize().await {
            shutdown.push("审计账本", finalization);
        }
        return Err(
            ProductionCommandRunReport::construction_failed_with_shutdown(
                ProductionCommandError::AuditLedger(Box::new(source)),
                shutdown,
            ),
        );
    }
    Ok(ActiveAudit { run, finalizer })
}

async fn finish_audit(
    audit: ActiveAudit,
    outcome: AuditRunOutcome,
    shutdown: &mut ShutdownFailures,
) {
    if let Err(source) = audit.run.append(AuditEvent::RunFinished { outcome }).await {
        shutdown.push("审计账本", source);
    }
    if let Err(source) = audit.finalizer.finalize().await {
        shutdown.push("审计账本", source);
    }
}

async fn audited_construction_failure(
    audit: ActiveAudit,
    error: ProductionCommandError,
    mut shutdown: ShutdownFailures,
) -> ProductionCommandRunReport {
    let category = error.audit_category();
    finish_audit(audit, AuditRunOutcome::Failed(category), &mut shutdown).await;
    ProductionCommandRunReport::construction_failed_with_shutdown(error, shutdown)
}

type ProductionTranslationProfile = Arc<RpgMakerTranslationProfile<OpenAiChatCompletionClient>>;
type ProductionTranslationAssetReader =
    RpgMakerStandardTranslationAssetReadingService<RusqliteStorage, RayonCpuExecutor>;
type ProductionTranslationPlanner = RpgMakerStandardTranslationTaskPlanningService<
    TranslationPlanningResourceReadingService<SystemFileSystem, RayonCpuExecutor>,
    RayonCpuExecutor,
    OpenAiChatCompletionClient,
>;
type ProductionTranslationExecutor = RpgMakerStandardTranslationTaskExecutionService<
    OpenAiChatCompletionExecutor,
    TokioAsyncDelay,
    TranslationTaskResponseProcessingService<RayonCpuExecutor>,
    ProductionTranslationProfile,
>;
type ProductionTranslationStore =
    RpgMakerStandardTranslationResultStorageService<RusqliteStorage, RayonCpuExecutor>;
type ProductionStandardTranslation = StandardTranslationService<
    ProductionTranslationAssetReader,
    ProductionTranslationPlanner,
    ProductionTranslationExecutor,
    ProductionTranslationStore,
    JsonLinesAuditRun,
>;
type ProductionLuaHost = TrustedLuaExecutionHostingService<
    SystemFileSystem,
    OpenAiChatCompletionExecutor,
    TrustedLua54Runtime,
    RusqliteStorage,
>;
type ProductionLuaTranslation = LuaTranslationService<ProductionLuaHost>;

struct ProductionSelectedTranslationExecutionBuilder<'a> {
    configuration: &'a TranslateConfiguration,
    file_system: SystemFileSystem,
    cpu: RayonCpuExecutor,
    sqlite: RusqliteStorage,
    llm: OpenAiChatCompletionExecutor,
    lua: Option<ProductionLuaSelection>,
    audit: JsonLinesAuditRun,
    cancellation: CooperativeCancellation,
}

impl SelectedTranslationExecutionBuilder for ProductionSelectedTranslationExecutionBuilder<'_> {
    type Client = OpenAiChatCompletionClient;
    type Standard = ProductionStandardTranslation;
    type Lua = ProductionLuaTranslation;
    type Error = ProductionTranslationExecutionBuildError;

    async fn build(
        &self,
        project: &crate::rpg_maker::project::OpenedProject,
    ) -> Result<SelectedTranslationExecution<Self::Client, Self::Standard, Self::Lua>, Self::Error>
    {
        let profile_configuration = self.configuration.profile();
        let language_pair = project.language_pair().clone();
        let path = self
            .configuration
            .prompt_root()
            .join(RPG_MAKER_PROMPT_DIRECTORY_NAME)
            .join(format!(
                "{}--{}.md",
                language_pair.source(),
                language_pair.target()
            ));
        let file = self
            .file_system
            .read_file(path.clone())
            .await
            .map_err(|source| {
                ProductionTranslationExecutionBuildError::prompt(
                    &language_pair,
                    &path,
                    "无法读取普通文件",
                    source,
                )
            })?;
        if file.resolved_path().file_name() != path.file_name() {
            return Err(ProductionTranslationExecutionBuildError::prompt(
                &language_pair,
                &path,
                "文件名与规范语言对不精确匹配",
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "固定后的 Prompt 文件名与规范语言对派生文件名不一致",
                ),
            ));
        }
        let markdown = String::from_utf8(file.into_bytes()).map_err(|source| {
            let utf8 = source.utf8_error();
            ProductionTranslationExecutionBuildError::prompt(
                &language_pair,
                &path,
                "文件不是合法 UTF-8",
                Utf8ResourceError {
                    path: path.clone(),
                    valid_up_to: utf8.valid_up_to(),
                    error_len: utf8.error_len(),
                },
            )
        })?;
        let system_prompt =
            RpgMakerSystemPrompt::new(language_pair.clone(), markdown).map_err(|source| {
                ProductionTranslationExecutionBuildError::prompt(
                    &language_pair,
                    &path,
                    "内容为空白",
                    source,
                )
            })?;
        let source_language = self
            .configuration
            .language_modules()
            .resolve(language_pair.source())
            .map_err(|source| {
                ProductionTranslationExecutionBuildError::language_module(&language_pair, source)
            })?;
        let translation_resources = Arc::new(ResolvedRpgMakerTranslationResources::new(
            system_prompt,
            source_language,
        ));
        let planning = RpgMakerTranslationPlanningConfiguration::new(
            profile_configuration.planning().max_message_characters(),
        );
        let profile = Arc::new(RpgMakerTranslationProfile::new(
            profile_configuration.id(),
            profile_configuration.max_in_flight_tasks(),
            planning,
            profile_configuration.request().clone(),
            Arc::clone(self.configuration.client()),
        ));
        let placeholders = Pcre2PlaceholderService::new()
            .map_err(ProductionTranslationExecutionBuildError::internal)?;
        let asset_reader = RpgMakerStandardTranslationAssetReadingService::new(
            self.sqlite.clone(),
            self.cpu.clone(),
            self.configuration.standard_asset(),
        );
        let resources = TranslationPlanningResourceReadingService::new(
            self.file_system.clone(),
            self.cpu.clone(),
        );
        let planner = RpgMakerStandardTranslationTaskPlanningService::<
            _,
            _,
            OpenAiChatCompletionClient,
        >::new(
            resources,
            Arc::clone(&translation_resources),
            placeholders,
            self.cpu.clone(),
        );
        let processor =
            TranslationTaskResponseProcessingService::new(self.cpu.clone(), translation_resources);
        let executor = RpgMakerStandardTranslationTaskExecutionService::<
            _,
            _,
            _,
            ProductionTranslationProfile,
        >::new(self.llm.clone(), TokioAsyncDelay, processor);
        let result_store = RpgMakerStandardTranslationResultStorageService::new(
            self.sqlite.clone(),
            self.cpu.clone(),
            self.configuration.translate_store(),
        );
        let standard = StandardTranslationService::new(
            asset_reader,
            planner,
            executor,
            result_store,
            self.audit.clone(),
            self.cancellation.clone(),
        );
        let lua = self.lua.as_ref().map(|selected| {
            let host = TrustedLuaExecutionHostingService::with_llm(
                self.file_system.clone(),
                self.llm.clone(),
                selected.runtime.clone(),
                self.sqlite.clone(),
            );
            SelectedLua::new(
                selected.script_path.clone(),
                LuaTranslationService::new(host),
            )
        });
        Ok(SelectedTranslationExecution::new(profile, standard, lua))
    }
}

struct ProductionTranslationExecutionBuildError {
    impact: TranslationExecutionBuildFailureImpact,
    safe_detail: String,
    source: BoxedError,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TranslationExecutionBuildFailureImpact {
    ConfigurationOrInput,
    Internal,
}

impl ProductionTranslationExecutionBuildError {
    fn prompt(
        language_pair: &crate::language::LanguagePair,
        path: &Path,
        reason: &str,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            impact: TranslationExecutionBuildFailureImpact::ConfigurationOrInput,
            safe_detail: format!(
                "RPG Maker Prompt {} -> {}（{}）：{reason}",
                language_pair.source(),
                language_pair.target(),
                path.display()
            ),
            source: Box::new(source),
        }
    }

    fn language_module(
        language_pair: &crate::language::LanguagePair,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            impact: TranslationExecutionBuildFailureImpact::ConfigurationOrInput,
            safe_detail: format!(
                "RPG Maker 翻译语言对 {} -> {}：缺少源语言模块",
                language_pair.source(),
                language_pair.target()
            ),
            source: Box::new(source),
        }
    }

    fn internal(source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            impact: TranslationExecutionBuildFailureImpact::Internal,
            safe_detail: "无法建立当前项目语言对的翻译执行上下文".to_owned(),
            source: Box::new(source),
        }
    }

    const fn failure_impact(&self) -> TranslationExecutionBuildFailureImpact {
        self.impact
    }
}

impl fmt::Debug for ProductionTranslationExecutionBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProductionTranslationExecutionBuildError")
            .field("impact", &self.impact)
            .field("safe_detail", &self.safe_detail)
            .field("source", &self.source)
            .finish()
    }
}

impl fmt::Display for ProductionTranslationExecutionBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.safe_detail)
    }
}

impl Error for ProductionTranslationExecutionBuildError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

struct Utf8ResourceError {
    path: PathBuf,
    valid_up_to: usize,
    error_len: Option<usize>,
}

impl fmt::Debug for Utf8ResourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Utf8ResourceError")
            .field("path", &self.path)
            .field("valid_up_to", &self.valid_up_to)
            .field("error_len", &self.error_len)
            .finish()
    }
}

impl fmt::Display for Utf8ResourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "系统提示词文件不是有效 UTF-8：{}",
            self.path.display()
        )
    }
}

impl Error for Utf8ResourceError {}

async fn load_additional_pem_roots(
    file_system: &SystemFileSystem,
    configuration: &crate::application::config::SelectedLlmExecutorConfiguration,
) -> Result<Vec<Vec<u8>>, ProductionCommandError> {
    let mut roots = Vec::with_capacity(configuration.additional_pem_files().len());
    for path in configuration.additional_pem_files() {
        let file = file_system
            .read_file(path.to_path_buf())
            .await
            .map_err(ProductionCommandError::construct)?;
        roots.push(file.into_bytes());
    }
    Ok(roots)
}

enum DrivenCommand<T> {
    Finished(T),
    Interrupted(T),
    SignalFailed { source: io::Error, result: T },
}

impl<T> DrivenCommand<T> {
    fn map<U>(self, map: impl FnOnce(T) -> U) -> DrivenCommand<U> {
        match self {
            Self::Finished(value) => DrivenCommand::Finished(map(value)),
            Self::Interrupted(value) => DrivenCommand::Interrupted(map(value)),
            Self::SignalFailed { source, result } => DrivenCommand::SignalFailed {
                source,
                result: map(result),
            },
        }
    }
}

async fn await_termination_signal() -> io::Result<()> {
    let mut ctrl_c = tokio::signal::windows::ctrl_c()?;
    let mut ctrl_break = tokio::signal::windows::ctrl_break()?;
    tokio::select! {
        signal = ctrl_c.recv() => signal.ok_or_else(|| io::Error::other("Ctrl-C 信号源意外关闭")),
        signal = ctrl_break.recv() => signal.ok_or_else(|| io::Error::other("Ctrl-Break 信号源意外关闭")),
    }
}

async fn drive_command<T>(
    future: impl Future<Output = T>,
    cancellation: &CooperativeCancellation,
) -> DrivenCommand<T> {
    tokio::pin!(future);
    tokio::select! {
        biased;
        signal = await_termination_signal() => match signal {
            Ok(()) => {
                cancellation.request();
                DrivenCommand::Interrupted(future.await)
            }
            Err(error) => {
                let result = future.await;
                DrivenCommand::SignalFailed { source: error, result }
            }
        },
        result = &mut future => DrivenCommand::Finished(result),
    }
}

pub(crate) struct ProductionCommandRunReport {
    pub(crate) result: CommandRunResult,
    pub(crate) shutdown_error: Option<ShutdownFailures>,
}

pub(crate) enum CommandRunResult {
    Succeeded(RpgMakerCommandOutput),
    Interrupted,
    Failed(ProductionCommandError),
}

impl ProductionCommandRunReport {
    fn construction_failed(error: ProductionCommandError) -> Self {
        Self {
            result: CommandRunResult::Failed(error),
            shutdown_error: None,
        }
    }

    fn construction_failed_with_shutdown(
        error: ProductionCommandError,
        shutdown: ShutdownFailures,
    ) -> Self {
        Self {
            result: CommandRunResult::Failed(error),
            shutdown_error: (!shutdown.is_empty()).then_some(shutdown),
        }
    }

    fn from_completion(
        execution: DrivenCommand<
            Result<OperationCompletion<RpgMakerCommandOutput>, ProductionCommandError>,
        >,
        shutdown: ShutdownFailures,
    ) -> Self {
        let shutdown_error = (!shutdown.is_empty()).then_some(shutdown);
        match execution {
            DrivenCommand::Finished(Ok(OperationCompletion::Completed(output))) => Self {
                result: CommandRunResult::Succeeded(output),
                shutdown_error,
            },
            DrivenCommand::Finished(Ok(OperationCompletion::Cancelled))
            | DrivenCommand::Interrupted(Ok(_)) => Self {
                result: CommandRunResult::Interrupted,
                shutdown_error,
            },
            DrivenCommand::Finished(Err(error)) => Self {
                result: CommandRunResult::Failed(error),
                shutdown_error,
            },
            DrivenCommand::Interrupted(Err(error)) => Self {
                result: CommandRunResult::Failed(error),
                shutdown_error,
            },
            DrivenCommand::SignalFailed { source, result } => {
                let outcome = match result {
                    Ok(OperationCompletion::Completed(_)) => SignalOutcome::CompletedStateApplied,
                    Ok(OperationCompletion::Cancelled) => SignalOutcome::Cancelled,
                    Err(error) => SignalOutcome::CommandFailed(Box::new(error)),
                };
                Self {
                    result: CommandRunResult::Failed(ProductionCommandError::Signal {
                        source,
                        outcome,
                    }),
                    shutdown_error,
                }
            }
        }
    }
}

fn map_completion<T, U>(
    completion: OperationCompletion<T>,
    map: impl FnOnce(T) -> U,
) -> OperationCompletion<U> {
    match completion {
        OperationCompletion::Completed(value) => OperationCompletion::Completed(map(value)),
        OperationCompletion::Cancelled => OperationCompletion::Cancelled,
    }
}

fn map_init_error<W, P>(
    error: InitServiceError<W, P>,
    workspace_impact: impl FnOnce(&W) -> ProjectWorkspaceConvergenceFailureImpact,
) -> ProductionCommandError
where
    W: Error + Send + Sync + 'static,
    P: Error + Send + Sync + 'static,
{
    match error {
        error @ InitServiceError::ProjectLease(_) => {
            ProductionCommandError::ProjectUnavailable(Box::new(error))
        }
        error @ InitServiceError::Workspace(_) => match &error {
            InitServiceError::Workspace(source) => match workspace_impact(source) {
                ProjectWorkspaceConvergenceFailureImpact::ConfigurationOrInput => {
                    ProductionCommandError::ConfigurationOrInput(Box::new(error))
                }
                ProjectWorkspaceConvergenceFailureImpact::ProjectState => {
                    ProductionCommandError::ProjectState(Box::new(error))
                }
                ProjectWorkspaceConvergenceFailureImpact::StateAppliedButFinalizationFailed => {
                    ProductionCommandError::StateAppliedButFinalizationFailed(Box::new(error))
                }
                ProjectWorkspaceConvergenceFailureImpact::OutcomeUnknown => {
                    ProductionCommandError::OutcomeUnknown(Box::new(error))
                }
                ProjectWorkspaceConvergenceFailureImpact::Internal => {
                    ProductionCommandError::Internal(Box::new(error))
                }
            },
            _ => unreachable!("当前分支已经确认是工作区收敛错误"),
        },
    }
}

fn map_extract_error<OE, BE, RE, LE, PE>(
    error: ExtractServiceError<OE, BE, RE, LE, PE>,
) -> ProductionCommandError
where
    OE: Error + Send + Sync + 'static,
    BE: Error + Send + Sync + 'static,
    RE: Error + Send + Sync + 'static,
    LE: Error + Send + Sync + 'static,
    PE: Error + Send + Sync + 'static,
{
    match error {
        error @ ExtractServiceError::ProjectLease(_) => {
            ProductionCommandError::ProjectUnavailable(Box::new(error))
        }
        error @ (ExtractServiceError::OpenProject(_)
        | ExtractServiceError::BuiltIn(_)
        | ExtractServiceError::Rules { .. }
        | ExtractServiceError::Lua { .. }) => ProductionCommandError::ProjectState(Box::new(error)),
    }
}

fn map_translate_error<RE, BE, SE, LE, PE>(
    error: TranslateServiceError<RE, BE, SE, LE, PE>,
    build_impact: impl FnOnce(&BE) -> TranslationExecutionBuildFailureImpact,
    standard_impact: impl FnOnce(&SE) -> StandardTranslationFailureImpact,
) -> ProductionCommandError
where
    RE: Error + Send + Sync + 'static,
    BE: Error + Send + Sync + 'static,
    SE: Error + Send + Sync + 'static,
    LE: Error + Send + Sync + 'static,
    PE: Error + Send + Sync + 'static,
{
    match error {
        error @ TranslateServiceError::ProjectLease(_) => {
            ProductionCommandError::ProjectUnavailable(Box::new(error))
        }
        error @ TranslateServiceError::ReadProject { .. } => {
            ProductionCommandError::ProjectState(Box::new(error))
        }
        error @ TranslateServiceError::BuildExecution(_) => match &error {
            TranslateServiceError::BuildExecution(source) => match build_impact(source) {
                TranslationExecutionBuildFailureImpact::ConfigurationOrInput => {
                    ProductionCommandError::ConfigurationOrInput(Box::new(error))
                }
                TranslationExecutionBuildFailureImpact::Internal => {
                    ProductionCommandError::Internal(Box::new(error))
                }
            },
            _ => unreachable!("当前分支已经确认是翻译执行上下文构造错误"),
        },
        error @ TranslateServiceError::Standard { .. } => match &error {
            TranslateServiceError::Standard { source } => match standard_impact(source) {
                StandardTranslationFailureImpact::ConfigurationOrInput => {
                    ProductionCommandError::ConfigurationOrInput(Box::new(error))
                }
                StandardTranslationFailureImpact::ProjectState => {
                    ProductionCommandError::ProjectState(Box::new(error))
                }
                StandardTranslationFailureImpact::ExternalModel => {
                    ProductionCommandError::ExternalModel(Box::new(error))
                }
                StandardTranslationFailureImpact::AuditLedger => {
                    ProductionCommandError::AuditLedger(Box::new(error))
                }
                StandardTranslationFailureImpact::StateAppliedButFinalizationFailed => {
                    ProductionCommandError::StateAppliedButFinalizationFailed(Box::new(error))
                }
            },
            _ => unreachable!("当前分支已经确认是 Standard 翻译错误"),
        },
        error @ (TranslateServiceError::MissingResolvedTranslationSemantics
        | TranslateServiceError::Lua { .. }) => ProductionCommandError::Internal(Box::new(error)),
    }
}

fn map_write_back_error<OE, SE, PE, LE, JE, KE>(
    error: WriteBackServiceError<OE, SE, PE, LE, JE, KE>,
) -> ProductionCommandError
where
    OE: Error + Send + Sync + 'static,
    SE: Error + Send + Sync + 'static,
    PE: Error + Send + Sync + 'static,
    LE: Error + Send + Sync + 'static,
    JE: Error + Send + Sync + 'static,
    KE: Error + Send + Sync + 'static,
{
    match error.failure_impact() {
        WriteBackFailureImpact::ProjectUnavailable => {
            ProductionCommandError::ProjectUnavailable(Box::new(error))
        }
        WriteBackFailureImpact::ProjectState => {
            ProductionCommandError::ProjectState(Box::new(error))
        }
        WriteBackFailureImpact::AuditLedger => ProductionCommandError::AuditLedger(Box::new(error)),
        WriteBackFailureImpact::StateAppliedButFinalizationFailed => {
            ProductionCommandError::StateAppliedButFinalizationFailed(Box::new(error))
        }
        WriteBackFailureImpact::OutcomeUnknown => {
            ProductionCommandError::OutcomeUnknown(Box::new(error))
        }
        WriteBackFailureImpact::Internal => ProductionCommandError::Internal(Box::new(error)),
    }
}

fn audit_outcome<T>(
    execution: &DrivenCommand<Result<OperationCompletion<T>, ProductionCommandError>>,
    shutdown: &ShutdownFailures,
) -> AuditRunOutcome {
    match execution {
        DrivenCommand::Finished(Ok(OperationCompletion::Completed(_))) if shutdown.is_empty() => {
            AuditRunOutcome::Succeeded
        }
        DrivenCommand::Finished(Ok(OperationCompletion::Completed(_))) => {
            AuditRunOutcome::Failed(AuditFailureCategory::StateAppliedButFinalizationFailed)
        }
        DrivenCommand::Finished(Ok(OperationCompletion::Cancelled))
        | DrivenCommand::Interrupted(Ok(_))
            if shutdown.is_empty() =>
        {
            AuditRunOutcome::Interrupted
        }
        DrivenCommand::Finished(Ok(OperationCompletion::Cancelled))
        | DrivenCommand::Interrupted(Ok(_)) => {
            AuditRunOutcome::Failed(AuditFailureCategory::Internal)
        }
        DrivenCommand::Finished(Err(error)) | DrivenCommand::Interrupted(Err(error)) => {
            AuditRunOutcome::Failed(error.audit_category())
        }
        DrivenCommand::SignalFailed {
            result: Err(error), ..
        } => AuditRunOutcome::Failed(error.audit_category()),
        DrivenCommand::SignalFailed { .. } => {
            AuditRunOutcome::Failed(AuditFailureCategory::Internal)
        }
    }
}

#[derive(Debug)]
pub(crate) enum ProductionCommandError {
    ConfigurationOrInput(BoxedError),
    ProjectUnavailable(BoxedError),
    ProjectState(BoxedError),
    ExternalModel(BoxedError),
    AuditLedger(BoxedError),
    StateAppliedButFinalizationFailed(BoxedError),
    OutcomeUnknown(BoxedError),
    Internal(BoxedError),
    Signal {
        source: io::Error,
        outcome: SignalOutcome,
    },
}

#[derive(Debug)]
pub(crate) enum SignalOutcome {
    CompletedStateApplied,
    Cancelled,
    CommandFailed(Box<ProductionCommandError>),
}

impl ProductionCommandError {
    fn construct(source: impl Error + Send + Sync + 'static) -> Self {
        Self::Internal(Box::new(source))
    }

    const fn audit_category(&self) -> AuditFailureCategory {
        match self {
            Self::ConfigurationOrInput(_) => AuditFailureCategory::ConfigurationOrInput,
            Self::ProjectUnavailable(_) => AuditFailureCategory::ProjectUnavailable,
            Self::ProjectState(_) => AuditFailureCategory::ProjectState,
            Self::ExternalModel(_) => AuditFailureCategory::ExternalModel,
            Self::AuditLedger(_) => AuditFailureCategory::AuditLedger,
            Self::StateAppliedButFinalizationFailed(_) => {
                AuditFailureCategory::StateAppliedButFinalizationFailed
            }
            Self::OutcomeUnknown(_) => AuditFailureCategory::OutcomeUnknown,
            Self::Internal(_) => AuditFailureCategory::Internal,
            Self::Signal { outcome, .. } => match outcome {
                SignalOutcome::CompletedStateApplied => {
                    AuditFailureCategory::StateAppliedButFinalizationFailed
                }
                SignalOutcome::Cancelled => AuditFailureCategory::Internal,
                SignalOutcome::CommandFailed(command) => command.audit_category(),
            },
        }
    }

    /// 只返回已由责任边界转换为用户可修复语义的安全诊断。
    fn configuration_or_input_detail(&self) -> Option<&dyn fmt::Display> {
        match self {
            Self::ConfigurationOrInput(source) => Some(source.as_ref()),
            Self::Signal {
                outcome: SignalOutcome::CommandFailed(command),
                ..
            } => command.configuration_or_input_detail(),
            _ => None,
        }
    }
}

impl fmt::Display for ProductionCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ConfigurationOrInput(_) => "配置或输入错误",
            Self::ProjectUnavailable(_) => "项目不存在或正忙",
            Self::ProjectState(_) => "项目状态损坏或提取过期",
            Self::ExternalModel(_) => "外部模型不可用",
            Self::AuditLedger(_) => "审计账本不可用",
            Self::StateAppliedButFinalizationFailed(_) => "状态已生效但收尾失败",
            Self::OutcomeUnknown(_) => "结果未知、必须保留现场",
            Self::Internal(_) => "内部技术故障",
            Self::Signal { outcome, .. } => match outcome {
                SignalOutcome::CompletedStateApplied => "状态已生效但收尾失败",
                SignalOutcome::Cancelled => "内部技术故障",
                SignalOutcome::CommandFailed(command) => return command.fmt(formatter),
            },
        })
    }
}

impl Error for ProductionCommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ConfigurationOrInput(source)
            | Self::ProjectUnavailable(source)
            | Self::ProjectState(source)
            | Self::ExternalModel(source)
            | Self::AuditLedger(source)
            | Self::StateAppliedButFinalizationFailed(source)
            | Self::OutcomeUnknown(source)
            | Self::Internal(source) => Some(source.as_ref()),
            Self::Signal { source, outcome } => match outcome {
                SignalOutcome::CommandFailed(command) => Some(command.as_ref()),
                SignalOutcome::CompletedStateApplied | SignalOutcome::Cancelled => Some(source),
            },
        }
    }
}

#[derive(Default)]
pub(crate) struct ShutdownFailures {
    failures: Vec<ShutdownFailure>,
}

impl ShutdownFailures {
    fn push(&mut self, component: &'static str, source: impl Error + Send + Sync + 'static) {
        self.failures.push(ShutdownFailure {
            component,
            source: Box::new(source),
        });
    }

    fn is_empty(&self) -> bool {
        self.failures.is_empty()
    }
}

impl fmt::Display for ShutdownFailures {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("内部技术故障")
    }
}

impl fmt::Debug for ShutdownFailures {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_list()
            .entries(self.failures.iter().map(|failure| failure.component))
            .finish()
    }
}

impl Error for ShutdownFailures {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.failures
            .first()
            .map(|failure| failure.source.as_ref() as _)
    }
}

struct ShutdownFailure {
    component: &'static str,
    source: BoxedError,
}

impl fmt::Debug for ShutdownFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ShutdownFailure")
            .field("component", &self.component)
            .field("source", &self.source)
            .finish()
    }
}

/// 在命令和全部 shutdown 都成功后呈现最终业务结果。
pub(crate) struct CommandResultRenderer;

impl CommandResultRenderer {
    pub(crate) fn render_success(
        output: RpgMakerCommandOutput,
        stdout: &mut dyn Write,
    ) -> io::Result<()> {
        match output {
            RpgMakerCommandOutput::Init(output) => {
                writeln!(stdout, "初始化完成：{}", output.name)?;
                match output.outcome {
                    InitOutcome::Created => writeln!(stdout, "项目状态：已创建"),
                    InitOutcome::Unchanged => writeln!(stdout, "项目状态：无变化"),
                    InitOutcome::Updated { stale_owners } => {
                        writeln!(stdout, "项目状态：已更新")?;
                        if !stale_owners.is_empty() {
                            let owners = stale_owners
                                .into_iter()
                                .map(|owner| match owner {
                                    InitStaleOwner::Builtin => "Builtin",
                                    InitStaleOwner::Rules => "Rules",
                                    InitStaleOwner::Lua => "Lua",
                                })
                                .collect::<Vec<_>>()
                                .join("、");
                            writeln!(stdout, "需重新提取：{owners}")?;
                        }
                        Ok(())
                    }
                }
            }
            RpgMakerCommandOutput::Extract(output) => {
                writeln!(stdout, "提取完成：{}", output.name)
            }
            RpgMakerCommandOutput::Translate(output) => {
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
                writeln!(
                    stdout,
                    "状态收敛：保留 {}，失效 {}，不适用 {}，复用 {}",
                    output.standard.retained,
                    output.standard.invalidated,
                    output.standard.not_applicable,
                    output.standard.reused,
                )?;
                if output.lua_executed {
                    writeln!(stdout, "Lua 翻译：已执行")?;
                }
                Ok(())
            }
            RpgMakerCommandOutput::WriteBack(output) => {
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

    pub(crate) fn render_failure(
        command_error: Option<&ProductionCommandError>,
        shutdown_error: Option<&dyn fmt::Display>,
        stderr: &mut dyn Write,
    ) -> io::Result<()> {
        if let Some(error) = command_error {
            if let Some(detail) = error.configuration_or_input_detail() {
                writeln!(stderr, "配置或输入错误：{detail}")?;
            } else {
                writeln!(stderr, "命令失败：{error}")?;
            }
        }
        if let Some(error) = shutdown_error {
            writeln!(stderr, "收尾失败：{error}")?;
        }
        Ok(())
    }

    pub(crate) fn render_applied_finalization_failure(
        shutdown_error: &dyn fmt::Display,
        stderr: &mut dyn Write,
    ) -> io::Result<()> {
        writeln!(stderr, "状态已生效但收尾失败：{shutdown_error}")
    }
}

#[cfg(test)]
mod command_error_rendering_tests {
    use super::*;

    #[derive(Debug)]
    struct TestError(&'static str);

    impl fmt::Display for TestError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for TestError {}

    #[test]
    fn configuration_or_input_failure_renders_its_safe_detail() {
        let error = ProductionCommandError::ConfigurationOrInput(Box::new(TestError(
            "RPG Maker system prompt 文件 prompts/rpg_maker/ja--zh-Hans.md 不存在",
        )));
        let mut stderr = Vec::new();

        CommandResultRenderer::render_failure(Some(&error), None, &mut stderr)
            .expect("诊断应可写入");

        assert_eq!(
            String::from_utf8(stderr).expect("诊断应为 UTF-8"),
            "配置或输入错误：RPG Maker system prompt 文件 prompts/rpg_maker/ja--zh-Hans.md 不存在\n"
        );
    }

    #[test]
    fn signal_failure_preserves_nested_user_repairable_detail() {
        let error = ProductionCommandError::Signal {
            source: io::Error::other("SIGNAL_SECRET_SENTINEL"),
            outcome: SignalOutcome::CommandFailed(Box::new(
                ProductionCommandError::ConfigurationOrInput(Box::new(TestError(
                    "语言对 ja -> zh-Hans 缺少 Prompt 资源",
                ))),
            )),
        };
        let mut stderr = Vec::new();

        CommandResultRenderer::render_failure(Some(&error), None, &mut stderr)
            .expect("诊断应可写入");
        let stderr = String::from_utf8(stderr).expect("诊断应为 UTF-8");

        assert_eq!(
            stderr,
            "配置或输入错误：语言对 ja -> zh-Hans 缺少 Prompt 资源\n"
        );
        assert!(!stderr.contains("SIGNAL_SECRET_SENTINEL"));
    }

    #[test]
    fn internal_failure_never_renders_its_source() {
        let error = ProductionCommandError::Internal(Box::new(TestError(
            "API_KEY_SENTINEL CLIENT_PARAMETERS_SENTINEL PROMPT_CONTENT_SENTINEL",
        )));
        let mut stderr = Vec::new();

        CommandResultRenderer::render_failure(Some(&error), None, &mut stderr)
            .expect("诊断应可写入");
        let stderr = String::from_utf8(stderr).expect("诊断应为 UTF-8");

        assert_eq!(stderr, "命令失败：内部技术故障\n");
        assert!(!stderr.contains("API_KEY_SENTINEL"));
        assert!(!stderr.contains("CLIENT_PARAMETERS_SENTINEL"));
        assert!(!stderr.contains("PROMPT_CONTENT_SENTINEL"));
    }

    #[test]
    fn internal_translation_build_failure_is_not_mapped_to_user_input() {
        let build = ProductionTranslationExecutionBuildError::internal(TestError(
            "CLIENT_PARAMETERS_SENTINEL",
        ));
        let error = TranslateServiceError::<
            TestError,
            ProductionTranslationExecutionBuildError,
            TestError,
            TestError,
            TestError,
        >::BuildExecution(build);

        let mapped = map_translate_error(
            error,
            ProductionTranslationExecutionBuildError::failure_impact,
            |_| StandardTranslationFailureImpact::ConfigurationOrInput,
        );
        let mut stderr = Vec::new();
        CommandResultRenderer::render_failure(Some(&mapped), None, &mut stderr)
            .expect("诊断应可写入");
        let stderr = String::from_utf8(stderr).expect("诊断应为 UTF-8");

        assert_eq!(stderr, "命令失败：内部技术故障\n");
        assert!(!stderr.contains("CLIENT_PARAMETERS_SENTINEL"));
    }
}
