//! RPG Maker Init 的输入复用、工作区恢复与生产装配。

use super::error::{ProductionCommandError, SignalOutcomeSource, map_init_error};
#[cfg(test)]
use super::lifecycle::CommandRunResult;
use super::lifecycle::{
    ProductionCommandRootGuard, ProductionCommandRunReport, ProductionWorkspaceConvergenceError,
    drive_project_lease, interrupted_non_cancellation_error, map_completion,
};
use super::progress::{
    ProductionProgressObserver, business_completed, defer_terminal_progress_status,
    finish_progress_business_state, finish_terminal_progress, init_phase_code,
    init_terminal_progress, pending_project_log_with_occurrence, progress_finalizing,
    progress_safe_stopping, progress_saving_plan, project_log_engine, record_failed_phase,
};
use super::run_plan::RunPlanResolutionError;
use super::run_plan::{
    RunPlanFinalizationInput, finalize_run_plan, replace_success_with_plan_error,
};
use super::{ProductionRpgMakerCommandRunner, RpgMakerCommandOutput};
use crate::application::config::ConfiguredInitCommand;
use crate::application::project_log::{CommandLogStart, start_command_log};
use crate::application::termination::{
    TerminationOutcome as DrivenCommand, TerminationSignals, drive_with_termination,
};
use crate::execution::{CooperativeCancellation, OperationCompletion};
#[cfg(test)]
use crate::i18n::UiLocale;
use crate::project_lease::ProjectCommandLeaseService;
#[cfg(test)]
use crate::rpg_maker::RpgMakerLayout;
use crate::rpg_maker::init::{
    InitInput, InitService, ProjectWorkspaceConvergenceError, ProjectWorkspaceConvergenceService,
};
use crate::rpg_maker::project_database::{
    InitRunPlan, ProjectDatabaseCreationService, ProjectDatabaseStateReconciliationService,
    ProjectRunPlanPersistenceService, ProjectRunPlanReadError, ProjectRunPlanReplacement,
    ProjectRunPlanRepository, ProjectWorkspaceLayout,
};
#[cfg(test)]
use crate::runtime::filesystem::SystemFileSystem;
use crate::runtime::performance::RunPerformanceCounters;
use crate::runtime::project_log::{
    DiagnosticScope, ProjectLogCommand, ProjectLogEvent, ResolvedRunPlan,
    RunPlanValueSource as ProjectLogValueSource,
};
#[cfg(test)]
use crate::storage::file_system::DirectoryPublishError;
use crate::storage::file_system::{
    DirectoryRecoveryOutcome, ExistingDirectoryResolver, RecoverableDirectoryPublisher,
    ResolveDirectoryError,
};
#[cfg(test)]
use std::path::{Path, PathBuf};
use std::sync::Arc;

