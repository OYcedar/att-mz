//! RPG Maker 命令最终结果和诊断的终端呈现。

use super::RpgMakerCommandOutput;
use super::business_log::usize_to_u64;
use super::error::ProductionCommandError;
use super::lifecycle::ShutdownFailures;
use crate::diagnostic::{
    StateEffect, public_path, render_diagnostic_report, render_state_effect_impact,
};
use crate::i18n::{UiLocalizer, UiMessage, project_log_value_source_label};
use crate::manual::{render_manual_command_error, render_manual_command_summary};
use crate::rpg_maker::init::{InitOutcome, InitStaleOwner};
use crate::rpg_maker::translate::TranslateOutput;
use crate::runtime::project_log::RunPlanValueSource as ProjectLogValueSource;
use std::io;
use std::io::Write;

/// 在命令和全部 shutdown 都成功后呈现最终业务结果。
pub(crate) struct CommandResultRenderer;

impl CommandResultRenderer {
    pub(crate) fn render_success(
        output: &RpgMakerCommandOutput,
        localizer: &UiLocalizer,
        stdout: &mut dyn Write,
    ) -> io::Result<()> {
        match output {
            RpgMakerCommandOutput::Init {
                output,
                plan_source,
                reused_path,
            } => {
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultInitCompleted {
                        project: output.name.as_str(),
                    })
                )?;
                match &output.outcome {
                    InitOutcome::Created => {
                        writeln!(stdout, "{}", localizer.format(UiMessage::ResultInitCreated))?
                    }
                    InitOutcome::Unchanged => writeln!(
                        stdout,
                        "{}",
                        localizer.format(UiMessage::ResultInitUnchanged)
                    )?,
                    InitOutcome::Updated { stale_owners } => {
                        writeln!(stdout, "{}", localizer.format(UiMessage::ResultInitUpdated))?;
                        if !stale_owners.is_empty() {
                            let owners = stale_owners
                                .iter()
                                .map(|owner| match owner {
                                    InitStaleOwner::Builtin => "Builtin",
                                    InitStaleOwner::Rules => "Rules",
                                })
                                .collect::<Vec<_>>()
                                .join("、");
                            writeln!(
                                stdout,
                                "{}",
                                localizer
                                    .format(UiMessage::ResultInitStaleOwners { owners: &owners })
                            )?;
                        }
                    }
                };
                if let Some(path) = reused_path {
                    let path = public_path(path);
                    writeln!(
                        stdout,
                        "{}",
                        localizer.format(UiMessage::NoticeInitReusePath { path: &path })
                    )?;
                }
                render_saved_plan_source(localizer, *plan_source, stdout)
            }
            RpgMakerCommandOutput::Extract {
                output,
                plan_source,
                owners,
                run_plan_warnings: _,
                has_saved_plan,
            } => {
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultExtractCompleted {
                        project: output.name.as_str(),
                    })
                )?;
                if *plan_source == ProjectLogValueSource::ProjectState {
                    writeln!(
                        stdout,
                        "{}",
                        localizer.format(UiMessage::NoticeExtractReuseOwners {
                            owners: &owners.join(", "),
                        })
                    )?;
                }
                if *has_saved_plan {
                    render_saved_plan_source(localizer, *plan_source, stdout)
                } else {
                    Ok(())
                }
            }
            RpgMakerCommandOutput::Translate {
                output,
                profile_source,
            } => {
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultTranslateCompleted {
                        project: output.name.as_str(),
                        profile: &output.profile_id,
                    })
                )?;
                let status = if output.summary.is_incomplete() {
                    "incomplete"
                } else if output.summary.total_tasks == 0 {
                    "no_work"
                } else {
                    "complete"
                };
                let status = localizer.format(UiMessage::ResultTranslateStatusValue { status });
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultTranslateStatus { status: &status })
                )?;
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultTranslateSummary {
                        total: usize_to_u64(output.summary.total_tasks, "任务总数"),
                        started: usize_to_u64(output.summary.started_tasks, "已开始任务数"),
                        not_started: usize_to_u64(
                            output.summary.not_started_tasks,
                            "未开始任务数",
                        ),
                        complete: usize_to_u64(output.summary.complete_tasks, "完整任务数"),
                        partial: usize_to_u64(output.summary.partial_tasks, "部分任务数"),
                        unavailable: usize_to_u64(
                            output.summary.unavailable_tasks,
                            "不可用任务数",
                        ),
                        failed: 0,
                        cancelled: 0,
                        written: usize_to_u64(output.summary.written_locations, "已写位置数"),
                        remaining: usize_to_u64(
                            output.summary.remaining_locations,
                            "剩余位置数",
                        ),
                        rejected: usize_to_u64(
                            output.summary.rejected_locations,
                            "Rejected 位置数",
                        ),
                    })
                )?;
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultTranslateConvergence {
                        retained: usize_to_u64(output.summary.retained, "保留决策数"),
                        invalidated: usize_to_u64(output.summary.invalidated, "失效决策数"),
                        not_applicable: usize_to_u64(
                            output.summary.not_applicable,
                            "不适用决策数",
                        ),
                        reused: usize_to_u64(output.summary.reused, "复用决策数"),
                    })
                )?;
                if output.summary.total_tasks == 0 && !output.summary.is_incomplete() {
                    writeln!(
                        stdout,
                        "{}",
                        localizer.format(UiMessage::NoticeNoModelRequest)
                    )?;
                }
                if *profile_source == ProjectLogValueSource::ProjectState {
                    writeln!(
                        stdout,
                        "{}",
                        localizer.format(UiMessage::NoticeTranslateReuseProfile {
                            profile: &output.profile_id,
                        })
                    )?;
                }
                render_saved_plan_source(localizer, *profile_source, stdout)
            }
            RpgMakerCommandOutput::WriteBack { output } => {
                let output_root = public_path(&output.output_root);
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultWriteBackCompleted {
                        project: output.name.as_str(),
                    })
                )?;
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultOutputDirectory { path: &output_root })
                )?;
                writeln!(
                    stdout,
                    "{}",
                    localizer.format(UiMessage::ResultWriteBackSummary {
                        translated: usize_to_u64(output.summary.translated_units, "已翻译 Unit 数"),
                        original: usize_to_u64(output.summary.original_units, "保留原文 Unit 数"),
                    })
                )?;
                Ok(())
            }
            RpgMakerCommandOutput::Manual { summary } => {
                render_manual_command_summary(summary, localizer, stdout)
            }
            RpgMakerCommandOutput::Lua { project } => writeln!(
                stdout,
                "{}",
                localizer.format(UiMessage::ResultProjectLuaCompleted {
                    project: project.as_str(),
                })
            ),
        }
    }

    pub(crate) fn render_success_warnings(
        output: &RpgMakerCommandOutput,
        localizer: &UiLocalizer,
        stderr: &mut dyn Write,
    ) -> io::Result<()> {
        match output {
            RpgMakerCommandOutput::Extract {
                output,
                run_plan_warnings,
                ..
            } => {
                for warning in &output.rules_warnings {
                    writeln!(
                        stderr,
                        "{}",
                        localizer.format(UiMessage::DiagnosticWarningHeading)
                    )?;
                    writeln!(
                        stderr,
                        "{}",
                        render_diagnostic_report(&warning.diagnostic_report(), localizer)
                    )?;
                }
                for warning in run_plan_warnings {
                    writeln!(
                        stderr,
                        "{}",
                        localizer.format(UiMessage::DiagnosticWarningHeading)
                    )?;
                    writeln!(stderr, "{}", render_diagnostic_report(warning, localizer))?;
                }
            }
            RpgMakerCommandOutput::Translate { output, .. } if output.summary.is_incomplete() => {
                render_rpg_maker_incomplete_warning(output, localizer, stderr)?;
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn render_failure(
        command_error: Option<&ProductionCommandError>,
        shutdown_error: Option<&ShutdownFailures>,
        localizer: &UiLocalizer,
        stderr: &mut dyn Write,
    ) -> io::Result<()> {
        let command_renders_its_own_headings = command_error
            .and_then(ProductionCommandError::manual_error)
            .is_some();
        if (command_error.is_some() && !command_renders_its_own_headings)
            || (command_error.is_none() && shutdown_error.is_some())
        {
            writeln!(
                stderr,
                "{}",
                localizer.format(UiMessage::DiagnosticErrorHeading)
            )?;
        }
        if let Some(error) = command_error {
            if let Some(manual) = error.manual_error() {
                render_manual_command_error(manual, localizer, stderr)?;
            } else {
                writeln!(
                    stderr,
                    "{}",
                    render_diagnostic_report(error.failure_report().report(), localizer)
                )?;
            }
        }
        if let Some(shutdown) = shutdown_error {
            render_shutdown_failures(shutdown, command_error.is_some(), localizer, stderr)?;
        }
        Ok(())
    }

    pub(crate) fn render_applied_finalization_failure(
        shutdown_error: &ShutdownFailures,
        localizer: &UiLocalizer,
        stderr: &mut dyn Write,
    ) -> io::Result<()> {
        writeln!(
            stderr,
            "{}",
            localizer.format(UiMessage::DiagnosticErrorHeading)
        )?;
        render_shutdown_failures(shutdown_error, false, localizer, stderr)
    }

    /// 进程结果呈现已经形成主错误时，把 shutdown 逐项呈现为相关错误。
    pub(crate) fn render_related_shutdown_failures(
        shutdown_error: &ShutdownFailures,
        localizer: &UiLocalizer,
        stderr: &mut dyn Write,
    ) -> io::Result<()> {
        render_shutdown_failures(shutdown_error, true, localizer, stderr)
    }
}

pub(super) fn render_rpg_maker_incomplete_warning(
    output: &TranslateOutput,
    localizer: &UiLocalizer,
    stderr: &mut dyn Write,
) -> io::Result<()> {
    let object = localizer.format(UiMessage::TranslateIncompleteObject {
        project: output.name.as_str(),
    });
    let reason = localizer.format(UiMessage::TranslateIncompleteRpgMakerReason {
        partial: usize_to_u64(output.summary.partial_tasks, "部分任务数"),
        unavailable: usize_to_u64(output.summary.unavailable_tasks, "不可用任务数"),
        protocol: usize_to_u64(output.summary.protocol_diagnostics, "协议问题数"),
        exhausted: usize_to_u64(output.summary.recoverable_request_exhaustions, "请求耗尽数"),
        admission: if output.summary.request_admission_stopped {
            "stopped"
        } else {
            "open"
        },
        not_started: usize_to_u64(output.summary.not_started_tasks, "未开始任务数"),
        remaining_decisions: usize_to_u64(output.summary.remaining_decisions, "剩余决策数"),
        remaining_locations: usize_to_u64(output.summary.remaining_locations, "剩余位置数"),
        rejected_locations: usize_to_u64(output.summary.rejected_locations, "Rejected 位置数"),
    });
    let impact = render_state_effect_impact(StateEffect::ProgressPreserved, localizer);
    let help = localizer.format(if output.summary.rejected_locations > 0 {
        UiMessage::TranslateIncompleteRejectedHelp
    } else {
        UiMessage::TranslateIncompleteHelp
    });
    writeln!(
        stderr,
        "{}",
        localizer.format(UiMessage::DiagnosticWarningHeading)
    )?;
    writeln!(
        stderr,
        "{}",
        localizer.format(UiMessage::DiagnosticObject { subject: &object })
    )?;
    writeln!(
        stderr,
        "{}",
        localizer.format(UiMessage::DiagnosticExplanation { reason: &reason })
    )?;
    writeln!(
        stderr,
        "{}",
        localizer.format(UiMessage::DiagnosticImpact { impact: &impact })
    )?;
    writeln!(
        stderr,
        "{}",
        localizer.format(UiMessage::DiagnosticResolution { action: &help })
    )
}

pub(super) fn render_shutdown_failures(
    failures: &ShutdownFailures,
    follows_primary: bool,
    localizer: &UiLocalizer,
    stderr: &mut dyn Write,
) -> io::Result<()> {
    let mut diagnostics = failures.diagnostic_reports();
    if follows_primary {
        for diagnostic in diagnostics {
            writeln!(
                stderr,
                "{}",
                localizer.format(UiMessage::DiagnosticRelated {
                    relation: "shutdown",
                })
            )?;
            writeln!(
                stderr,
                "{}",
                render_diagnostic_report(diagnostic, localizer)
            )?;
        }
    } else if let Some(primary) = diagnostics.next() {
        writeln!(stderr, "{}", render_diagnostic_report(primary, localizer))?;
        for diagnostic in diagnostics {
            writeln!(
                stderr,
                "{}",
                localizer.format(UiMessage::DiagnosticRelated {
                    relation: "shutdown",
                })
            )?;
            writeln!(
                stderr,
                "{}",
                render_diagnostic_report(diagnostic, localizer)
            )?;
        }
    }
    Ok(())
}

pub(super) fn render_saved_plan_source(
    localizer: &UiLocalizer,
    source: ProjectLogValueSource,
    stdout: &mut dyn Write,
) -> io::Result<()> {
    let source = plan_source_message(source);
    writeln!(
        stdout,
        "{} ({})",
        localizer.format(UiMessage::ResultPlanSaved),
        localizer.format(source),
    )
}

pub(super) fn plan_source_message(source: ProjectLogValueSource) -> UiMessage<'static> {
    project_log_value_source_label(match source {
        ProjectLogValueSource::Explicit => "explicit",
        ProjectLogValueSource::ProjectState => "project_state",
        ProjectLogValueSource::ProductDefault => "product_default",
    })
    .expect("每个运行方案来源代码都必须具有本地化日志标签")
}
