//! RPG Maker Manual 与只读导出命令的生产装配。

use super::error::ProductionCommandError;
use super::lifecycle::{ProductionCommandRootGuard, ProductionCommandRunReport};
use super::{ProductionRpgMakerCommandRunner, RpgMakerCommandOutput};
use crate::application::config::ConfiguredManualCommand;
use crate::application::termination::{TerminationSignals, drive_with_termination};
use crate::execution::{CooperativeCancellation, OperationCompletion};
use crate::manual::execute_rpg_maker_manual_command;
use crate::project_lease::{ProjectCommandLeaseProvider, ProjectCommandLeaseService};
use crate::rpg_maker::project_database::ProjectWorkspaceLayout;
use crate::runtime::performance::RunPerformanceCounters;
use std::sync::Arc;

impl ProductionRpgMakerCommandRunner {
    pub(super) async fn run_manual(
        self,
        command: ConfiguredManualCommand,
        termination_signals: &mut TerminationSignals,
    ) -> ProductionCommandRunReport {
        let performance = Arc::new(RunPerformanceCounters::default());
        let cancellation = CooperativeCancellation::default();
        let roots = match ProductionCommandRootGuard::start_init(
            command.common().sqlite().clone(),
            performance,
        )
        .await
        {
            Ok(roots) => roots,
            Err(failure) => return failure.into_report(),
        };
        let file_system = roots.file_system().clone();
        let sqlite = roots.sqlite().clone();
        let project = command.project_name().clone();
        let database_path = ProjectWorkspaceLayout::for_project(
            command.common().projects_root(),
            self.layout,
            &project,
        )
        .database_path()
        .to_path_buf();
        let operation = command.operation();
        let file = command.file().to_path_buf();
        let export_selection = command.export_selection().cloned();
        let language_modules = command.language_modules().cloned();
        let lease_provider = ProjectCommandLeaseService::new(
            command.common().projects_root().to_path_buf(),
            self.layout.engine().storage_name(),
            file_system.clone(),
        );
        let engine = self.layout.engine();
        let operation_project = project.clone();
        let operation_cancellation = cancellation.clone();
        let execution = drive_with_termination(
            async move {
                let _lease = lease_provider
                    .acquire(&operation_project)
                    .await
                    .map_err(ProductionCommandError::project_lease)?;
                if operation_cancellation.is_requested() {
                    return Ok(OperationCompletion::Cancelled);
                }
                let blocking_cancellation = operation_cancellation.clone();
                let summary = tokio::task::spawn_blocking(move || {
                    execute_rpg_maker_manual_command(
                        &database_path,
                        engine,
                        operation,
                        &file,
                        export_selection.as_ref(),
                        language_modules.as_ref(),
                        &blocking_cancellation,
                    )
                })
                .await
                .map_err(ProductionCommandError::manual_worker)?;
                let summary = match summary {
                    Ok(summary) => summary,
                    Err(source) if source.is_cancelled() => {
                        return Ok(OperationCompletion::Cancelled);
                    }
                    Err(source) => return Err(ProductionCommandError::manual(source)),
                };
                Ok(OperationCompletion::Completed(
                    RpgMakerCommandOutput::Manual { summary },
                ))
            },
            termination_signals,
            || {
                cancellation.request();
                file_system.cancel_waits();
                sqlite.cancel_waits();
            },
            || {},
        )
        .await;
        let shutdown = roots.shutdown().await;
        ProductionCommandRunReport::from_completion_with_project_log(execution, shutdown, None)
    }
}
