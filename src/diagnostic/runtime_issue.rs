//! 运行时、文件系统、SQLite 与 HTTP 边界建立的封闭诊断问题。

use serde::{Deserialize, Serialize};

use super::DiagnosticStage;
use super::issue::IoFailure;
use super::model::DiagnosticResolution;
use super::safe_value::{SafeIdentifier, SafePath, SafeText};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeComponent {
    Process,
    WindowsUtf8Environment,
    TokioRuntime,
    CpuExecutor,
    FileSystemExecutor,
    SqliteExecutor,
    TerminationSignals,
    TerminalProgress,
}

impl RuntimeComponent {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::WindowsUtf8Environment => "windows_utf8_environment",
            Self::TokioRuntime => "tokio_runtime",
            Self::CpuExecutor => "cpu_executor",
            Self::FileSystemExecutor => "filesystem_executor",
            Self::SqliteExecutor => "sqlite_executor",
            Self::TerminationSignals => "termination_signals",
            Self::TerminalProgress => "terminal_progress",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeOperation {
    ValidateEnvironment,
    ResolveCurrentExecutable,
    DetectAvailableParallelism,
    BuildAsyncRuntime,
    StartWorker,
    ExecuteTask,
    PrepareRpgMakerTranslationSnapshot,
    DecodeRpgMakerTranslationUnits,
    AssembleRpgMakerTranslationCorpus,
    EncodeRpgMakerTranslationResult,
    PrepareRpgMakerPlanningResources,
    CompileRpgMakerCustomPlaceholders,
    PrepareRpgMakerTranslationCorpus,
    PreprocessRpgMakerTranslationScopes,
    DeduplicateRpgMakerTranslationCorpus,
    PlanRpgMakerTranslationScopes,
    FinalizeRpgMakerTranslationPlan,
    PrepareRpgMakerPrompt,
    CompileRpgMakerBuiltinPlaceholders,
    ReceiveTerminationSignal,
    WriteStdout,
    WriteStderr,
    Shutdown,
    StartTerminalProgressRenderer,
    PublishTerminalProgressCompletion,
    RenderTerminalProgressDynamicLine,
    RenderTerminalProgressStatus,
    ClearTerminalProgressDynamicLine,
    RenderTerminalProgressFinalMessage,
    FinalizeTerminalProgress,
    ReportTerminalProgressSafeStop,
    FinishTerminalProgress,
    JoinTerminalProgressRenderer,
}

/// 已由业务服务保证不会发生、只能表示内部契约被破坏的命令边界操作。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeBoundaryOperation {
    InitProjectLeaseAlreadyHeld,
    InitWorkspaceStageRequestInvalid,
    ExtractProjectLeaseAlreadyHeld,
    ExtractProjectAlreadyOpened,
    TranslateProjectLeaseAlreadyHeld,
    TranslateProjectAlreadyOpened,
    WriteBackProjectLeaseAlreadyHeld,
    WriteBackProjectAlreadyOpened,
    TranslateResultStorePlanInvalid,
    TranslateResultStoreSessionChanged,
    TranslateResultStoreSessionFinalized,
}