impl ProductionRpgMakerCommandRunner {
    pub(super) async fn run_init(
        self,
        command: ConfiguredInitCommand,
        termination_signals: &mut TerminationSignals,
    ) -> ProductionCommandRunReport {
        let performance = Arc::new(RunPerformanceCounters::default());
        let progress = init_terminal_progress(self.locale);
        let progress_observer =
            ProductionProgressObserver::without_project_log(progress.observer(), init_phase_code);
        let cancellation = CooperativeCancellation::default();
        let projects_root = command.common().projects_root().to_path_buf();
        let sqlite_configuration = command.common().sqlite().clone();
        let roots = match ProductionCommandRootGuard::start_init(
            sqlite_configuration.clone(),
            Arc::clone(&performance),
        )
        .await
        {
            Ok(roots) => roots,
            Err(failure) => return failure.into_report(),
        };
        let file_system = roots.file_system().clone();
        let sqlite = roots.sqlite().clone();
        let arguments = &command.arguments;
        let project_name = arguments.project.name.clone();
        let project_workspace =
            ProjectWorkspaceLayout::for_project(&projects_root, self.layout, &project_name);
        let database_path = project_workspace.database_path().to_path_buf();
        let lease_provider = ProjectCommandLeaseService::new(
            projects_root.clone(),
            self.layout.engine().storage_name(),
            file_system.clone(),
        );
        let project_lease = drive_project_lease(
            &lease_provider,
            &project_name,
            &file_system,
            &sqlite,
            &cancellation,
            termination_signals,
            || {
                defer_terminal_progress_status(
                    progress.safe_stopping(progress_safe_stopping(self.locale)),
                );
            },
        )
        .await;
        let project_lease_guard = match project_lease {
            DrivenCommand::Finished(Ok(lease)) => lease,
            DrivenCommand::Finished(Err(error)) => {
                let shutdown = roots.shutdown().await;
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    error, shutdown,
                );
            }
            DrivenCommand::Interrupted(result) => {
                let error = interrupted_non_cancellation_error(result);
                let shutdown = finish_terminal_progress(progress, roots.shutdown().await);
                return match error {
                    Some(error) => ProductionCommandRunReport::failed_before_logging_with_shutdown(
                        error, shutdown,
                    ),
                    None => ProductionCommandRunReport::interrupted_before_logging(shutdown),
                };
            }
            DrivenCommand::SignalFailed { source, result } => {
                let outcome = match result {
                    Ok(lease) => {
                        drop(lease);
                        SignalOutcomeSource::Cancelled
                    }
                    Err(error) => SignalOutcomeSource::CommandFailed(error),
                };
                let shutdown = finish_terminal_progress(progress, roots.shutdown().await);
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    ProductionCommandError::signal(source, outcome),
                    shutdown,
                );
            }
        };
        let directory_publisher = file_system.directory_publisher(command.publisher().clone());
        let recovery_target = project_workspace.workspace_root().to_path_buf();
        let engine_workspace_root = recovery_target
            .parent()
            .expect("固定项目工作区必有引擎父目录")
            .to_path_buf();
        let recovery = drive_with_termination(
            async {
                match file_system
                    .resolve_existing_directory(engine_workspace_root)
                    .await
                {
                    Ok(_) => directory_publisher
                        .recover(recovery_target)
                        .await
                        .map_err(ProjectWorkspaceConvergenceError::Recover),
                    Err(ResolveDirectoryError::NotFound { .. }) => {
                        Ok(DirectoryRecoveryOutcome::Unchanged)
                    }
                    Err(source) => {
                        Err(ProjectWorkspaceConvergenceError::ObserveEngineWorkspaceRoot(source))
                    }
                }
            },
            termination_signals,
            || {
                cancellation.request();
                file_system.cancel_waits();
                sqlite.cancel_waits();
            },
            || {
                defer_terminal_progress_status(
                    progress.safe_stopping(progress_safe_stopping(self.locale)),
                );
            },
        )
        .await
        .map(
            |result: Result<DirectoryRecoveryOutcome, ProductionWorkspaceConvergenceError>| {
                result.map_err(map_init_error)
            },
        );
        match recovery {
            DrivenCommand::Finished(Ok(_)) => {}
            DrivenCommand::Finished(Err(error)) => {
                let shutdown = roots.shutdown().await;
                drop(project_lease_guard);
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    error, shutdown,
                );
            }
            DrivenCommand::Interrupted(result) => {
                let error = interrupted_non_cancellation_error(result);
                let shutdown = finish_terminal_progress(progress, roots.shutdown().await);
                drop(project_lease_guard);
                return match error {
                    Some(error) => ProductionCommandRunReport::failed_before_logging_with_shutdown(
                        error, shutdown,
                    ),
                    None => ProductionCommandRunReport::interrupted_before_logging(shutdown),
                };
            }
            DrivenCommand::SignalFailed { source, result } => {
                let outcome = match result {
                    Ok(DirectoryRecoveryOutcome::Recovered) => {
                        SignalOutcomeSource::CompletedStateApplied
                    }
                    Ok(DirectoryRecoveryOutcome::Unchanged) => SignalOutcomeSource::Cancelled,
                    Err(error) => SignalOutcomeSource::CommandFailed(error),
                };
                let shutdown = finish_terminal_progress(progress, roots.shutdown().await);
                drop(project_lease_guard);
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    ProductionCommandError::signal(source, outcome),
                    shutdown,
                );
            }
        }
        let (game_root, plan_source) = match arguments.path.clone() {
            Some(path) => (path, ProjectLogValueSource::Explicit),
            None => {
                let repository = ProjectRunPlanPersistenceService::new(sqlite.clone());
                match repository.read(database_path.clone()).await {
                    Ok(plans) => match plans.init() {
                        Some(plan) => (
                            plan.source_path().to_path_buf(),
                            ProjectLogValueSource::ProjectState,
                        ),
                        None => {
                            let shutdown = roots.shutdown().await;
                            drop(project_lease_guard);
                            return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                                ProductionCommandError::run_plan_resolution(
                                    RunPlanResolutionError::InitPathRequired,
                                ),
                                shutdown,
                            );
                        }
                    },
                    Err(ProjectRunPlanReadError::DatabaseNotFound { .. }) => {
                        let shutdown = roots.shutdown().await;
                        drop(project_lease_guard);
                        return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                            ProductionCommandError::run_plan_resolution(
                                RunPlanResolutionError::InitPathRequired,
                            ),
                            shutdown,
                        );
                    }
                    Err(error) => {
                        let shutdown = roots.shutdown().await;
                        drop(project_lease_guard);
                        return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                            ProductionCommandError::project_run_plan_read(error),
                            shutdown,
                        );
                    }
                }
            }
        };
        let resolved_game_root = match file_system.resolve_existing_directory(game_root).await {
            Ok(path) => path,
            Err(error) => {
                let shutdown = roots.shutdown().await;
                drop(project_lease_guard);
                return ProductionCommandRunReport::failed_before_logging_with_shutdown(
                    ProductionCommandError::input_directory(error),
                    shutdown,
                );
            }
        };
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
        )
        .with_progress(progress_observer.clone());
        let service = InitService::new(workspace, cancellation.clone());
        let input = InitInput {
            name: project_name,
            game_root: resolved_game_root.clone(),
            source_language: arguments.source_language.clone(),
            target_language: arguments.target_language.clone(),
        };

        let safe_stopping = progress_safe_stopping(self.locale);
        let mut execution = drive_with_termination(
            service.execute(input),
            termination_signals,
            || {
                cancellation.request();
                file_system.cancel_waits();
                sqlite.cancel_waits();
            },
            || {
                defer_terminal_progress_status(progress.safe_stopping(safe_stopping));
            },
        )
        .await
        .map(|result| result.map_err(map_init_error));
        finish_progress_business_state(&progress_observer, &execution);
        let shutdown = roots.shutdown().await;
        let reused_path = (plan_source == ProjectLogValueSource::ProjectState)
            .then(|| resolved_game_root.clone());
        let workspace_is_legal = matches!(
            &execution,
            DrivenCommand::Finished(Ok(OperationCompletion::Completed(_)))
                | DrivenCommand::Interrupted(Ok(OperationCompletion::Completed(_)))
                | DrivenCommand::SignalFailed {
                    result: Ok(OperationCompletion::Completed(_)),
                    ..
                }
        );
        if !workspace_is_legal {
            drop(project_lease_guard);
            let shutdown = finish_terminal_progress(progress, shutdown);
            return ProductionCommandRunReport::from_completion_with_project_log(
                execution.map(|result| {
                    result.map(|completion| {
                        map_completion(completion, |output| RpgMakerCommandOutput::Init {
                            output,
                            plan_source,
                            reused_path,
                        })
                    })
                }),
                shutdown,
                None,
            );
        }
        let project_log = start_command_log(CommandLogStart {
            common: command.common(),
            locale: self.locale,
            engine: project_log_engine(self.layout),
            project: command.arguments.project.name.as_str(),
            command: ProjectLogCommand::Init,
            performance: Arc::clone(&performance),
            selected_api_key_redactor: None,
        });
        self.panic_boundary.observe_project_log(&project_log);
        project_log.handle().emit(ProjectLogEvent::RunPlanResolved {
            plan: ResolvedRunPlan::init(plan_source, &resolved_game_root),
        });
        let replacement = InitRunPlan::new(resolved_game_root)
            .map(ProjectRunPlanReplacement::Init)
            .map_err(ProductionCommandError::invalid_run_plan);
        if !matches!(execution, DrivenCommand::Interrupted(_)) {
            defer_terminal_progress_status(progress.finalizing(progress_finalizing(self.locale)));
        }
        execution = match replacement {
            Ok(replacement) => {
                if business_completed(&execution) && shutdown.is_empty() {
                    defer_terminal_progress_status(
                        progress.finalizing(progress_saving_plan(self.locale)),
                    );
                }
                finalize_run_plan(
                    execution,
                    &shutdown,
                    RunPlanFinalizationInput {
                        database_path,
                        replacement,
                        sqlite_configuration,
                    },
                    &project_log,
                    termination_signals,
                    || {
                        defer_terminal_progress_status(
                            progress.safe_stopping(progress_safe_stopping(self.locale)),
                        );
                        let (confirmed, total) = progress_observer.confirmed_amount();
                        project_log
                            .handle()
                            .emit(ProjectLogEvent::CancellationRequested { confirmed, total });
                    },
                )
                .await
            }
            Err(error) => replace_success_with_plan_error(execution, Err(error)),
        };
        drop(project_lease_guard);
        let shutdown = finish_terminal_progress(progress, shutdown);
        let terminal_diagnostic = record_failed_phase(
            &progress_observer,
            &project_log,
            &execution,
            &shutdown,
            DiagnosticScope::Run,
        );
        let pending_project_log = pending_project_log_with_occurrence(
            project_log,
            &execution,
            &shutdown,
            terminal_diagnostic,
        );
        ProductionCommandRunReport::from_completion_with_project_log(
            execution.map(|result| {
                result.map(|completion| {
                    map_completion(completion, |output| RpgMakerCommandOutput::Init {
                        output,
                        plan_source,
                        reused_path,
                    })
                })
            }),
            shutdown,
            Some(pending_project_log),
        )
    }
}
#[cfg(test)]
mod init_entry_recovery_tests {
    use std::fs;

