//! RPG Maker 命令入口、生产装配路由与类型化结果。

mod business_log;
mod error;
mod extract;
mod init;
mod lifecycle;
mod lua;
mod manual;
mod progress;
mod rendering;
mod run_plan;
mod translate;
mod translation_setup;
mod write_back;

pub(crate) use error::ProductionCommandError;
pub(crate) use lifecycle::{
    CommandPanicBoundary, CommandRunResult, ProductionCommandRunReport, ShutdownFailures,
};
pub(crate) use rendering::CommandResultRenderer;

use self::lifecycle::catch_command_panic;
use self::lifecycle::command_panic_context;
use crate::application::config::ConfiguredRpgMakerCommand;
use crate::application::termination::TerminationSignals;
use crate::diagnostic::DiagnosticReport;
use crate::i18n::UiLocale;
use crate::manual::ManualCommandSummary;
use crate::rpg_maker::RpgMakerLayout;
use crate::rpg_maker::extract::ExtractOutput;
use crate::rpg_maker::init::InitOutput;
use crate::rpg_maker::translate::TranslateOutput;
use crate::rpg_maker::write_back::WriteBackOutput;
use crate::runtime::project_log::RunPlanValueSource as ProjectLogValueSource;
use std::path::PathBuf;

/// 一个 RPG Maker 命令成功完成后的类型化结果。
pub(crate) enum RpgMakerCommandOutput {
    Init {
        output: InitOutput,
        plan_source: ProjectLogValueSource,
        reused_path: Option<PathBuf>,
    },
    Extract {
        output: ExtractOutput,
        plan_source: ProjectLogValueSource,
        owners: Vec<String>,
        run_plan_warnings: Vec<DiagnosticReport>,
        has_saved_plan: bool,
    },
    Translate {
        output: TranslateOutput,
        profile_source: ProjectLogValueSource,
    },
    WriteBack {
        output: WriteBackOutput,
    },
    Manual {
        summary: ManualCommandSummary,
    },
    Lua {
        project: crate::project_name::ProjectName,
    },
}

/// 按本次命令只构造实际需要的 RPG Maker 生产纵向切片。
pub(crate) struct ProductionRpgMakerCommandRunner {
    layout: RpgMakerLayout,
    locale: UiLocale,
    panic_boundary: CommandPanicBoundary,
}

impl ProductionRpgMakerCommandRunner {
    pub(crate) fn new(layout: RpgMakerLayout, locale: UiLocale) -> Self {
        Self {
            layout,
            locale,
            panic_boundary: CommandPanicBoundary::default(),
        }
    }

    pub(crate) async fn run(
        self,
        command: ConfiguredRpgMakerCommand,
        termination_signals: &mut TerminationSignals,
    ) -> ProductionCommandRunReport {
        let (engine, command_name, project_workspace) =
            command_panic_context(self.layout, &command);
        self.panic_boundary
            .prepare(engine, command_name, &project_workspace);
        if let ConfiguredRpgMakerCommand::Translate(command) = &command
            && command.resolved_profile_id().is_some()
        {
            self.panic_boundary.observe_selected_api_key_redactor(
                command.translation().client().api_key_redactor(),
            );
        }
        let panic_boundary = self.panic_boundary.clone();
        catch_command_panic(panic_boundary, async move {
            match command {
                ConfiguredRpgMakerCommand::Init(command) => {
                    self.run_init(command, termination_signals).await
                }
                ConfiguredRpgMakerCommand::Extract(command) => {
                    self.run_extract(command, termination_signals).await
                }
                ConfiguredRpgMakerCommand::Translate(command) => {
                    self.run_translate(*command, termination_signals).await
                }
                ConfiguredRpgMakerCommand::WriteBack(command) => {
                    self.run_write_back(command, termination_signals).await
                }
                ConfiguredRpgMakerCommand::Manual(command) => {
                    self.run_manual(command, termination_signals).await
                }
                ConfiguredRpgMakerCommand::Ownership(command)
                | ConfiguredRpgMakerCommand::Translation(command) => {
                    self.run_manual(command, termination_signals).await
                }
                ConfiguredRpgMakerCommand::Lua(command) => {
                    self.run_atomic_project_lua(command, termination_signals)
                        .await
                }
            }
        })
        .await
    }
}