impl RuntimeBoundaryOperation {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::InitProjectLeaseAlreadyHeld => "init_project_lease_already_held",
            Self::InitWorkspaceStageRequestInvalid => "init_workspace_stage_request_invalid",
            Self::ExtractProjectLeaseAlreadyHeld => "extract_project_lease_already_held",
            Self::ExtractProjectAlreadyOpened => "extract_project_already_opened",
            Self::TranslateProjectLeaseAlreadyHeld => "translate_project_lease_already_held",
            Self::TranslateProjectAlreadyOpened => "translate_project_already_opened",
            Self::WriteBackProjectLeaseAlreadyHeld => "write_back_project_lease_already_held",
            Self::WriteBackProjectAlreadyOpened => "write_back_project_already_opened",
            Self::TranslateResultStorePlanInvalid => "translate_result_store_plan_invalid",
            Self::TranslateResultStoreSessionChanged => "translate_result_store_session_changed",
            Self::TranslateResultStoreSessionFinalized => {
                "translate_result_store_session_finalized"
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TranslationTaskCounterInvariant {
    StartedBreakdown,
    PlannedBreakdown,
    Overflow,
}

impl TranslationTaskCounterInvariant {
    const fn as_str(self) -> &'static str {
        match self {
            Self::StartedBreakdown => "started_breakdown",
            Self::PlannedBreakdown => "planned_breakdown",
            Self::Overflow => "overflow",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeEngine {
    Generic,
    RpgMakerMv,
    RpgMakerMz,
}

impl RuntimeEngine {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::RpgMakerMv => "rpg_maker_mv",
            Self::RpgMakerMz => "rpg_maker_mz",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeCommand {
    Init,
    Extract,
    Builtin,
    Rules,
    Translate,
    WriteBack,
    Lua,
}

impl RuntimeCommand {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::Extract => "extract",
            Self::Builtin => "builtin",
            Self::Rules => "rules",
            Self::Translate => "translate",
            Self::WriteBack => "write_back",
            Self::Lua => "lua",
        }
    }

    const fn stage(self) -> DiagnosticStage {
        match self {
            Self::Init => DiagnosticStage::Init,
            Self::Extract | Self::Builtin | Self::Rules => DiagnosticStage::Extract,
            Self::Translate => DiagnosticStage::Translate,
            Self::WriteBack => DiagnosticStage::WriteBack,
            Self::Lua => DiagnosticStage::Lua,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimePanicBoundary {
    ProcessStartup,
    AfterCliParsing,
}

impl RuntimePanicBoundary {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ProcessStartup => "process_startup",
            Self::AfterCliParsing => "after_cli_parsing",
        }
    }
}

impl RuntimeOperation {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ValidateEnvironment => "validate_environment",
            Self::ResolveCurrentExecutable => "resolve_current_executable",
            Self::DetectAvailableParallelism => "detect_available_parallelism",
            Self::BuildAsyncRuntime => "build_async_runtime",
            Self::StartWorker => "start_worker",
            Self::ExecuteTask => "execute_task",
            Self::PrepareRpgMakerTranslationSnapshot => "prepare_rpg_maker_translation_snapshot",
            Self::DecodeRpgMakerTranslationUnits => "decode_rpg_maker_translation_units",
            Self::AssembleRpgMakerTranslationCorpus => "assemble_rpg_maker_translation_corpus",
            Self::EncodeRpgMakerTranslationResult => "encode_rpg_maker_translation_result",
            Self::PrepareRpgMakerPlanningResources => "prepare_rpg_maker_planning_resources",
            Self::CompileRpgMakerCustomPlaceholders => "compile_rpg_maker_custom_placeholders",
            Self::PrepareRpgMakerTranslationCorpus => "prepare_rpg_maker_translation_corpus",
            Self::PreprocessRpgMakerTranslationScopes => "preprocess_rpg_maker_translation_scopes",
            Self::DeduplicateRpgMakerTranslationCorpus => {
                "deduplicate_rpg_maker_translation_corpus"
            }
            Self::PlanRpgMakerTranslationScopes => "plan_rpg_maker_translation_scopes",
            Self::FinalizeRpgMakerTranslationPlan => "finalize_rpg_maker_translation_plan",
            Self::PrepareRpgMakerPrompt => "prepare_rpg_maker_prompt",
            Self::CompileRpgMakerBuiltinPlaceholders => "compile_rpg_maker_builtin_placeholders",
            Self::ReceiveTerminationSignal => "receive_termination_signal",
            Self::WriteStdout => "write_stdout",
            Self::WriteStderr => "write_stderr",
            Self::Shutdown => "shutdown",
            Self::StartTerminalProgressRenderer => "start_terminal_progress_renderer",
            Self::PublishTerminalProgressCompletion => "publish_terminal_progress_completion",
            Self::RenderTerminalProgressDynamicLine => "render_terminal_progress_dynamic_line",
            Self::RenderTerminalProgressStatus => "render_terminal_progress_status",
            Self::ClearTerminalProgressDynamicLine => "clear_terminal_progress_dynamic_line",
            Self::RenderTerminalProgressFinalMessage => "render_terminal_progress_final_message",
            Self::FinalizeTerminalProgress => "finalize_terminal_progress",
            Self::ReportTerminalProgressSafeStop => "report_terminal_progress_safe_stop",
            Self::FinishTerminalProgress => "finish_terminal_progress",
            Self::JoinTerminalProgressRenderer => "join_terminal_progress_renderer",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum RuntimeIssue {
    UnsupportedWindowsCodePage {
        expected: u32,
        actual: u32,
    },
    ProcessPanicked {
        boundary: RuntimePanicBoundary,
    },
    CommandPanicked {
        engine: RuntimeEngine,
        command: RuntimeCommand,
        project_workspace: SafePath,
        log_path: Option<SafePath>,
    },
    ResultPresentationPanicked {
        engine: RuntimeEngine,
        command: RuntimeCommand,
        project_workspace: SafePath,
        log_path: Option<SafePath>,
    },
    InternalInvariant {
        stage: DiagnosticStage,
        component: RuntimeComponent,
        operation: RuntimeBoundaryOperation,
    },
    TranslationTaskCountersInvalid {
        planned: u64,
        started: u64,
        complete: u64,
        partial: u64,
        unavailable: u64,
        failed: u64,
        cancelled: u64,
        not_started: u64,
        violation: TranslationTaskCounterInvariant,
    },
    Io {
        component: RuntimeComponent,
        operation: RuntimeOperation,
        failure: IoFailure,
    },
    ResourceLimit {
        component: RuntimeComponent,
        operation: RuntimeOperation,
        requested: usize,
        maximum: usize,
    },
    InvalidConfiguration {
        component: RuntimeComponent,
    },
    Cancelled {
        component: RuntimeComponent,
        operation: RuntimeOperation,
    },
    ExecutorClosed {
        component: RuntimeComponent,
        operation: RuntimeOperation,
    },
    StatePoisoned {
        component: RuntimeComponent,
        operation: RuntimeOperation,
    },
    WorkerPanicked {
        component: RuntimeComponent,
        operation: RuntimeOperation,
    },
    ConcurrentShutdown {
        component: RuntimeComponent,
    },
}

impl RuntimeIssue {
    pub(crate) const fn stage(&self) -> DiagnosticStage {
        match self {
            Self::UnsupportedWindowsCodePage { .. }
            | Self::ProcessPanicked {
                boundary: RuntimePanicBoundary::ProcessStartup,
            } => DiagnosticStage::ProcessStartup,
            Self::ProcessPanicked {
                boundary: RuntimePanicBoundary::AfterCliParsing,
            } => DiagnosticStage::Runtime,
            Self::CommandPanicked { command, .. } => command.stage(),
            Self::ResultPresentationPanicked { .. } => DiagnosticStage::ProcessOutput,
            Self::InternalInvariant { stage, .. } => *stage,
            Self::TranslationTaskCountersInvalid { .. } => DiagnosticStage::Translate,
            Self::Io {
                operation: RuntimeOperation::WriteStdout | RuntimeOperation::WriteStderr,
                ..
            } => DiagnosticStage::ProcessOutput,
            Self::Io {
                component: RuntimeComponent::TerminalProgress,
                ..
            }
            | Self::ExecutorClosed {
                component: RuntimeComponent::TerminalProgress,
                ..
            }
            | Self::WorkerPanicked {
                component: RuntimeComponent::TerminalProgress,
                ..
            } => DiagnosticStage::Shutdown,
            Self::ConcurrentShutdown { .. }
            | Self::Io {
                operation: RuntimeOperation::Shutdown,
                ..
            }
            | Self::ExecutorClosed {
                operation: RuntimeOperation::Shutdown,
                ..
            }
            | Self::StatePoisoned {
                operation: RuntimeOperation::Shutdown,
                ..
            }
            | Self::WorkerPanicked {
                operation: RuntimeOperation::Shutdown,
                ..
            } => DiagnosticStage::Shutdown,
            Self::Io {
                operation:
                    RuntimeOperation::ValidateEnvironment
                    | RuntimeOperation::ResolveCurrentExecutable
                    | RuntimeOperation::DetectAvailableParallelism
                    | RuntimeOperation::BuildAsyncRuntime
                    | RuntimeOperation::StartWorker,
                ..
            }
            | Self::ResourceLimit { .. }
            | Self::InvalidConfiguration { .. } => DiagnosticStage::ProcessStartup,
            Self::Io {
                operation:
                    RuntimeOperation::PrepareRpgMakerPrompt
                    | RuntimeOperation::CompileRpgMakerBuiltinPlaceholders,
                ..
            }
            | Self::ExecutorClosed {
                operation:
                    RuntimeOperation::PrepareRpgMakerPrompt
                    | RuntimeOperation::CompileRpgMakerBuiltinPlaceholders,
                ..
            }
            | Self::StatePoisoned {
                operation:
                    RuntimeOperation::PrepareRpgMakerPrompt
                    | RuntimeOperation::CompileRpgMakerBuiltinPlaceholders,
                ..
            }
            | Self::WorkerPanicked {
                operation:
                    RuntimeOperation::PrepareRpgMakerPrompt
                    | RuntimeOperation::CompileRpgMakerBuiltinPlaceholders,
                ..
            } => DiagnosticStage::CommandPreparation,
            Self::Io {
                operation:
                    RuntimeOperation::PrepareRpgMakerPlanningResources
                    | RuntimeOperation::CompileRpgMakerCustomPlaceholders
                    | RuntimeOperation::PrepareRpgMakerTranslationCorpus
                    | RuntimeOperation::PreprocessRpgMakerTranslationScopes
                    | RuntimeOperation::DeduplicateRpgMakerTranslationCorpus
                    | RuntimeOperation::PlanRpgMakerTranslationScopes
                    | RuntimeOperation::FinalizeRpgMakerTranslationPlan,
                ..
            }
            | Self::ExecutorClosed {
                operation:
                    RuntimeOperation::PrepareRpgMakerPlanningResources
                    | RuntimeOperation::CompileRpgMakerCustomPlaceholders
                    | RuntimeOperation::PrepareRpgMakerTranslationCorpus
                    | RuntimeOperation::PreprocessRpgMakerTranslationScopes
                    | RuntimeOperation::DeduplicateRpgMakerTranslationCorpus
                    | RuntimeOperation::PlanRpgMakerTranslationScopes
                    | RuntimeOperation::FinalizeRpgMakerTranslationPlan,
                ..
            }
            | Self::StatePoisoned {
                operation:
                    RuntimeOperation::PrepareRpgMakerPlanningResources
                    | RuntimeOperation::CompileRpgMakerCustomPlaceholders
                    | RuntimeOperation::PrepareRpgMakerTranslationCorpus
                    | RuntimeOperation::PreprocessRpgMakerTranslationScopes
                    | RuntimeOperation::DeduplicateRpgMakerTranslationCorpus
                    | RuntimeOperation::PlanRpgMakerTranslationScopes
                    | RuntimeOperation::FinalizeRpgMakerTranslationPlan,
                ..
            }
            | Self::WorkerPanicked {
                operation:
                    RuntimeOperation::PrepareRpgMakerPlanningResources
                    | RuntimeOperation::CompileRpgMakerCustomPlaceholders
                    | RuntimeOperation::PrepareRpgMakerTranslationCorpus
                    | RuntimeOperation::PreprocessRpgMakerTranslationScopes
                    | RuntimeOperation::DeduplicateRpgMakerTranslationCorpus
                    | RuntimeOperation::PlanRpgMakerTranslationScopes
                    | RuntimeOperation::FinalizeRpgMakerTranslationPlan,
                ..
            }
            | Self::Cancelled {
                operation:
                    RuntimeOperation::PrepareRpgMakerPlanningResources
                    | RuntimeOperation::CompileRpgMakerCustomPlaceholders
                    | RuntimeOperation::PrepareRpgMakerTranslationCorpus
                    | RuntimeOperation::PreprocessRpgMakerTranslationScopes
                    | RuntimeOperation::DeduplicateRpgMakerTranslationCorpus
                    | RuntimeOperation::PlanRpgMakerTranslationScopes
                    | RuntimeOperation::FinalizeRpgMakerTranslationPlan,
                ..
            } => DiagnosticStage::Translate,
            Self::Io {
                operation:
                    RuntimeOperation::PrepareRpgMakerTranslationSnapshot
                    | RuntimeOperation::DecodeRpgMakerTranslationUnits
                    | RuntimeOperation::AssembleRpgMakerTranslationCorpus
                    | RuntimeOperation::EncodeRpgMakerTranslationResult,
                ..
            }
            | Self::ExecutorClosed {
                operation:
                    RuntimeOperation::PrepareRpgMakerTranslationSnapshot
                    | RuntimeOperation::DecodeRpgMakerTranslationUnits
                    | RuntimeOperation::AssembleRpgMakerTranslationCorpus
                    | RuntimeOperation::EncodeRpgMakerTranslationResult,
                ..
            }
            | Self::StatePoisoned {
                operation:
                    RuntimeOperation::PrepareRpgMakerTranslationSnapshot
                    | RuntimeOperation::DecodeRpgMakerTranslationUnits
                    | RuntimeOperation::AssembleRpgMakerTranslationCorpus
                    | RuntimeOperation::EncodeRpgMakerTranslationResult,
                ..
            }
            | Self::WorkerPanicked {
                operation:
                    RuntimeOperation::PrepareRpgMakerTranslationSnapshot
                    | RuntimeOperation::DecodeRpgMakerTranslationUnits
                    | RuntimeOperation::AssembleRpgMakerTranslationCorpus
                    | RuntimeOperation::EncodeRpgMakerTranslationResult,
                ..
            }
            | Self::Cancelled {
                operation:
                    RuntimeOperation::PrepareRpgMakerTranslationSnapshot
                    | RuntimeOperation::DecodeRpgMakerTranslationUnits
                    | RuntimeOperation::AssembleRpgMakerTranslationCorpus
                    | RuntimeOperation::EncodeRpgMakerTranslationResult,
                ..
            } => DiagnosticStage::Translate,
            Self::Io {
                operation: RuntimeOperation::ReceiveTerminationSignal,
                ..
            } => DiagnosticStage::Shutdown,
            _ => DiagnosticStage::Runtime,
        }
    }

    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedWindowsCodePage { .. } => "runtime.windows_code_page",
            Self::ProcessPanicked { .. } => "runtime.process_panicked",
            Self::CommandPanicked { .. } => "runtime.command_panicked",
            Self::ResultPresentationPanicked { .. } => "runtime.result_presentation_panicked",
            Self::InternalInvariant { .. } => "runtime.internal_invariant",
            Self::TranslationTaskCountersInvalid { .. } => {
                "runtime.translation.task_counters_invalid"
            }
            Self::Io {
                component: RuntimeComponent::TerminalProgress,
                operation: RuntimeOperation::StartTerminalProgressRenderer,
                ..
            } => "runtime.terminal_progress_start",
            Self::Io {
                component: RuntimeComponent::TerminalProgress,
                ..
            } => "runtime.terminal_progress_io",
            Self::ExecutorClosed {
                component: RuntimeComponent::TerminalProgress,
                ..
            } => "runtime.terminal_progress_channel_closed",
            Self::WorkerPanicked {
                component: RuntimeComponent::TerminalProgress,
                ..
            } => "runtime.terminal_progress_renderer_panicked",
            Self::Io {
                operation: RuntimeOperation::ResolveCurrentExecutable,
                ..
            } => "runtime.current_executable",
            Self::Io {
                operation: RuntimeOperation::DetectAvailableParallelism,
                ..
            } => "runtime.available_parallelism",
            Self::Io {
                operation: RuntimeOperation::BuildAsyncRuntime,
                ..
            } => "runtime.async_runtime_build",
            Self::Io {
                operation: RuntimeOperation::StartWorker,
                ..
            } => "runtime.worker_start",
            Self::Io {
                operation: RuntimeOperation::WriteStdout,
                ..
            } => "runtime.stdout_write",
            Self::Io {
                operation: RuntimeOperation::WriteStderr,
                ..
            } => "runtime.stderr_write",
            Self::Io { .. } => "runtime.io",
            Self::ResourceLimit { .. } => "runtime.resource_limit",
            Self::InvalidConfiguration { .. } => "runtime.invalid_configuration",
            Self::Cancelled { .. } => "runtime.cancelled",
            Self::ExecutorClosed { .. } => "runtime.executor_closed",
            Self::StatePoisoned { .. } => "runtime.state_poisoned",
            Self::WorkerPanicked { .. } => "runtime.worker_panicked",
            Self::ConcurrentShutdown { .. } => "runtime.concurrent_shutdown",
        }
    }

    pub(crate) const fn resolution(&self) -> DiagnosticResolution {
        match self {
            Self::UnsupportedWindowsCodePage { .. }
            | Self::ProcessPanicked { .. }
            | Self::CommandPanicked { .. }
            | Self::ResultPresentationPanicked { .. } => DiagnosticResolution::ReportBug,
            Self::InternalInvariant { .. } | Self::TranslationTaskCountersInvalid { .. } => {
                DiagnosticResolution::ReportBug
            }
            Self::InvalidConfiguration { .. } => DiagnosticResolution::FixConfiguration,
            Self::StatePoisoned { .. } | Self::WorkerPanicked { .. } => {
                DiagnosticResolution::ReportBug
            }
            Self::ResourceLimit { .. } => DiagnosticResolution::ReportBug,
            Self::Io { .. }
            | Self::Cancelled { .. }
            | Self::ExecutorClosed { .. }
            | Self::ConcurrentShutdown { .. } => DiagnosticResolution::Retry,
        }
    }

    pub(crate) const fn summary_code(&self) -> &'static str {
        match self {
            Self::UnsupportedWindowsCodePage { .. } => "unsupported_windows_code_page",
            Self::ProcessPanicked { .. } => "internal_invariant",
            Self::CommandPanicked { .. } | Self::ResultPresentationPanicked { .. } => {
                "internal_invariant"
            }
            Self::InternalInvariant { .. } => "internal_invariant",
            Self::TranslationTaskCountersInvalid { .. } => "internal_invariant",
            Self::Io {
                operation: RuntimeOperation::StartWorker,
                ..
            } => "worker_spawn_failed",
            Self::Io { .. } => "external_service_unavailable",
            Self::ResourceLimit { .. } => "invalid_value",
            Self::InvalidConfiguration { .. } => "invalid_value",
            Self::Cancelled { .. } => "lock_cancelled",
            Self::ExecutorClosed { .. } => "executor_closed",
            Self::StatePoisoned { .. } => "executor_state_poisoned",
            Self::WorkerPanicked { .. } => "worker_panicked",
            Self::ConcurrentShutdown { .. } => "concurrent_shutdown",
        }
    }

    pub(crate) fn subject(&self) -> String {
        match self {
            Self::UnsupportedWindowsCodePage { .. } => {
                RuntimeComponent::WindowsUtf8Environment.as_str().to_owned()
            }
            Self::ProcessPanicked { .. } => RuntimeComponent::Process.as_str().to_owned(),
            Self::CommandPanicked {
                project_workspace, ..
            }
            | Self::ResultPresentationPanicked {
                project_workspace, ..
            } => project_workspace.to_string(),
            Self::InternalInvariant { component, .. } => component.as_str().to_owned(),
            Self::TranslationTaskCountersInvalid { .. } => "translation_task_results".to_owned(),
            Self::Io { component, .. }
            | Self::ResourceLimit { component, .. }
            | Self::InvalidConfiguration { component }
            | Self::Cancelled { component, .. }
            | Self::ExecutorClosed { component, .. }
            | Self::StatePoisoned { component, .. }
            | Self::WorkerPanicked { component, .. }
            | Self::ConcurrentShutdown { component } => component.as_str().to_owned(),
        }
    }

    pub(crate) fn facts(&self) -> Vec<(&'static str, String)> {
        if let Self::UnsupportedWindowsCodePage { expected, actual } = self {
            return vec![
                (
                    "component",
                    RuntimeComponent::WindowsUtf8Environment.as_str().to_owned(),
                ),
                (
                    "operation",
                    RuntimeOperation::ValidateEnvironment.as_str().to_owned(),
                ),
                ("expected_code_page", expected.to_string()),
                ("actual_code_page", actual.to_string()),
            ];
        }
        if let Self::ProcessPanicked { boundary } = self {
            return vec![
                ("component", RuntimeComponent::Process.as_str().to_owned()),
                ("boundary", boundary.as_str().to_owned()),
            ];
        }
        if let Self::InternalInvariant {
            component,
            operation,
            ..
        } = self
        {
            return vec![
                ("component", component.as_str().to_owned()),
                ("operation", operation.as_str().to_owned()),
            ];
        }
        if let Self::TranslationTaskCountersInvalid {
            planned,
            started,
            complete,
            partial,
            unavailable,
            failed,
            cancelled,
            not_started,
            violation,
        } = self
        {
            return vec![
                ("violation", violation.as_str().to_owned()),
                ("planned", planned.to_string()),
                ("started", started.to_string()),
                ("complete", complete.to_string()),
                ("partial", partial.to_string()),
                ("unavailable", unavailable.to_string()),
                ("failed", failed.to_string()),
                ("cancelled", cancelled.to_string()),
                ("not_started", not_started.to_string()),
            ];
        }
        if let Self::CommandPanicked {
            engine,
            command,
            project_workspace,
            log_path,
        }
        | Self::ResultPresentationPanicked {
            engine,
            command,
            project_workspace,
            log_path,
        } = self
        {
            let mut facts = vec![
                ("engine", engine.as_str().to_owned()),
                ("command", command.as_str().to_owned()),
                ("project_workspace", project_workspace.to_string()),
            ];
            if let Some(log_path) = log_path {
                facts.push(("log_path", log_path.to_string()));
            }
            return facts;
        }
        let (component, operation) = match self {
            Self::Io {
                component,
                operation,
                ..
            }
            | Self::ResourceLimit {
                component,
                operation,
                ..
            }
            | Self::Cancelled {
                component,
                operation,
            } => (*component, Some(*operation)),
            Self::ExecutorClosed {
                component,
                operation,
            }
            | Self::StatePoisoned {
                component,
                operation,
            }
            | Self::WorkerPanicked {
                component,
                operation,
            } => (*component, Some(*operation)),
            Self::InvalidConfiguration { component } | Self::ConcurrentShutdown { component } => {
                (*component, None)
            }
            Self::UnsupportedWindowsCodePage { .. }
            | Self::ProcessPanicked { .. }
            | Self::CommandPanicked { .. }
            | Self::ResultPresentationPanicked { .. } => {
                unreachable!("进程级问题已在上方单独处理")
            }
            Self::InternalInvariant { .. } => {
                unreachable!("内部契约问题已在上方单独处理")
            }
            Self::TranslationTaskCountersInvalid { .. } => {
                unreachable!("翻译任务计数问题已在上方单独处理")
            }
        };
        let mut facts = vec![("component", component.as_str().to_owned())];
        if let Some(operation) = operation {
            facts.push(("operation", operation.as_str().to_owned()));
        }
        match self {
            Self::Io { failure, .. } => {
                facts.push(("io_kind", failure.kind.as_str().to_owned()));
                if let Some(code) = failure.raw_os_code {
                    facts.push(("raw_os_code", code.to_string()));
                }
            }
            Self::ResourceLimit {
                requested, maximum, ..
            } => {
                facts.push(("requested", requested.to_string()));
                facts.push(("maximum", maximum.to_string()));
            }
            _ => {}
        }
        facts
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FileSystemOperation {
    Open,
    Read,
    Write,
    Create,
    Remove,
    Rename,
    Metadata,
    ListDirectory,
    AcquireExclusiveLease,
    ResolveDirectory,
    PrepareCandidate,
    RecoverTarget,
    FingerprintTree,
    WindowsOrdinalCaseKey,
    Cryptography,
    Shutdown,
}

impl FileSystemOperation {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Read => "read",
            Self::Write => "write",
            Self::Create => "create",
            Self::Remove => "remove",
            Self::Rename => "rename",
            Self::Metadata => "metadata",
            Self::ListDirectory => "list_directory",
            Self::AcquireExclusiveLease => "acquire_exclusive_lease",
            Self::ResolveDirectory => "resolve_directory",
            Self::PrepareCandidate => "prepare_candidate",
            Self::RecoverTarget => "recover_target",
            Self::FingerprintTree => "fingerprint_tree",
            Self::WindowsOrdinalCaseKey => "windows_ordinal_case_key",
            Self::Cryptography => "cryptography",
            Self::Shutdown => "shutdown",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FileSystemPathViolation {
    NotAbsolute,
    MissingParent,
    MissingFileName,
    NotRegularFile,
    NotDirectory,
    ReparsePoint,
    HardLink,
    CaseCollision,
    IdentityChanged,
    SourceChanged,
    InvalidWindowsName,
    ReservedWindowsName,
    OutsideScope,
    UnexpectedObject,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FileSystemOrdinalKeyPhase {
    Measure,
    Map,
}

impl FileSystemOrdinalKeyPhase {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Measure => "measure",
            Self::Map => "map",
        }
    }
}

impl FileSystemPathViolation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::NotAbsolute => "not_absolute",
            Self::MissingParent => "missing_parent",
            Self::MissingFileName => "missing_file_name",
            Self::NotRegularFile => "not_regular_file",
            Self::NotDirectory => "not_directory",
            Self::ReparsePoint => "reparse_point",
            Self::HardLink => "hard_link",
            Self::CaseCollision => "case_collision",
            Self::IdentityChanged => "identity_changed",
            Self::SourceChanged => "source_changed",
            Self::InvalidWindowsName => "invalid_windows_name",
            Self::ReservedWindowsName => "reserved_windows_name",
            Self::OutsideScope => "outside_scope",
            Self::UnexpectedObject => "unexpected_object",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FileSystemDiagnosticStage {
    ProcessStartup,
    CommandPreparation,
    Configuration,
    Project,
    Init,
    Extract,
    Translate,
    WriteBack,
    Publication,
    Logging,
    Shutdown,
}

impl FileSystemDiagnosticStage {
    const fn diagnostic_stage(self) -> DiagnosticStage {
        match self {
            Self::ProcessStartup => DiagnosticStage::ProcessStartup,
            Self::CommandPreparation => DiagnosticStage::CommandPreparation,
            Self::Configuration => DiagnosticStage::Configuration,
            Self::Project => DiagnosticStage::ProjectOpening,
            Self::Init => DiagnosticStage::Init,
            Self::Extract => DiagnosticStage::Extract,
            Self::Translate => DiagnosticStage::Translate,
            Self::WriteBack => DiagnosticStage::WriteBack,
            Self::Publication => DiagnosticStage::Publication,
            Self::Logging => DiagnosticStage::Logging,
            Self::Shutdown => DiagnosticStage::Shutdown,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileSystemDiagnosticContext {
    stage: FileSystemDiagnosticStage,
    operation: FileSystemOperation,
}

impl FileSystemDiagnosticContext {
    pub(crate) const fn new(
        stage: FileSystemDiagnosticStage,
        operation: FileSystemOperation,
    ) -> Self {
        Self { stage, operation }
    }

    pub(crate) const fn operation(self) -> FileSystemOperation {
        self.operation
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileSystemIssue {
    context: FileSystemDiagnosticContext,
    problem: FileSystemProblem,
}

impl FileSystemIssue {
    pub(crate) const fn new(
        context: FileSystemDiagnosticContext,
        problem: FileSystemProblem,
    ) -> Self {
        Self { context, problem }
    }

    pub(crate) const fn stage(&self) -> DiagnosticStage {
        self.context.stage.diagnostic_stage()
    }

    pub(crate) const fn code(&self) -> &'static str {
        self.problem.code()
    }

    pub(crate) const fn resolution(&self) -> DiagnosticResolution {
        self.problem.resolution()
    }

    pub(crate) const fn summary_code(&self) -> &'static str {
        self.problem.summary_code()
    }

    pub(crate) fn subject(&self) -> String {
        self.problem.subject()
    }

    pub(crate) fn facts(&self) -> Vec<(&'static str, String)> {
        let mut facts = self.problem.facts();
        facts.insert(0, ("operation", self.context.operation.as_str().to_owned()));
        facts
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum FileSystemJournalViolation {
    Serialization {
        category: SafeIdentifier,
        line: u64,
        column: u64,
    },
    FrameLengthOverflow {
        actual: u64,
        maximum: u64,
    },
    NotRegularFile,
    CrcMismatch {
        frame_index: u64,
    },
    InvalidJson {
        frame_index: u64,
        category: SafeIdentifier,
        line: u64,
        column: u64,
    },
    InvalidOperationId {
        frame_index: u64,
    },
    NonCanonicalOperationId {
        frame_index: u64,
    },
    FrameIdentityMismatch {
        frame_index: u64,
    },
    ExtraFrame {
        frame_index: u64,
    },
    PhaseOrder {
        frame_index: u64,
        expected: SafeIdentifier,
        actual: SafeIdentifier,
    },
    InvalidArtifactFileName,
    ArtifactIdentityMismatch,
    ArtifactNamesMismatch,
}

impl FileSystemJournalViolation {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Serialization { .. } => "serialization",
            Self::FrameLengthOverflow { .. } => "frame_length_overflow",
            Self::NotRegularFile => "not_regular_file",
            Self::CrcMismatch { .. } => "crc_mismatch",
            Self::InvalidJson { .. } => "invalid_json",
            Self::InvalidOperationId { .. } => "invalid_operation_id",
            Self::NonCanonicalOperationId { .. } => "noncanonical_operation_id",
            Self::FrameIdentityMismatch { .. } => "frame_identity_mismatch",
            Self::ExtraFrame { .. } => "extra_frame",
            Self::PhaseOrder { .. } => "phase_order",
            Self::InvalidArtifactFileName => "invalid_artifact_file_name",
            Self::ArtifactIdentityMismatch => "artifact_identity_mismatch",
            Self::ArtifactNamesMismatch => "artifact_names_mismatch",
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        let mut facts = vec![("journal_violation", self.as_str().to_owned())];
        match self {
            Self::Serialization {
                category,
                line,
                column,
            } => {
                facts.push(("json_category", category.to_string()));
                facts.push(("line", line.to_string()));
                facts.push(("column", column.to_string()));
            }
            Self::FrameLengthOverflow { actual, maximum } => {
                facts.push(("actual", actual.to_string()));
                facts.push(("maximum", maximum.to_string()));
            }
            Self::CrcMismatch { frame_index }
            | Self::InvalidOperationId { frame_index }
            | Self::NonCanonicalOperationId { frame_index }
            | Self::FrameIdentityMismatch { frame_index }
            | Self::ExtraFrame { frame_index } => {
                facts.push(("frame_index", frame_index.to_string()));
            }
            Self::InvalidJson {
                frame_index,
                category,
                line,
                column,
            } => {
                facts.push(("frame_index", frame_index.to_string()));
                facts.push(("json_category", category.to_string()));
                facts.push(("line", line.to_string()));
                facts.push(("column", column.to_string()));
            }
            Self::PhaseOrder {
                frame_index,
                expected,
                actual,
            } => {
                facts.push(("frame_index", frame_index.to_string()));
                facts.push(("expected_phase", expected.to_string()));
                facts.push(("actual_phase", actual.to_string()));
            }
            Self::NotRegularFile
            | Self::InvalidArtifactFileName
            | Self::ArtifactIdentityMismatch
            | Self::ArtifactNamesMismatch => {}
        }
        facts
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FileSystemRecoveryViolation {
    ObservationFailed,
    ArtifactReparsePoint,
    UnexpectedResidualArtifact,
    TargetNameMismatch,
    TargetIdentityUnknown,
    RestoredIdentityMismatch,
    BackupIdentityUnknown,
    OriginalAndTargetMissing,
}

impl FileSystemRecoveryViolation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ObservationFailed => "observation_failed",
            Self::ArtifactReparsePoint => "artifact_reparse_point",
            Self::UnexpectedResidualArtifact => "unexpected_residual_artifact",
            Self::TargetNameMismatch => "target_name_mismatch",
            Self::TargetIdentityUnknown => "target_identity_unknown",
            Self::RestoredIdentityMismatch => "restored_identity_mismatch",
            Self::BackupIdentityUnknown => "backup_identity_unknown",
            Self::OriginalAndTargetMissing => "original_and_target_missing",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum FileSystemProblem {
    NotFound {
        path: SafePath,
    },
    NotDirectory {
        path: SafePath,
    },
    NotFile {
        path: SafePath,
    },
    Io {
        path: SafePath,
        failure: IoFailure,
    },
    InvalidPath {
        path: SafePath,
        violation: FileSystemPathViolation,
    },
    HardLink {
        path: SafePath,
        link_count: u32,
    },
    CaseCollision {
        first_path: SafePath,
        second_path: SafePath,
    },
    OutsideScope {
        root: SafePath,
        path: SafePath,
    },
    UnexpectedObject {
        path: SafePath,
    },
    ReparsePoint {
        path: SafePath,
    },
    NonLocalVolume {
        path: SafePath,
    },
    NonNtfsVolume {
        path: SafePath,
        actual: SafeText,
    },
    CaseSensitiveDirectory {
        path: SafePath,
    },
    Cancelled {
        path: SafePath,
    },
    TargetExists {
        path: SafePath,
    },
    IdentityChanged {
        path: SafePath,
    },
    WindowsStatus {
        operation: FileSystemOperation,
        status: i32,
    },
    ExecutorClosed,
    WorkerPanicked,
    WrongPublisherInstance,
    RollbackFailed {
        path: SafePath,
    },
    CleanupFailed {
        path: SafePath,
    },
    JournalCorrupt {
        path: SafePath,
        artifacts: Vec<SafePath>,
        violation: FileSystemJournalViolation,
    },
    RecoveryRequired {
        target_root: SafePath,
        artifacts: Vec<SafePath>,
        violation: FileSystemRecoveryViolation,
    },
    RecoveryCleanupFailed {
        target_root: SafePath,
        artifacts: Vec<SafePath>,
    },
    OutcomeUnknown {
        target_root: SafePath,
        artifacts: Vec<SafePath>,
        violation: FileSystemRecoveryViolation,
    },
    OrdinalKeyTooLarge {
        path: SafePath,
        observed: u64,
        maximum: u64,
    },
    OrdinalKeyIo {
        path: SafePath,
        phase: FileSystemOrdinalKeyPhase,
        failure: IoFailure,
    },
}

impl FileSystemProblem {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "filesystem.not_found",
            Self::NotDirectory { .. } => "filesystem.not_directory",
            Self::NotFile { .. } => "filesystem.not_file",
            Self::Io { .. } => "filesystem.io",
            Self::InvalidPath { .. } => "filesystem.invalid_path",
            Self::HardLink { .. } => "filesystem.hard_link",
            Self::CaseCollision { .. } => "filesystem.case_collision",
            Self::OutsideScope { .. } => "filesystem.outside_scope",
            Self::UnexpectedObject { .. } => "filesystem.unexpected_object",
            Self::ReparsePoint { .. } => "filesystem.reparse_point",
            Self::NonLocalVolume { .. } => "filesystem.non_local_volume",
            Self::NonNtfsVolume { .. } => "filesystem.non_ntfs_volume",
            Self::CaseSensitiveDirectory { .. } => "filesystem.case_sensitive_directory",
            Self::Cancelled { .. } => "filesystem.cancelled",
            Self::TargetExists { .. } => "filesystem.target_exists",
            Self::IdentityChanged { .. } => "filesystem.identity_changed",
            Self::WindowsStatus { .. } => "filesystem.windows_status",
            Self::ExecutorClosed => "filesystem.executor_closed",
            Self::WorkerPanicked => "filesystem.worker_panicked",
            Self::WrongPublisherInstance => "filesystem.wrong_publisher_instance",
            Self::RollbackFailed { .. } => "filesystem.rollback_failed",
            Self::CleanupFailed { .. } => "filesystem.cleanup_failed",
            Self::JournalCorrupt { .. } => "filesystem.journal_corrupt",
            Self::RecoveryRequired { .. } => "filesystem.recovery_required",
            Self::RecoveryCleanupFailed { .. } => "filesystem.recovery_cleanup_failed",
            Self::OutcomeUnknown { .. } => "filesystem.outcome_unknown",
            Self::OrdinalKeyTooLarge { .. } => "filesystem.ordinal_key_too_large",
            Self::OrdinalKeyIo { .. } => "filesystem.ordinal_key_io",
        }
    }

    pub(crate) const fn resolution(&self) -> DiagnosticResolution {
        match self {
            Self::NotFound { .. }
            | Self::NotDirectory { .. }
            | Self::NotFile { .. }
            | Self::Io { .. }
            | Self::ReparsePoint { .. }
            | Self::NonLocalVolume { .. }
            | Self::NonNtfsVolume { .. }
            | Self::CaseSensitiveDirectory { .. }
            | Self::TargetExists { .. } => DiagnosticResolution::CheckPathAndPermissions,
            Self::HardLink { .. }
            | Self::CaseCollision { .. }
            | Self::OutsideScope { .. }
            | Self::UnexpectedObject { .. } => DiagnosticResolution::FixInput,
            Self::Cancelled { .. } | Self::ExecutorClosed => DiagnosticResolution::Retry,
            Self::RecoveryRequired { .. }
            | Self::RecoveryCleanupFailed { .. }
            | Self::OutcomeUnknown { .. }
            | Self::RollbackFailed { .. }
            | Self::CleanupFailed { .. }
            | Self::JournalCorrupt { .. }
            | Self::IdentityChanged { .. } => DiagnosticResolution::PreserveRecoveryArtifacts,
            Self::InvalidPath { .. } | Self::WrongPublisherInstance | Self::WorkerPanicked => {
                DiagnosticResolution::ReportBug
            }
            Self::WindowsStatus { .. } => DiagnosticResolution::Retry,
            Self::OrdinalKeyTooLarge { .. } => DiagnosticResolution::FixInput,
            Self::OrdinalKeyIo { .. } => DiagnosticResolution::CheckPathAndPermissions,
        }
    }

    pub(crate) const fn summary_code(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "not_found",
            Self::NotDirectory { .. } | Self::NotFile { .. } => "invalid_path",
            Self::Io { .. } => "external_service_unavailable",
            Self::InvalidPath { .. } => "invalid_path",
            Self::HardLink { .. }
            | Self::CaseCollision { .. }
            | Self::OutsideScope { .. }
            | Self::UnexpectedObject { .. } => "invalid_path",
            Self::ReparsePoint { .. } => "reparse_point_forbidden",
            Self::NonLocalVolume { .. } => "non_local_volume",
            Self::NonNtfsVolume { .. } => "non_ntfs_volume",
            Self::CaseSensitiveDirectory { .. } => "case_sensitive_directory",
            Self::Cancelled { .. } => "lock_cancelled",
            Self::TargetExists { .. } => "target_already_exists",
            Self::IdentityChanged { .. } => "file_identity_changed",
            Self::WindowsStatus { .. } => "external_service_unavailable",
            Self::ExecutorClosed => "executor_closed",
            Self::WorkerPanicked => "worker_panicked",
            Self::WrongPublisherInstance => "wrong_publisher_instance",
            Self::RollbackFailed { .. } => "rollback_failed",
            Self::CleanupFailed { .. } => "finalization_failed",
            Self::JournalCorrupt { .. } => "journal_corrupt",
            Self::RecoveryRequired { .. } => "write_back_recovery_required",
            Self::RecoveryCleanupFailed { .. } => "finalization_failed",
            Self::OutcomeUnknown { .. } => "transaction_outcome_unknown",
            Self::OrdinalKeyTooLarge { .. } => "resource_limit_exceeded",
            Self::OrdinalKeyIo { .. } => "external_service_unavailable",
        }
    }

    pub(crate) fn subject(&self) -> String {
        match self {
            Self::NotFound { path }
            | Self::NotDirectory { path }
            | Self::NotFile { path }
            | Self::Io { path, .. }
            | Self::InvalidPath { path, .. }
            | Self::HardLink { path, .. }
            | Self::OutsideScope { path, .. }
            | Self::UnexpectedObject { path }
            | Self::ReparsePoint { path }
            | Self::NonLocalVolume { path }
            | Self::NonNtfsVolume { path, .. }
            | Self::CaseSensitiveDirectory { path }
            | Self::Cancelled { path, .. }
            | Self::TargetExists { path }
            | Self::IdentityChanged { path }
            | Self::RollbackFailed { path }
            | Self::CleanupFailed { path }
            | Self::JournalCorrupt { path, .. }
            | Self::OrdinalKeyTooLarge { path, .. }
            | Self::OrdinalKeyIo { path, .. } => path.to_string(),
            Self::CaseCollision { second_path, .. } => second_path.to_string(),
            Self::RecoveryRequired { target_root, .. }
            | Self::RecoveryCleanupFailed { target_root, .. }
            | Self::OutcomeUnknown { target_root, .. } => target_root.to_string(),
            Self::WindowsStatus { operation, .. } => operation.as_str().to_owned(),
            Self::ExecutorClosed | Self::WorkerPanicked => "filesystem_executor".to_owned(),
            Self::WrongPublisherInstance => "directory_publisher".to_owned(),
        }
    }

    pub(crate) fn facts(&self) -> Vec<(&'static str, String)> {
        let mut facts = Vec::new();
        match self {
            Self::NotFound { path } | Self::NotDirectory { path } | Self::NotFile { path } => {
                facts.push(("path", path.to_string()))
            }
            Self::Io { path, failure } => {
                facts.push(("path", path.to_string()));
                facts.push(("io_kind", failure.kind.as_str().to_owned()));
                if let Some(code) = failure.raw_os_code {
                    facts.push(("raw_os_code", code.to_string()));
                }
            }
            Self::InvalidPath { path, violation } => {
                facts.push(("path", path.to_string()));
                facts.push(("violation", violation.as_str().to_owned()));
            }
            Self::HardLink { path, link_count } => {
                facts.push(("path", path.to_string()));
                facts.push(("link_count", link_count.to_string()));
            }
            Self::CaseCollision {
                first_path,
                second_path,
            } => {
                facts.push(("first_path", first_path.to_string()));
                facts.push(("second_path", second_path.to_string()));
            }
            Self::OutsideScope { root, path } => {
                facts.push(("root", root.to_string()));
                facts.push(("path", path.to_string()));
            }
            Self::UnexpectedObject { path } => facts.push(("path", path.to_string())),
            Self::NonNtfsVolume { path, actual } => {
                facts.push(("path", path.to_string()));
                facts.push(("filesystem", actual.to_string()));
            }
            Self::WindowsStatus { operation, status } => {
                facts.push(("operation", operation.as_str().to_owned()));
                facts.push(("status", status.to_string()));
            }
            Self::RecoveryRequired {
                target_root,
                artifacts,
                violation,
            }
            | Self::OutcomeUnknown {
                target_root,
                artifacts,
                violation,
            } => {
                facts.push(("target_root", target_root.to_string()));
                facts.push(("artifacts", join_paths(artifacts)));
                facts.push(("recovery_violation", violation.as_str().to_owned()));
            }
            Self::RecoveryCleanupFailed {
                target_root,
                artifacts,
            } => {
                facts.push(("target_root", target_root.to_string()));
                facts.push(("artifacts", join_paths(artifacts)));
            }
            Self::JournalCorrupt {
                path,
                artifacts,
                violation,
            } => {
                facts.push(("path", path.to_string()));
                facts.push(("artifacts", join_paths(artifacts)));
                facts.extend(violation.facts());
            }
            Self::OrdinalKeyTooLarge {
                path,
                observed,
                maximum,
            } => {
                facts.push(("path", path.to_string()));
                facts.push(("observed", observed.to_string()));
                facts.push(("maximum", maximum.to_string()));
            }
            Self::OrdinalKeyIo {
                path,
                phase,
                failure,
            } => {
                facts.push(("path", path.to_string()));
                facts.push(("phase", phase.as_str().to_owned()));
                facts.push(("io_kind", failure.kind.as_str().to_owned()));
                if let Some(code) = failure.raw_os_code {
                    facts.push(("raw_os_code", code.to_string()));
                }
            }
            _ => {}
        }
        facts
    }
}

fn join_paths(paths: &[SafePath]) -> String {
    paths
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SqliteTransactionState {
    NotStarted,
    Active,
    Committed,
    RolledBack,
    FinalizationFailed,
    OutcomeUnknown,
}

impl SqliteTransactionState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::Active => "active",
            Self::Committed => "committed",
            Self::RolledBack => "rolled_back",
            Self::FinalizationFailed => "finalization_failed",
            Self::OutcomeUnknown => "outcome_unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SqliteOperation {
    DetectAvailableParallelism,
    StartWorker,
    Open,
    Execute,
    Query,
    Transaction,
    Backup,
    Cleanup,
    Shutdown,
}

impl SqliteOperation {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::DetectAvailableParallelism => "detect_available_parallelism",
            Self::StartWorker => "start_worker",
            Self::Open => "open",
            Self::Execute => "execute",
            Self::Query => "query",
            Self::Transaction => "transaction",
            Self::Backup => "backup",
            Self::Cleanup => "cleanup",
            Self::Shutdown => "shutdown",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SqliteDriverKind {
    SqliteFailure,
    SingleThreadedMode,
    ColumnConversion,
    IntegralOutOfRange,
    InvalidUtf8,
    EmbeddedNul,
    InvalidParameterName,
    InvalidPath,
    ExecuteReturnedRows,
    NoRows,
    MultipleRows,
    InvalidColumnIndex,
    InvalidColumnName,
    InvalidColumnType,
    UnexpectedChangedRows,
    ParameterConversion,
    InvalidQuery,
    CallbackPanicked,
    MultipleStatements,
    InvalidParameterCount,
    SqlInput,
    InvalidDatabaseIndex,
    RusqliteNonSqliteFailure,
}

impl SqliteDriverKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::SqliteFailure => "sqlite_failure",
            Self::SingleThreadedMode => "single_threaded_mode",
            Self::ColumnConversion => "column_conversion",
            Self::IntegralOutOfRange => "integral_out_of_range",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::EmbeddedNul => "embedded_nul",
            Self::InvalidParameterName => "invalid_parameter_name",
            Self::InvalidPath => "invalid_path",
            Self::ExecuteReturnedRows => "execute_returned_rows",
            Self::NoRows => "no_rows",
            Self::MultipleRows => "multiple_rows",
            Self::InvalidColumnIndex => "invalid_column_index",
            Self::InvalidColumnName => "invalid_column_name",
            Self::InvalidColumnType => "invalid_column_type",
            Self::UnexpectedChangedRows => "unexpected_changed_rows",
            Self::ParameterConversion => "parameter_conversion",
            Self::InvalidQuery => "invalid_query",
            Self::CallbackPanicked => "callback_panicked",
            Self::MultipleStatements => "multiple_statements",
            Self::InvalidParameterCount => "invalid_parameter_count",
            Self::SqlInput => "sql_input",
            Self::InvalidDatabaseIndex => "invalid_database_index",
            Self::RusqliteNonSqliteFailure => "rusqlite_non_sqlite_failure",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SqliteDriverFailure {
    pub(crate) kind: SqliteDriverKind,
    pub(crate) primary_code: Option<i32>,
    pub(crate) extended_code: Option<i32>,
    pub(crate) column_index: Option<usize>,
    pub(crate) column_name: Option<SafeIdentifier>,
    pub(crate) parameter_actual: Option<usize>,
    pub(crate) parameter_expected: Option<usize>,
    pub(crate) changed_rows: Option<usize>,
    pub(crate) sql_offset: Option<i32>,
    pub(crate) database_index: Option<usize>,
}

impl SqliteDriverFailure {
    /// 在 rusqlite 边界保留闭集类别、数值代码和结构位置，不读取 Display 文本。
    pub(crate) fn from_error(source: &rusqlite::Error) -> Self {
        let (kind, column_index, column_name, parameter_actual, parameter_expected, changed_rows) =
            match source {
                rusqlite::Error::SqliteFailure(_, _) => (
                    SqliteDriverKind::SqliteFailure,
                    None,
                    None,
                    None,
                    None,
                    None,
                ),
                rusqlite::Error::SqliteSingleThreadedMode => (
                    SqliteDriverKind::SingleThreadedMode,
                    None,
                    None,
                    None,
                    None,
                    None,
                ),
                rusqlite::Error::FromSqlConversionFailure(index, _, _) => (
                    SqliteDriverKind::ColumnConversion,
                    Some(*index),
                    None,
                    None,
                    None,
                    None,
                ),
                rusqlite::Error::IntegralValueOutOfRange(index, _) => (
                    SqliteDriverKind::IntegralOutOfRange,
                    Some(*index),
                    None,
                    None,
                    None,
                    None,
                ),
                rusqlite::Error::Utf8Error(index, _) => (
                    SqliteDriverKind::InvalidUtf8,
                    Some(*index),
                    None,
                    None,
                    None,
                    None,
                ),
                rusqlite::Error::NulError(_) => {
                    (SqliteDriverKind::EmbeddedNul, None, None, None, None, None)
                }
                rusqlite::Error::InvalidParameterName(_) => (
                    SqliteDriverKind::InvalidParameterName,
                    None,
                    None,
                    None,
                    None,
                    None,
                ),
                rusqlite::Error::InvalidPath(_) => {
                    (SqliteDriverKind::InvalidPath, None, None, None, None, None)
                }
                rusqlite::Error::ExecuteReturnedResults => (
                    SqliteDriverKind::ExecuteReturnedRows,
                    None,
                    None,
                    None,
                    None,
                    None,
                ),
                rusqlite::Error::QueryReturnedNoRows => {
                    (SqliteDriverKind::NoRows, None, None, None, None, None)
                }
                rusqlite::Error::QueryReturnedMoreThanOneRow => {
                    (SqliteDriverKind::MultipleRows, None, None, None, None, None)
                }
                rusqlite::Error::InvalidColumnIndex(index) => (
                    SqliteDriverKind::InvalidColumnIndex,
                    Some(*index),
                    None,
                    None,
                    None,
                    None,
                ),
                rusqlite::Error::InvalidColumnName(name) => (
                    SqliteDriverKind::InvalidColumnName,
                    None,
                    SafeIdentifier::new(name).ok(),
                    None,
                    None,
                    None,
                ),
                rusqlite::Error::InvalidColumnType(index, name, _) => (
                    SqliteDriverKind::InvalidColumnType,
                    Some(*index),
                    SafeIdentifier::new(name).ok(),
                    None,
                    None,
                    None,
                ),
                rusqlite::Error::StatementChangedRows(actual) => (
                    SqliteDriverKind::UnexpectedChangedRows,
                    None,
                    None,
                    None,
                    None,
                    Some(*actual),
                ),
                rusqlite::Error::ToSqlConversionFailure(_) => (
                    SqliteDriverKind::ParameterConversion,
                    None,
                    None,
                    None,
                    None,
                    None,
                ),
                rusqlite::Error::InvalidQuery => {
                    (SqliteDriverKind::InvalidQuery, None, None, None, None, None)
                }
                rusqlite::Error::UnwindingPanic => (
                    SqliteDriverKind::CallbackPanicked,
                    None,
                    None,
                    None,
                    None,
                    None,
                ),
                rusqlite::Error::MultipleStatement => (
                    SqliteDriverKind::MultipleStatements,
                    None,
                    None,
                    None,
                    None,
                    None,
                ),
                rusqlite::Error::InvalidParameterCount(actual, expected) => (
                    SqliteDriverKind::InvalidParameterCount,
                    None,
                    None,
                    Some(*actual),
                    Some(*expected),
                    None,
                ),
                rusqlite::Error::SqlInputError { .. } => {
                    (SqliteDriverKind::SqlInput, None, None, None, None, None)
                }
                rusqlite::Error::InvalidDatabaseIndex(_) => (
                    SqliteDriverKind::InvalidDatabaseIndex,
                    None,
                    None,
                    None,
                    None,
                    None,
                ),
                _ => (
                    SqliteDriverKind::RusqliteNonSqliteFailure,
                    None,
                    None,
                    None,
                    None,
                    None,
                ),
            };
        let (primary_code, extended_code) = source.sqlite_error().map_or((None, None), |error| {
            (Some(error.extended_code & 0xff), Some(error.extended_code))
        });
        Self {
            kind,
            primary_code,
            extended_code,
            column_index,
            column_name,
            parameter_actual,
            parameter_expected,
            changed_rows,
            sql_offset: match source {
                rusqlite::Error::SqlInputError { offset, .. } => Some(*offset),
                _ => None,
            },
            database_index: match source {
                rusqlite::Error::InvalidDatabaseIndex(index) => Some(*index),
                _ => None,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SqliteDiagnosticStage {
    ProcessStartup,
    CommandPreparation,
    Project,
    Init,
    Extract,
    Translate,
    WriteBack,
    Lua,
    RunPlanFinalization,
    Publication,
    Shutdown,
    Runtime,
}

impl SqliteDiagnosticStage {
    const fn diagnostic_stage(self) -> DiagnosticStage {
        match self {
            Self::ProcessStartup => DiagnosticStage::ProcessStartup,
            Self::CommandPreparation => DiagnosticStage::CommandPreparation,
            Self::Project => DiagnosticStage::ProjectOpening,
            Self::Init => DiagnosticStage::Init,
            Self::Extract => DiagnosticStage::Extract,
            Self::Translate => DiagnosticStage::Translate,
            Self::WriteBack => DiagnosticStage::WriteBack,
            Self::Lua => DiagnosticStage::Lua,
            Self::RunPlanFinalization => DiagnosticStage::RunPlanFinalization,
            Self::Publication => DiagnosticStage::Publication,
            Self::Shutdown => DiagnosticStage::Shutdown,
            Self::Runtime => DiagnosticStage::Runtime,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SqliteDiagnosticContext {
    stage: SqliteDiagnosticStage,
    operation: SqliteOperation,
    transaction: SqliteTransactionState,
}

impl SqliteDiagnosticContext {
    pub(crate) const fn new(
        stage: SqliteDiagnosticStage,
        operation: SqliteOperation,
        transaction: SqliteTransactionState,
    ) -> Self {
        Self {
            stage,
            operation,
            transaction,
        }
    }

    pub(crate) const fn with_operation(self, operation: SqliteOperation) -> Self {
        Self { operation, ..self }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SqliteIssue {
    context: SqliteDiagnosticContext,
    problem: SqliteProblem,
}

impl SqliteIssue {
    pub(crate) const fn new(context: SqliteDiagnosticContext, problem: SqliteProblem) -> Self {
        Self { context, problem }
    }

    pub(crate) const fn stage(&self) -> DiagnosticStage {
        self.context.stage.diagnostic_stage()
    }

    pub(crate) const fn code(&self) -> &'static str {
        self.problem.code()
    }
    pub(crate) const fn resolution(&self) -> DiagnosticResolution {
        self.problem.resolution()
    }
    pub(crate) const fn summary_code(&self) -> &'static str {
        self.problem.summary_code()
    }
    pub(crate) fn subject(&self) -> String {
        self.problem.subject()
    }
    pub(crate) fn facts(&self) -> Vec<(&'static str, String)> {
        let mut facts = self.problem.facts();
        facts.insert(
            0,
            ("transaction", self.context.transaction.as_str().to_owned()),
        );
        facts.insert(0, ("operation", self.context.operation.as_str().to_owned()));
        facts
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum SqliteProblem {
    RootDriver {
        query_id: Option<SafeIdentifier>,
        query_ordinal: Option<usize>,
        failure: SqliteDriverFailure,
    },
    RootInteractiveSessionAlreadyOpen,
    RootInvalidValue,
    RootInternalInvariant,
    RootBackupIncomplete,
    Io {
        database: SafePath,
        failure: IoFailure,
    },
    Driver {
        database: SafePath,
        query_id: Option<SafeIdentifier>,
        query_ordinal: Option<usize>,
        failure: SqliteDriverFailure,
    },
    Cancelled {
        database: SafePath,
    },
    ExecutorClosed {
        database: SafePath,
    },
    InteractiveSessionAlreadyOpen {
        database: SafePath,
    },
    WorkerStart {
        database: SafePath,
        failure: IoFailure,
    },
    WorkerPanicked {
        database: SafePath,
    },
    InvalidTarget {
        database: SafePath,
    },
    UnexpectedArtifact {
        database: SafePath,
    },
    InvalidValue {
        database: SafePath,
    },
    InternalInvariant {
        database: SafePath,
    },
    BackupIncomplete {
        database: SafePath,
    },
}

impl SqliteProblem {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::RootDriver { .. } => "sqlite.root_driver",
            Self::RootInteractiveSessionAlreadyOpen => {
                "sqlite.root_interactive_session_already_open"
            }
            Self::RootInvalidValue => "sqlite.root_invalid_value",
            Self::RootInternalInvariant => "sqlite.root_internal_invariant",
            Self::RootBackupIncomplete => "sqlite.root_backup_incomplete",
            Self::Io { .. } => "sqlite.io",
            Self::Driver { .. } => "sqlite.driver",
            Self::Cancelled { .. } => "sqlite.cancelled",
            Self::ExecutorClosed { .. } => "sqlite.executor_closed",
            Self::InteractiveSessionAlreadyOpen { .. } => "sqlite.interactive_session_open",
            Self::WorkerStart { .. } => "sqlite.worker_start",
            Self::WorkerPanicked { .. } => "sqlite.worker_panicked",
            Self::InvalidTarget { .. } => "sqlite.invalid_target",
            Self::UnexpectedArtifact { .. } => "sqlite.unexpected_artifact",
            Self::InvalidValue { .. } => "sqlite.invalid_value",
            Self::InternalInvariant { .. } => "sqlite.internal_invariant",
            Self::BackupIncomplete { .. } => "sqlite.backup_incomplete",
        }
    }

    pub(crate) const fn resolution(&self) -> DiagnosticResolution {
        match self {
            Self::RootDriver { .. } | Self::RootBackupIncomplete => DiagnosticResolution::Retry,
            Self::RootInteractiveSessionAlreadyOpen => DiagnosticResolution::ResolveContention,
            Self::RootInvalidValue => DiagnosticResolution::FixInput,
            Self::RootInternalInvariant => DiagnosticResolution::ReportBug,
            Self::Io { .. } | Self::InvalidTarget { .. } => {
                DiagnosticResolution::CheckPathAndPermissions
            }
            Self::Driver { .. }
            | Self::Cancelled { .. }
            | Self::ExecutorClosed { .. }
            | Self::WorkerStart { .. }
            | Self::BackupIncomplete { .. } => DiagnosticResolution::Retry,
            Self::InteractiveSessionAlreadyOpen { .. } => DiagnosticResolution::ResolveContention,
            Self::UnexpectedArtifact { .. } => DiagnosticResolution::CheckProjectState,
            Self::InvalidValue { .. } => DiagnosticResolution::FixInput,
            Self::WorkerPanicked { .. } | Self::InternalInvariant { .. } => {
                DiagnosticResolution::ReportBug
            }
        }
    }

    pub(crate) const fn summary_code(&self) -> &'static str {
        match self {
            Self::RootDriver { .. } => "invalid_value",
            Self::RootInteractiveSessionAlreadyOpen => "interactive_session_already_open",
            Self::RootInvalidValue => "invalid_value",
            Self::RootInternalInvariant => "internal_invariant",
            Self::RootBackupIncomplete => "backup_incomplete",
            Self::Io { .. } => "external_service_unavailable",
            Self::Driver { .. } => "invalid_value",
            Self::Cancelled { .. } => "lock_cancelled",
            Self::ExecutorClosed { .. } => "executor_closed",
            Self::InteractiveSessionAlreadyOpen { .. } => "interactive_session_already_open",
            Self::WorkerStart { .. } => "worker_spawn_failed",
            Self::WorkerPanicked { .. } => "worker_panicked",
            Self::InvalidTarget { .. } => "invalid_path",
            Self::UnexpectedArtifact { .. } => "unexpected_artifact",
            Self::InvalidValue { .. } => "invalid_value",
            Self::InternalInvariant { .. } => "internal_invariant",
            Self::BackupIncomplete { .. } => "backup_incomplete",
        }
    }

    pub(crate) fn subject(&self) -> String {
        match self {
            Self::RootDriver { .. }
            | Self::RootInteractiveSessionAlreadyOpen
            | Self::RootInvalidValue
            | Self::RootInternalInvariant
            | Self::RootBackupIncomplete => "sqlite_executor".to_owned(),
            Self::Io { database, .. }
            | Self::Driver { database, .. }
            | Self::Cancelled { database, .. }
            | Self::ExecutorClosed { database }
            | Self::InteractiveSessionAlreadyOpen { database }
            | Self::WorkerStart { database, .. }
            | Self::WorkerPanicked { database }
            | Self::InvalidTarget { database }
            | Self::UnexpectedArtifact { database }
            | Self::InvalidValue { database }
            | Self::InternalInvariant { database, .. }
            | Self::BackupIncomplete { database, .. } => database.to_string(),
        }
    }

    pub(crate) fn facts(&self) -> Vec<(&'static str, String)> {
        let mut facts = match self {
            Self::RootDriver { .. }
            | Self::RootInteractiveSessionAlreadyOpen
            | Self::RootInvalidValue
            | Self::RootInternalInvariant
            | Self::RootBackupIncomplete => Vec::new(),
            _ => vec![("database", self.subject())],
        };
        match self {
            Self::Io { failure, .. } => {
                facts.push(("io_kind", failure.kind.as_str().to_owned()));
                if let Some(code) = failure.raw_os_code {
                    facts.push(("raw_os_code", code.to_string()));
                }
            }
            Self::Driver {
                query_id,
                query_ordinal,
                failure,
                ..
            } => {
                if let Some(id) = query_id {
                    facts.push(("query_id", id.to_string()));
                }
                if let Some(ordinal) = query_ordinal {
                    facts.push(("query_ordinal", ordinal.to_string()));
                }
                push_sqlite_driver_facts(&mut facts, failure);
            }
            Self::RootDriver {
                query_id,
                query_ordinal,
                failure,
            } => {
                if let Some(id) = query_id {
                    facts.push(("query_id", id.to_string()));
                }
                if let Some(ordinal) = query_ordinal {
                    facts.push(("query_ordinal", ordinal.to_string()));
                }
                push_sqlite_driver_facts(&mut facts, failure);
            }
            _ => {}
        }
        facts
    }
}

fn push_sqlite_driver_facts(
    facts: &mut Vec<(&'static str, String)>,
    failure: &SqliteDriverFailure,
) {
    facts.push(("driver_kind", failure.kind.as_str().to_owned()));
    if let Some(code) = failure.primary_code {
        facts.push(("primary_code", code.to_string()));
    }
    if let Some(code) = failure.extended_code {
        facts.push(("extended_code", code.to_string()));
    }
    if let Some(index) = failure.column_index {
        facts.push(("column_index", index.to_string()));
    }
    if let Some(name) = &failure.column_name {
        facts.push(("column_name", name.to_string()));
    }
    if let Some(offset) = failure.sql_offset {
        facts.push(("sql_offset", offset.to_string()));
    }
    if let Some(index) = failure.database_index {
        facts.push(("database_index", index.to_string()));
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum HttpScheme {
    Http,
    Https,
}

impl HttpScheme {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HttpEndpoint {
    scheme: HttpScheme,
    host: SafeIdentifier,
    port: Option<u16>,
}

impl HttpEndpoint {
    pub(crate) fn new(scheme: HttpScheme, host: impl AsRef<str>, port: Option<u16>) -> Self {
        Self {
            scheme,
            host: SafeIdentifier::from_validated(host),
            port,
        }
    }
}

impl std::fmt::Display for HttpEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}://{}", self.scheme.as_str(), self.host)?;
        if let Some(port) = self.port {
            write!(formatter, ":{port}")?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HttpTransportPhase {
    Connect,
    Send,
    ReadErrorResponse,
    ReadSuccessResponse,
}

impl HttpTransportPhase {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Send => "send",
            Self::ReadErrorResponse => "read_error_response",
            Self::ReadSuccessResponse => "read_success_response",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HttpTransportKind {
    Dns,
    Connect,
    Send,
    Read,
    Tls,
    Timeout,
    Decode,
    Redirect,
}

/// 读取非成功 HTTP 响应正文时保留的完整传输事实。
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HttpResponseReadFailure {
    pub(crate) phase: HttpTransportPhase,
    pub(crate) transport: HttpTransportKind,
    pub(crate) io_kind: Option<super::SafeIoKind>,
    pub(crate) raw_os_code: Option<i32>,
}

impl HttpTransportKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Dns => "dns",
            Self::Connect => "connect",
            Self::Send => "send",
            Self::Read => "read",
            Self::Tls => "tls",
            Self::Timeout => "timeout",
            Self::Decode => "decode",
            Self::Redirect => "redirect",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HttpJsonCategory {
    Io,
    Syntax,
    Data,
    Eof,
}

impl HttpJsonCategory {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Io => "io",
            Self::Syntax => "syntax",
            Self::Data => "data",
            Self::Eof => "eof",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HttpEnvelopeViolation {
    MissingChoices,
    EmptyChoices,
    MissingMessage,
    MissingContent,
    InvalidContract,
}

impl HttpEnvelopeViolation {
    const fn as_str(self) -> &'static str {
        match self {
            Self::MissingChoices => "missing_choices",
            Self::EmptyChoices => "empty_choices",
            Self::MissingMessage => "missing_message",
            Self::MissingContent => "missing_content",
            Self::InvalidContract => "invalid_contract",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum HttpIssue {
    InvalidProxy,
    InvalidCertificate,
    ClientBuild,
    WaitCancelled {
        endpoint: HttpEndpoint,
    },
    ExecutorClosed {
        endpoint: HttpEndpoint,
    },
    RequestSerialization {
        endpoint: HttpEndpoint,
        category: HttpJsonCategory,
        line: usize,
        column: usize,
    },
    Transport {
        endpoint: HttpEndpoint,
        phase: HttpTransportPhase,
        transport: HttpTransportKind,
        io_kind: Option<super::SafeIoKind>,
        raw_os_code: Option<i32>,
    },
    Status {
        endpoint: HttpEndpoint,
        status: u16,
        retry_after_seconds: Option<u64>,
        provider_code: Option<SafeIdentifier>,
        provider_type: Option<SafeIdentifier>,
        provider_message: Option<SafeText>,
        response_read_failure: Option<HttpResponseReadFailure>,
    },
    ResponseJson {
        endpoint: HttpEndpoint,
        category: HttpJsonCategory,
        line: usize,
        column: usize,
    },
    InvalidEnvelope {
        endpoint: HttpEndpoint,
        violation: HttpEnvelopeViolation,
    },
}

impl HttpIssue {
    pub(crate) const fn stage(&self) -> DiagnosticStage {
        match self {
            Self::InvalidProxy | Self::InvalidCertificate | Self::ClientBuild => {
                DiagnosticStage::CommandPreparation
            }
            _ => DiagnosticStage::ModelRequest,
        }
    }

    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::InvalidProxy => "http.invalid_proxy",
            Self::InvalidCertificate => "http.invalid_certificate",
            Self::ClientBuild => "http.client_build",
            Self::WaitCancelled { .. } => "http.wait_cancelled",
            Self::ExecutorClosed { .. } => "http.executor_closed",
            Self::RequestSerialization { .. } => "http.request_serialization",
            Self::Transport {
                transport: HttpTransportKind::Dns,
                ..
            } => "http.transport.dns",
            Self::Transport {
                transport: HttpTransportKind::Connect,
                ..
            } => "http.transport.connect",
            Self::Transport {
                transport: HttpTransportKind::Tls,
                ..
            } => "http.transport.tls",
            Self::Transport {
                transport: HttpTransportKind::Timeout,
                ..
            } => "http.transport.timeout",
            Self::Transport {
                transport: HttpTransportKind::Read,
                ..
            } => "http.transport.read",
            Self::Transport { .. } => "http.transport.send",
            Self::Status { .. } => "http.status",
            Self::ResponseJson { .. } => "http.response_json",
            Self::InvalidEnvelope { .. } => "http.invalid_envelope",
        }
    }

    pub(crate) const fn resolution(&self) -> DiagnosticResolution {
        match self {
            Self::InvalidProxy | Self::InvalidCertificate => DiagnosticResolution::FixConfiguration,
            Self::ClientBuild => DiagnosticResolution::Retry,
            Self::WaitCancelled { .. } | Self::ExecutorClosed { .. } => DiagnosticResolution::Retry,
            Self::RequestSerialization { .. } => DiagnosticResolution::ReportBug,
            Self::Status {
                status: 401 | 403, ..
            } => DiagnosticResolution::FixConfiguration,
            Self::Transport { .. }
            | Self::Status { .. }
            | Self::ResponseJson { .. }
            | Self::InvalidEnvelope { .. } => DiagnosticResolution::CheckModelService,
        }
    }

    pub(crate) const fn summary_code(&self) -> &'static str {
        match self {
            Self::InvalidProxy => "invalid_value",
            Self::InvalidCertificate => "invalid_encoding",
            Self::ClientBuild => "transport_failed",
            Self::WaitCancelled { .. } => "lock_cancelled",
            Self::ExecutorClosed { .. } => "executor_closed",
            Self::RequestSerialization { .. } => "request_serialization_failed",
            Self::Transport { .. } => "transport_failed",
            Self::Status { .. } => "external_service_rejected",
            Self::ResponseJson { .. } => "response_parsing_failed",
            Self::InvalidEnvelope { .. } => "invalid_response_contract",
        }
    }

    pub(crate) fn subject(&self) -> String {
        match self {
            Self::InvalidProxy => "llm.proxy".to_owned(),
            Self::InvalidCertificate => "llm.additional_pem_files".to_owned(),
            Self::ClientBuild => "llm_http_client".to_owned(),
            Self::WaitCancelled { endpoint }
            | Self::ExecutorClosed { endpoint }
            | Self::RequestSerialization { endpoint, .. }
            | Self::Transport { endpoint, .. }
            | Self::Status { endpoint, .. }
            | Self::ResponseJson { endpoint, .. }
            | Self::InvalidEnvelope { endpoint, .. } => endpoint.to_string(),
        }
    }

    pub(crate) fn facts(&self) -> Vec<(&'static str, String)> {
        let mut facts = Vec::new();
        match self {
            Self::Transport {
                endpoint,
                phase,
                transport,
                io_kind,
                raw_os_code,
            } => {
                facts.push(("endpoint", endpoint.to_string()));
                facts.push(("phase", phase.as_str().to_owned()));
                facts.push(("transport", transport.as_str().to_owned()));
                if let Some(kind) = io_kind {
                    facts.push(("io_kind", kind.as_str().to_owned()));
                }
                if let Some(code) = raw_os_code {
                    facts.push(("raw_os_code", code.to_string()));
                }
            }
            Self::Status {
                endpoint,
                status,
                retry_after_seconds,
                provider_code,
                provider_type,
                provider_message,
                response_read_failure,
            } => {
                facts.push(("endpoint", endpoint.to_string()));
                facts.push(("status", status.to_string()));
                if let Some(seconds) = retry_after_seconds {
                    facts.push(("retry_after_seconds", seconds.to_string()));
                }
                if let Some(code) = provider_code {
                    facts.push(("provider_code", code.to_string()));
                }
                if let Some(kind) = provider_type {
                    facts.push(("provider_type", kind.to_string()));
                }
                if let Some(message) = provider_message {
                    facts.push(("provider_message", message.to_string()));
                }
                if let Some(failure) = response_read_failure {
                    facts.push(("response_read_phase", failure.phase.as_str().to_owned()));
                    facts.push((
                        "response_read_transport",
                        failure.transport.as_str().to_owned(),
                    ));
                    if let Some(kind) = failure.io_kind {
                        facts.push(("response_read_io_kind", kind.as_str().to_owned()));
                    }
                    if let Some(code) = failure.raw_os_code {
                        facts.push(("response_read_raw_os_code", code.to_string()));
                    }
                }
            }
            Self::RequestSerialization {
                endpoint,
                category,
                line,
                column,
            }
            | Self::ResponseJson {
                endpoint,
                category,
                line,
                column,
            } => {
                facts.push(("endpoint", endpoint.to_string()));
                facts.push(("json_category", category.as_str().to_owned()));
                facts.push(("line", line.to_string()));
                facts.push(("column", column.to_string()));
            }
            Self::InvalidEnvelope {
                endpoint,
                violation,
            } => {
                facts.push(("endpoint", endpoint.to_string()));
                facts.push(("violation", violation.as_str().to_owned()));
            }
            Self::WaitCancelled { endpoint } | Self::ExecutorClosed { endpoint } => {
                facts.push(("endpoint", endpoint.to_string()))
            }
            Self::InvalidProxy | Self::InvalidCertificate | Self::ClientBuild => {}
        }
        facts
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;

    #[test]
    fn windows_code_page_contract_is_literal_and_contains_no_free_text() {
        let diagnostic =
            crate::diagnostic::Diagnostic::runtime(RuntimeIssue::UnsupportedWindowsCodePage {
                expected: 65_001,
                actual: 936,
            });

        assert_eq!(
            serde_json::to_value(diagnostic).expect("进程诊断必须可序列化"),
            serde_json::json!({
                "code": "runtime.windows_code_page",
                "stage": "process_startup",
                "issue": {
                    "family": "runtime",
                    "details": {
                        "kind": "unsupported_windows_code_page",
                        "expected": 65001,
                        "actual": 936
                    }
                },
                "resolution": "report_bug"
            })
        );
    }

    #[test]
    fn error_response_read_failure_keeps_phase_and_os_facts() {
        let diagnostic = crate::diagnostic::Diagnostic::http(HttpIssue::Status {
            endpoint: HttpEndpoint::new(HttpScheme::Https, "api.example.test", Some(8443)),
            status: 503,
            retry_after_seconds: None,
            provider_code: None,
            provider_type: None,
            provider_message: None,
            response_read_failure: Some(HttpResponseReadFailure {
                phase: HttpTransportPhase::ReadErrorResponse,
                transport: HttpTransportKind::Read,
                io_kind: Some(crate::diagnostic::SafeIoKind::ConnectionReset),
                raw_os_code: Some(10054),
            }),
        });

        assert_eq!(
            serde_json::to_value(diagnostic).expect("HTTP 诊断必须可序列化"),
            serde_json::json!({
                "code": "http.status",
                "stage": "model_request",
                "issue": {
                    "family": "http",
                    "details": {
                        "kind": "status",
                        "endpoint": {
                            "scheme": "https",
                            "host": "api.example.test",
                            "port": 8443
                        },
                        "status": 503,
                        "retry_after_seconds": null,
                        "provider_code": null,
                        "provider_type": null,
                        "provider_message": null,
                        "response_read_failure": {
                            "phase": "read_error_response",
                            "transport": "read",
                            "io_kind": "connection_reset",
                            "raw_os_code": 10054
                        }
                    }
                },
                "resolution": "check_model_service"
            })
        );
    }

    #[test]
    fn invalid_translation_counters_keep_every_observed_count() {
        let diagnostic =
            crate::diagnostic::Diagnostic::runtime(RuntimeIssue::TranslationTaskCountersInvalid {
                planned: 4,
                started: 3,
                complete: 1,
                partial: 1,
                unavailable: 0,
                failed: 0,
                cancelled: 0,
                not_started: 1,
                violation: TranslationTaskCounterInvariant::StartedBreakdown,
            });

        let value = serde_json::to_value(diagnostic).expect("运行时诊断必须可序列化");
        assert_eq!(value["code"], "runtime.translation.task_counters_invalid");
        assert_eq!(value["issue"]["details"]["planned"], 4);
        assert_eq!(value["issue"]["details"]["started"], 3);
        assert_eq!(value["issue"]["details"]["violation"], "started_breakdown");
    }
}