    use super::*;
    use crate::application::arguments::InitArguments;
    #[cfg(test)]
    use crate::application::arguments::ProjectArguments;
    use crate::language::LanguageId;
    use crate::runtime::filesystem::DirectoryPublisherConfig;
    #[cfg(test)]
    use crate::runtime::filesystem::TestPublishFaultAction;
    #[cfg(test)]
    use crate::runtime::filesystem::TestPublishFaultPoint;
    #[cfg(test)]
    use crate::runtime::filesystem::register_test_publish_faults;
    use crate::storage::file_system::DirectoryPublishIntent;
    #[cfg(test)]
    use crate::storage::file_system::DirectorySourceMapping;
    #[cfg(test)]
    use crate::storage::file_system::DirectoryStageRequest;
    #[cfg(test)]
    use crate::storage::file_system::RecoverableDirectoryPublisher;

    fn init_command(
        projects_root: &Path,
        game_root: Option<PathBuf>,
        include_settings: bool,
    ) -> ConfiguredInitCommand {
        ConfiguredInitCommand::for_test(
            InitArguments {
                project: ProjectArguments {
                    name: "entry-recovery".parse().expect("测试项目名应有效"),
                },
                path: game_root,
                source_language: include_settings
                    .then(|| "ja".parse::<LanguageId>().expect("测试原文语言应有效")),
                target_language: include_settings
                    .then(|| "zh-Hans".parse::<LanguageId>().expect("测试译文语言应有效")),
            },
            projects_root,
            "mz",
        )
    }

    fn write_minimal_mz_game(game_root: &Path) {
        fs::create_dir_all(game_root.join("data")).expect("测试 data 目录应可建立");
        fs::create_dir_all(game_root.join("js")).expect("测试 js 目录应可建立");
        fs::write(game_root.join("data/System.json"), b"{}").expect("测试数据文件应可写入");
        fs::write(game_root.join("js/rmmz_core.js"), b"/* MZ */").expect("测试核心脚本应可写入");
    }

    fn copy_tree(source: &Path, target: &Path) {
        fs::create_dir(target).expect("候选根应可建立");
        for entry in fs::read_dir(source).expect("项目工作区应可读取") {
            let entry = entry.expect("项目工作区目录项应可读取");
            let source_path = entry.path();
            let target_path = target.join(entry.file_name());
            if entry
                .file_type()
                .expect("项目工作区目录项类型应可读取")
                .is_dir()
            {
                copy_tree(&source_path, &target_path);
            } else {
                fs::copy(&source_path, &target_path).expect("项目工作区文件应可复制");
            }
        }
    }

    fn finish_successful_report(mut report: ProductionCommandRunReport) {
        match &report.result {
            CommandRunResult::Succeeded(_) => {}
            CommandRunResult::Interrupted => panic!("Init 不应取消"),
            CommandRunResult::Failed(error) => panic!("Init 应成功，实际为 {error}"),
        }
        assert!(report.shutdown_error.is_none());
        if let Some(project_log) = report.pending_project_log.take() {
            assert!(project_log.finish().is_none());
        }
    }

    #[tokio::test]
    async fn init_recovers_missing_workspace_before_reusing_omitted_path() {
        let temporary = tempfile::tempdir().expect("临时目录应可建立");
        let projects_root = temporary.path().join("projects");
        let game_root = temporary.path().join("game");
        fs::create_dir(&projects_root).expect("项目根应可建立");
        write_minimal_mz_game(&game_root);

        let mut signals = TerminationSignals::new();
        let first =
            ProductionRpgMakerCommandRunner::new(RpgMakerLayout::MZ, UiLocale::SimplifiedChinese)
                .run_init(
                    init_command(&projects_root, Some(game_root.clone()), true),
                    &mut signals,
                )
                .await;
        finish_successful_report(first);

        let workspace = projects_root.join("mz/entry-recovery");
        let replacement = temporary.path().join("replacement");
        copy_tree(&workspace, &replacement);
        let file_system = SystemFileSystem::new().expect("测试文件系统根应可建立");
        let publisher = file_system.directory_publisher(
            DirectoryPublisherConfig::production(
                projects_root.join(".att-locks/directory-publish/mz"),
            )
            .expect("测试发布配置应有效"),
        );
        let staged = publisher
            .prepare(
                DirectoryStageRequest::new(
                    workspace.clone(),
                    DirectoryPublishIntent::ReplaceExisting,
                    vec![
                        DirectorySourceMapping::new(replacement, PathBuf::new())
                            .expect("测试来源映射应有效"),
                    ],
                    Vec::new(),
                    Vec::new(),
                )
                .expect("测试候选请求应有效"),
            )
            .await
            .expect("测试替换候选应可准备");
        let canonical_workspace = workspace
            .parent()
            .expect("工作区应有父目录")
            .canonicalize()
            .expect("工作区父目录应可规范化")
            .join(workspace.file_name().expect("工作区应有名称"));
        register_test_publish_faults(
            canonical_workspace,
            [(
                TestPublishFaultPoint::BeforeBackupCleanup,
                TestPublishFaultAction::Error,
            )],
        );
        assert!(matches!(
            publisher.publish(staged).await,
            Err(DirectoryPublishError::PublishedWithResiduals { .. })
        ));
        file_system
            .shutdown()
            .await
            .expect("测试文件系统根应可关闭");

        fs::remove_dir_all(&workspace).expect("测试应可移除已发布目标以模拟中断现场");

        let mut signals = TerminationSignals::new();
        let resumed =
            ProductionRpgMakerCommandRunner::new(RpgMakerLayout::MZ, UiLocale::SimplifiedChinese)
                .run_init(init_command(&projects_root, None, false), &mut signals)
                .await;
        finish_successful_report(resumed);

        assert!(workspace.join("project.db").is_file());
        let publication_workspace = workspace
            .parent()
            .expect("工作区应有父目录")
            .join(".directory-publish")
            .join(workspace.file_name().expect("工作区应有名称"));
        assert_eq!(
            fs::read_dir(publication_workspace)
                .expect("目标目录发布工作目录应可读取")
                .count(),
            0,
            "恢复后不得留下 stage、backup 或 journal"
        );
    }
}
