//! Generic 命令边界的实际错误、取消与状态交接验证。

use super::diagnostics::{
    GenericCommandError, GenericDiscardFailure, generic_accepted_task_diagnostics,
    generic_command_error_report, generic_cpu_execution_failure, generic_manual_failure,
    generic_placeholder_protection_report, generic_planning_report, generic_preparation_failure,
    generic_project_lease_failure, generic_prompt_resource_failure, generic_read_file_failure,
    generic_response_parse_diagnostic, generic_response_problem_diagnostic,
    generic_scratch_command_error, generic_task_response_diagnostic,
    generic_translation_resource_failure,
};
use super::lifecycle::{
    Driven, GenericCommandPanicContext, GenericCommandRunReport, GenericCommandRunResult,
    GenericOperationCancelled, catch_generic_command_panic,
    drive_generic_translate_with_panic_boundary, ensure_generic_operation_running,
    generic_translate_panic_error, write_back_signal_result,
};
use super::project_log::{
    complete_generic_translate_phase, finish_generic_translate_project_log,
    generic_project_log_slot, generic_terminal_occurrence_slot, generic_translate_driven_error,
    generic_translate_project_log_state, install_generic_project_log,
    install_generic_translate_task_log, mark_generic_translate_run_plan_saved,
    resolve_generic_translate_run_plan, set_generic_translate_summary,
    start_generic_translate_phase, take_generic_project_log, update_generic_translate_summary,
};
use super::tasks::{
    GenericCommittedTaskFinalResult, GenericPreparedTaskOutcome, GenericTaskRecordInFlight,
    GenericTaskTerminal, cancelled_generic_prepared_task, should_remember_profile_separately,
};
use super::write_back::{
    GenericWriteBackPublicationGate, begin_generic_write_back_publication,
    directory_prepare_cancelled_without_cleanup, generic_directory_discard_failure,
    generic_prepare_error, generic_prepare_failure, generic_publication_request_failure,
    publish_generic_write_back,
};
use super::{GENERIC_ENGINE_NAME, GenericCommandOutput, GenericTranslationSummary};
use crate::application::project_log::{CommandLogStart, start_command_log};
use crate::application::termination::TerminationSignals;
use crate::application::translation_prompt::PromptResourceLoadError;
use crate::diagnostic::{
    Diagnostic, DiagnosticReport, DiagnosticStage, FileSystemDiagnosticStage, FileSystemOperation,
    GenericDiagnosticStage, GenericIssue, GenericProblem, GenericResponseReviewFinding,
    GenericTaskResponseProblem, RelatedFailureRelation, RuntimeComponent, RuntimeIssue,
    RuntimeOperation, StateEffect,
};
use crate::execution::CooperativeCancellation;
use crate::execution::cpu::CpuTaskExecutionError;
use crate::generic::user_message::render_generic_user_message;
use crate::generic::write_back::materialization::{GenericScratchError, WRITE_BACK_SCRATCH_NAME};
use crate::generic::{
    CommitTranslationResultsOutcome, GenericCurrentTranslation, GenericInitRequest,
    GenericPlaceholderRuleDefinition, GenericPlaceholderRuleSource, GenericPlaceholderService,
    GenericPlanningError, GenericPlanningUnitLocator, GenericPreparationError, GenericProjectStore,
    GenericUnitKey, GenericUnitMap, GenericWriteBackError, GenericWriteBackTextOptions,
    ResponseProblem, TranslationReview, automatic_translation_state_fingerprint,
    build_write_back_candidate, build_write_back_candidate_with_cancellation,
    collect_generic_current_translations, compile_generic_layout_rules,
    prepare_generic_translation,
};
use crate::i18n::UiLocale;
use crate::language::{
    JapaneseLanguageModule, JapaneseResidualPolicy, LanguageAnalysis, LanguageId, LanguageModule,
    LanguageText,
};
use crate::llm::ApiKeyRedactor;
use crate::manual::ManualCommandError;
use crate::project_lease::{
    ProjectCommandLeaseError, ProjectCommandLeaseProvider, ProjectCommandLeaseService,
};
use crate::project_name::ProjectName;
use crate::runtime::cpu::CpuExecutorUnavailable;
use crate::runtime::filesystem::{SystemFileSystem, SystemFileSystemError};
use crate::runtime::performance::RunPerformanceCounters;
use crate::runtime::project_log::{
    ProjectLogAmount, ProjectLogCommand, ProjectLogEngine, ProjectLogPhase, RunPlanValueSource,
};
use crate::runtime::windows::WindowsFsError;
use crate::storage::file_system::{
    DirectoryDiscardError, DirectoryPrepareError, DirectoryPublishError, DirectoryPublishIntent,
    DirectorySourceMapping, DirectoryStageRequest, DirectoryStageRequestError, ReadFileError,
    RecoverableDirectoryPublisher, StagingCleanupFailure,
};
use crate::translation::candidate_validation::ReviewFinding;
use crate::translation::layout_rules::LayoutRuleSet;
use crate::translation::placeholder::PlaceholderProtectionError;
use crate::translation::placeholder_token;
use crate::translation::planning_resource::{
    CompiledTerminology, PlaceholderDefinitionError, TerminologyDefinitionError,
    TranslationPlanningResourceReadingError,
};
use crate::translation::task_planning::TaskId;
use crate::translation_protocol::TranslationResponseMode;
use rusqlite::Connection;
use std::error::Error;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Condvar, Mutex, mpsc};
use std::time::Duration;
use std::{fs, io};

#[test]
fn admitted_result_preservation_only_narrows_after_a_later_failure() {
    let accepted = GenericPreparedTaskOutcome::Accepted {
        writes: Vec::new(),
        rejections: Vec::new(),
        diagnostics: Vec::new(),
        finish_review: false,
        reviews: Vec::new(),
        accepted_units: 0,
        response_problems: 0,
        response_complete: true,
        accepted_output_ids: Vec::new(),
    };
    let preserving_external_failure = GenericPreparedTaskOutcome::Failed {
        error: generic_manual_failure(manual_read_failure()),
        preserve_admitted_results: true,
    };
    let blocking_internal_failure = GenericPreparedTaskOutcome::Failed {
        error: generic_manual_failure(manual_read_failure()),
        preserve_admitted_results: false,
    };

    assert!(!accepted.blocks_later_commits_after_prior_failure());
    assert!(!preserving_external_failure.blocks_later_commits_after_prior_failure());
    assert!(blocking_internal_failure.blocks_later_commits_after_prior_failure());
    assert!(GenericPreparedTaskOutcome::Cancelled.blocks_later_commits_after_prior_failure());
}

fn manual_read_failure() -> ManualCommandError {
    ManualCommandError::Document(crate::manual::ManualDocumentError::Read {
        path: PathBuf::from("C:/project/manual.toml"),
        source: io::Error::new(io::ErrorKind::PermissionDenied, "测试读取失败"),
    })
}

#[test]
fn only_direct_generic_manual_failure_uses_detailed_manual_renderer() {
    let direct = generic_manual_failure(manual_read_failure());
    assert!(direct.manual_error().is_some());

    let signal = GenericCommandError::Signal {
        source: io::Error::other("测试信号失败"),
        operation: Some(Box::new(generic_manual_failure(manual_read_failure()))),
        state_applied: false,
    };
    assert!(
        signal.manual_error().is_none(),
        "Signal 外层的类型化主错误和 related 不能被递归 Manual 呈现替换"
    );
    assert_eq!(generic_command_error_report(&signal).related().len(), 1);

    let discard_report = DiagnosticReport::new(
        StateEffect::RecoveryRequired,
        Diagnostic::runtime(RuntimeIssue::WorkerPanicked {
            component: RuntimeComponent::Process,
            operation: RuntimeOperation::Shutdown,
        }),
    );
    let discard = GenericDiscardFailure::new(discard_report, io::Error::other("测试候选清理失败"));
    let publish_discard = GenericCommandError::PublishDiscard {
        operation: Box::new(generic_manual_failure(manual_read_failure())),
        discard,
    };
    assert!(
        publish_discard.manual_error().is_none(),
        "Discard 外层必须保留类型化相关报告"
    );
    assert_eq!(
        generic_command_error_report(&publish_discard)
            .related()
            .len(),
        1
    );
}

struct TestManualTranslation<'a> {
    id: &'a str,
    relative_path: &'a str,
    group_id: &'a str,
    unit_id: &'a str,
    kind: &'a str,
    source: &'a str,
    translation: &'a str,
}

fn apply_test_manual_translation(store: &GenericProjectStore, entry: TestManualTranslation<'_>) {
    let source = entry
        .source
        .split('\n')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let translation = entry
        .translation
        .split('\n')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let project = store.open().expect("应该可读取测试项目语言对");
    let write = crate::manual::ValidatedManualTranslation {
        id: entry.id.to_owned(),
        kind: crate::manual::ManualTranslationType::Free,
        source: source.clone(),
        translation,
        locator: crate::manual::ManualTranslationLocator::Generic {
            group_id: entry.group_id.to_owned(),
            unit_id: entry.unit_id.to_owned(),
        },
        applicability: crate::manual::generic_manual_applicability(
            entry.group_id,
            entry.unit_id,
            entry.relative_path,
            entry.kind,
            project.language_pair().source().as_str(),
            project.language_pair().target().as_str(),
            &source,
        ),
    };
    let connection = Connection::open(store.database_path()).expect("应该可打开测试项目数据库");
    crate::manual::apply_generic_manual_translations(&connection, &[write])
        .expect("应该可保存独立人工译文");
}

fn task_id(value: usize) -> TaskId {
    TaskId::new(value)
}

fn compact_json(message: &str) -> String {
    let json = message
        .strip_prefix("```json\n")
        .and_then(|value| value.strip_suffix("\n```"))
        .expect("模型 user message 必须是单一 JSON 围栏");
    serde_json::to_string(
        &serde_json::from_str::<serde_json::Value>(json)
            .expect("模型 user message 必须是有效 JSON"),
    )
    .expect("模型 user message 应该可以重新序列化")
}

#[tokio::test]
async fn generic_panic_boundary_keeps_command_stage_and_workspace() {
    let workspace = PathBuf::from("projects/generic/panic-project");
    let context = GenericCommandPanicContext::new(
        crate::diagnostic::RuntimeCommand::Translate,
        workspace.clone(),
    );
    let redactor = Arc::new(ApiKeyRedactor::new(secrecy::SecretString::from(
        "selected-secret",
    )));
    context.observe_selected_api_key_redactor(Arc::clone(&redactor));
    let report = catch_generic_command_panic(context, async {
        panic!("不得读取的测试 panic payload")
    })
    .await;

    assert!(
        report
            .selected_api_key_redactor
            .as_ref()
            .is_some_and(|selected| Arc::ptr_eq(selected, &redactor))
    );

    let GenericCommandRunResult::Failed(GenericCommandError::Operation { failure }) = report.result
    else {
        panic!("Generic 命令 panic 必须成为命令级结构化失败");
    };
    let diagnostic = failure.report();
    assert_eq!(diagnostic.effect(), StateEffect::OutcomeUnknown);
    assert_eq!(diagnostic.primary().stage(), DiagnosticStage::Translate);
    assert_eq!(diagnostic.primary().code(), "runtime.command_panicked");
    let wire = serde_json::to_string(diagnostic).expect("panic 诊断必须可序列化");
    assert!(wire.contains("\"command\":\"translate\""));
    assert!(wire.contains(&workspace.to_string_lossy().to_string()));
    assert!(!wire.contains("不得读取的测试 panic payload"));
}

#[tokio::test]
async fn generic_command_panic_after_log_start_preserves_established_log_path() {
    let temporary = tempfile::tempdir().expect("应建立 Generic panic 日志测试目录");
    let common = crate::application::config::CommonCommandConfiguration::for_test(temporary.path());
    let project = "panic-log";
    let workspace = temporary.path().join(GENERIC_ENGINE_NAME).join(project);
    fs::create_dir_all(&workspace).expect("应建立 Generic 测试项目目录");
    let active = start_command_log(CommandLogStart {
        common: &common,
        locale: UiLocale::SimplifiedChinese,
        engine: ProjectLogEngine::Generic,
        project,
        command: ProjectLogCommand::Extract,
        performance: Arc::new(RunPerformanceCounters::default()),
        selected_api_key_redactor: None,
    });
    let expected = active
        .established_log_path()
        .expect("测试项目日志 runtime 应建立")
        .to_path_buf();
    let context =
        GenericCommandPanicContext::new(crate::diagnostic::RuntimeCommand::Extract, workspace);
    context.observe_project_log(&active);

    let report = catch_generic_command_panic(context, async {
        panic!("测试项目日志建立后的 Generic 命令 panic")
    })
    .await;

    assert_eq!(report.panic_log_path.as_deref(), Some(expected.as_path()));
    let GenericCommandRunResult::Failed(error) = report.result else {
        panic!("Generic 命令 panic 必须报告失败");
    };
    let diagnostic = generic_command_error_report(&error);
    let crate::diagnostic::DiagnosticIssue::Runtime(RuntimeIssue::CommandPanicked {
        log_path: Some(log_path),
        ..
    }) = diagnostic.primary().issue()
    else {
        panic!("Generic 命令 panic 的类型化诊断必须保留已建立日志路径");
    };
    assert_eq!(log_path.as_str(), expected.to_string_lossy().as_ref());
    let _ = active.pending_cancelled().finish();
}

#[tokio::test]
async fn generic_translate_panic_boundary_keeps_engine_finalization_path() {
    let workspace = PathBuf::from("projects/generic/translate-panic");
    let mut termination_signals = TerminationSignals::new();
    let driven = drive_generic_translate_with_panic_boundary(
        async {
            panic!("不得读取的 Translate panic payload");
            #[allow(unreachable_code)]
            Ok(GenericCommandOutput::Lua {
                project: "unreachable".parse().expect("项目名应合法"),
            })
        },
        &mut termination_signals,
        || {},
        GenericCommandPanicContext::new(
            crate::diagnostic::RuntimeCommand::Translate,
            workspace.clone(),
        ),
    )
    .await;

    let Driven::Finished(Err(error)) = driven else {
        panic!("Translate 内部 panic 必须回到引擎自己的失败收尾路径");
    };
    assert!(error.is_application_scope_panic());
    let diagnostic = generic_command_error_report(&error);
    assert_eq!(diagnostic.effect(), StateEffect::ProgressPreserved);
    assert_eq!(diagnostic.primary().code(), "runtime.command_panicked");
    let wire = serde_json::to_string(&diagnostic).expect("panic 诊断必须可序列化");
    assert!(wire.contains(&workspace.to_string_lossy().to_string()));
    assert!(!wire.contains("不得读取的 Translate panic payload"));
}

#[test]
fn generic_translate_panic_finalizes_started_tasks_and_project_log_once() {
    let temporary = tempfile::tempdir().expect("应建立 Translate panic 日志目录");
    let common = crate::application::config::CommonCommandConfiguration::for_test(temporary.path());
    let project = "translate-panic-log";
    let workspace = temporary.path().join("generic").join(project);
    fs::create_dir_all(&workspace).expect("应建立 Generic 项目工作区");
    let project_log = generic_project_log_slot();
    install_generic_project_log(
        &project_log,
        start_command_log(CommandLogStart {
            common: &common,
            locale: UiLocale::English,
            engine: ProjectLogEngine::Generic,
            project,
            command: ProjectLogCommand::Translate,
            performance: Arc::new(RunPerformanceCounters::default()),
            selected_api_key_redactor: None,
        }),
    );
    let state = generic_translate_project_log_state();
    start_generic_translate_phase(
        &project_log,
        &state,
        ProjectLogPhase::Planning,
        ProjectLogAmount::Indeterminate,
    );
    resolve_generic_translate_run_plan(
        &project_log,
        &state,
        &workspace.join("project.db"),
        RunPlanValueSource::Explicit,
        "local",
        None,
        None,
    );
    let tasks = install_generic_translate_task_log(&project_log, &state, 2);
    // 复用写入先于模型 Task；随后两个模型 Task 都在完成前 panic，所以模型 Unit 仍全部剩余。
    set_generic_translate_summary(
        &state,
        GenericTranslationSummary {
            total_tasks: 2,
            planned_units: 2,
            remaining_units: 2,
            reused_units: 1,
            written_units: 1,
            ..GenericTranslationSummary::default()
        },
    );
    mark_generic_translate_run_plan_saved(&state);
    complete_generic_translate_phase(
        &project_log,
        &state,
        ProjectLogPhase::Planning,
        ProjectLogAmount::Determinate {
            completed: 2,
            total: 2,
        },
    );
    start_generic_translate_phase(
        &project_log,
        &state,
        ProjectLogPhase::ConfirmedTasks,
        ProjectLogAmount::Determinate {
            completed: 0,
            total: 2,
        },
    );
    tasks.started(0);
    tasks.started(1);

    let panic_context = GenericCommandPanicContext::new(
        crate::diagnostic::RuntimeCommand::Translate,
        workspace.clone(),
    );
    let driven = Driven::Finished(Err(generic_translate_panic_error(&panic_context)));
    let error = generic_translate_driven_error(&driven).expect("panic 必须形成失败");
    tasks.fail_in_flight_after_panic(generic_command_error_report(error));
    let occurrence = finish_generic_translate_project_log(&project_log, &state, &driven)
        .expect("panic 失败必须形成项目日志诊断");
    let active = take_generic_project_log(&project_log).expect("项目日志必须仍由引擎持有");
    let _ = occurrence.into_pending(active).finish();

    let mut logs = fs::read_dir(workspace.join("logs"))
        .expect("日志目录必须可读取")
        .map(|entry| entry.expect("日志条目必须可读取").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect::<Vec<_>>();
    logs.sort();
    assert_eq!(logs.len(), 1, "panic 运行必须只有一份项目日志");
    let records = fs::read_to_string(&logs[0])
        .expect("panic 项目日志必须可读取")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("日志必须是 JSONL"))
        .collect::<Vec<_>>();
    assert_eq!(
        records
            .iter()
            .filter(|record| record["event"] == "task.started")
            .count(),
        2
    );
    let finished = records
        .iter()
        .filter(|record| record["event"] == "task.finished")
        .collect::<Vec<_>>();
    assert_eq!(finished.len(), 2, "每个已开始任务都必须获得 panic 终态");
    assert!(
        finished
            .iter()
            .all(|record| record["payload"]["outcome"]["kind"] == "failed")
    );
    for event in [
        "phase.stopped",
        "run_plan.finalized",
        "translation.finished",
        "run.finished",
    ] {
        assert_eq!(
            records
                .iter()
                .filter(|record| record["event"] == event)
                .count(),
            1,
            "panic 必须恰好写一条 {event}"
        );
    }
    let run_plan = records
        .iter()
        .find(|record| record["event"] == "run_plan.finalized")
        .expect("run plan 必须有终态");
    assert_eq!(run_plan["payload"]["result"]["kind"], "saved");
    let translation = records
        .iter()
        .find(|record| record["event"] == "translation.finished")
        .expect("Translate 必须有终态");
    assert_eq!(translation["payload"]["result"]["kind"], "failed");
    assert_eq!(
        translation["payload"]["result"]["tasks"],
        serde_json::json!({
            "planned": 2,
            "started": 2,
            "complete": 0,
            "partial": 0,
            "unavailable": 0,
            "failed": 2,
            "cancelled": 0,
            "not_started": 0,
        })
    );
    assert_eq!(
        translation["payload"]["result"]["summary"]["summary"]["written_units"],
        1
    );
    assert_eq!(
        records.last().expect("panic 日志不得为空")["payload"]["result"]["kind"],
        "failed",
        "run.finished 必须是最后的 Failed 终态"
    );
}

#[test]
fn generic_translate_log_state_keeps_saved_progress_summary() {
    let state = generic_translate_project_log_state();
    let initial = GenericTranslationSummary {
        total_tasks: 2,
        cleared_units: 1,
        ..GenericTranslationSummary::default()
    };
    set_generic_translate_summary(&state, initial);
    mark_generic_translate_run_plan_saved(&state);
    update_generic_translate_summary(&state, |summary| {
        summary.accepted_units += 1;
        summary.written_units += 1;
        summary.complete_tasks += 1;
    });

    let state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert!(state.run_plan_saved);
    assert_eq!(
        state.summary,
        Some(GenericTranslationSummary {
            total_tasks: 2,
            complete_tasks: 1,
            cleared_units: 1,
            accepted_units: 1,
            written_units: 1,
            ..GenericTranslationSummary::default()
        })
    );
}

#[test]
fn generic_partial_response_diagnostic_keeps_task_and_output_id() {
    let diagnostic =
        generic_response_problem_diagnostic(2, 3, &ResponseProblem::MissingId { output_id: 7 });

    assert_eq!(diagnostic.effect(), StateEffect::ProgressPreserved);
    assert_eq!(
        diagnostic.primary().code(),
        "generic.translation.response.missing_id"
    );
    let wire = serde_json::to_value(&diagnostic).expect("响应诊断必须可序列化");
    assert_eq!(
        wire["primary"]["issue"]["details"]["problem"]["task_ordinal"],
        3
    );
    assert_eq!(
        wire["primary"]["issue"]["details"]["problem"]["total_tasks"],
        3
    );
    assert_eq!(
        wire["primary"]["issue"]["details"]["problem"]["problem"]["output_id"],
        7
    );
}

#[test]
fn generic_review_diagnostics_follow_the_exact_commit_outcome() {
    let committed_review = TranslationReview::new(
        task_id(0),
        GenericPlanningUnitLocator::new("scene.jsonl", "group", "committed", "dialogue"),
        ReviewFinding::SourceResidual,
    );
    let conflicted_review = TranslationReview::new(
        task_id(1),
        GenericPlanningUnitLocator::new("scene.jsonl", "group", "conflicted", "dialogue"),
        ReviewFinding::SourceResidual,
    );
    let commit = CommitTranslationResultsOutcome {
        committed: 1,
        rejected: 0,
        resolved_rejected: 0,
        newly_rejected: 0,
        conflicts: vec![("group".to_owned(), "conflicted".to_owned())],
    };

    let diagnostics = generic_accepted_task_diagnostics(
        0,
        1,
        true,
        vec![generic_response_problem_diagnostic(
            0,
            1,
            &ResponseProblem::MissingId { output_id: 2 },
        )],
        vec![committed_review, conflicted_review],
        Some(&commit),
    );

    assert_eq!(
        diagnostics
            .iter()
            .map(DiagnosticReport::effect)
            .collect::<Vec<_>>(),
        [
            StateEffect::Applied,
            StateEffect::ProgressPreserved,
            StateEffect::Applied,
            StateEffect::ProgressPreserved,
        ]
    );
}

#[test]
fn generic_review_diagnostics_without_a_committed_translation_preserve_progress() {
    let review = TranslationReview::new(
        task_id(0),
        GenericPlanningUnitLocator::new("scene.jsonl", "group", "unit", "dialogue"),
        ReviewFinding::SourceResidual,
    );
    let conflict = CommitTranslationResultsOutcome {
        committed: 0,
        rejected: 0,
        resolved_rejected: 0,
        newly_rejected: 0,
        conflicts: vec![("group".to_owned(), "unit".to_owned())],
    };

    for commit in [None, Some(&conflict)] {
        let diagnostics =
            generic_accepted_task_diagnostics(0, 1, true, Vec::new(), vec![review.clone()], commit);
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| diagnostic.effect() == StateEffect::ProgressPreserved)
        );
    }
}

#[test]
fn raw_response_errors_keep_the_generic_shape_and_syntax_categories() {
    for (raw, expected_code, expected_summary, expected_category) in [
        (
            r#"{"think":"判断"}"#,
            "generic.translation.response.invalid_shape",
            "response_shape_invalid",
            "shape",
        ),
        (
            "```json\n{\"0\":[\"first\"]}\n```\n```json\n{\"1\":[\"second\"]}\n```",
            "generic.translation.response.invalid_json",
            "response_json_invalid",
            "syntax",
        ),
    ] {
        let error = crate::translation_protocol::parse_translation_response(
            raw,
            TranslationResponseMode::new(true, false),
        )
        .expect_err("测试原始响应必须由共享解析器拒绝");
        let report = generic_response_parse_diagnostic(0, 1, error);

        assert_eq!(report.primary().code(), expected_code);
        assert_eq!(report.primary().issue().summary_code(), expected_summary);
        let wire = serde_json::to_string(&report).expect("Generic 诊断必须可序列化");
        assert!(
            wire.contains(&format!(r#""category":"{expected_category}""#)),
            "诊断必须保留共享解析类别：{wire}"
        );
    }
}

#[test]
fn generic_planning_fact_diagnostic_keeps_full_unit_locator() {
    let locator = GenericPlanningUnitLocator::new(
        "a.jsonl",
        "group-a".to_owned(),
        "unit-a".to_owned(),
        "dialogue".to_owned(),
    );
    let report = generic_planning_report(&GenericPlanningError::Missing(locator));
    let wire = serde_json::to_value(report).expect("规划诊断必须可序列化");

    assert_eq!(
        wire["primary"]["issue"]["details"]["problem"]["unit"],
        serde_json::json!({
            "relative_path": "a.jsonl",
            "group_id": "group-a",
            "unit_id": "unit-a",
            "role": "dialogue",
            "line": null,
            "unit": null,
        })
    );
}

#[test]
fn cancellation_after_request_keeps_task_record_draft() {
    let prepared = cancelled_generic_prepared_task(
        1,
        Some(GenericTaskRecordInFlight {
            task_index: 1,
            requested_outputs: 1,
            user_message: "request".to_owned(),
        }),
        None,
        1,
        Some("SiliconFlow".to_owned()),
    );

    assert_eq!(prepared.task_index, 1);
    assert!(matches!(
        prepared.outcome,
        GenericPreparedTaskOutcome::Cancelled
    ));
    let record = prepared.record.expect("请求开始后的取消必须保留可提交记录");
    assert_eq!(record.task_index, 1);
    assert_eq!(record.requested_outputs, 1);
    assert_eq!(record.user_message, "request");
    assert!(record.raw_assistant.is_none());
    assert_eq!(record.provider.as_deref(), Some("SiliconFlow"));
}

#[test]
fn cancellation_during_response_processing_keeps_received_assistant() {
    let prepared = cancelled_generic_prepared_task(
        1,
        Some(GenericTaskRecordInFlight {
            task_index: 1,
            requested_outputs: 1,
            user_message: "request".to_owned(),
        }),
        Some(Arc::new(r#"{"0":["译文"]}"#.to_owned())),
        1,
        None,
    );

    let record = prepared.record.expect("收到响应后的取消必须保留任务记录");
    assert_eq!(
        record.raw_assistant.as_deref().map(String::as_str),
        Some(r#"{"0":["译文"]}"#)
    );
}

struct BlockingLanguageModule {
    inner: JapaneseLanguageModule,
    started: Mutex<Option<mpsc::SyncSender<()>>>,
    release: Arc<(Mutex<bool>, Condvar)>,
    analysis_count: Arc<AtomicUsize>,
}

impl LanguageModule for BlockingLanguageModule {
    fn analyze_source(&self, text: &LanguageText) -> LanguageAnalysis {
        self.analysis_count.fetch_add(1, Ordering::AcqRel);
        if let Some(started) = self.started.lock().expect("开始信号锁不应中毒").take() {
            started.send(()).expect("测试线程必须等待语言分析开始");
        }
        let (released, release_signal) = self.release.as_ref();
        let released = released.lock().expect("释放信号锁不应中毒");
        drop(
            release_signal
                .wait_while(released, |released| !*released)
                .expect("释放信号锁不应中毒"),
        );
        self.inner.analyze_source(text)
    }

    fn find_source_residual(
        &self,
        analysis: &LanguageAnalysis,
        translation: &LanguageText,
    ) -> Result<Option<crate::language::LanguageResidual>, crate::language::LanguageModuleError>
    {
        self.inner.find_source_residual(analysis, translation)
    }
}

#[test]
fn received_signal_after_lua_commit_preserves_successful_terminal_state() {
    let mut connection = Connection::open_in_memory().expect("应该建立测试数据库");
    connection
        .execute_batch("CREATE TABLE committed(value INTEGER NOT NULL);")
        .expect("应该建立测试表");
    let transaction = connection.transaction().expect("应该开始事务");
    transaction
        .execute("INSERT INTO committed(value) VALUES (1)", [])
        .expect("脚本事务应该写入");
    transaction.commit().expect("脚本事务应该提交");

    let report = GenericCommandRunReport::from_driven(
        Driven::Interrupted(Ok(GenericCommandOutput::Lua {
            project: "signal-race".parse().expect("项目名应该有效"),
        })),
        Vec::new(),
        None,
    );

    assert!(matches!(
        report.result,
        GenericCommandRunResult::Succeeded(GenericCommandOutput::Lua { .. })
    ));
    assert_eq!(
        connection
            .query_row("SELECT COUNT(*) FROM committed", [], |row| row
                .get::<_, i64>(0))
            .expect("应该读取已提交事务"),
        1,
        "信号晚到不能把已经提交的 Lua 事务伪报为取消"
    );
}

#[test]
fn signal_receiver_failure_after_applied_state_reports_applied_impact() {
    let report = GenericCommandRunReport::from_driven(
        Driven::SignalFailed {
            source: io::Error::other("signal receiver failed"),
            result: Ok(GenericCommandOutput::Lua {
                project: "signal-failure".parse().expect("项目名应该有效"),
            }),
        },
        Vec::new(),
        None,
    );

    let GenericCommandRunResult::Failed(error) = report.result else {
        panic!("信号接收失败仍应报告运行失败");
    };
    assert_eq!(
        generic_command_error_report(&error).effect(),
        StateEffect::AppliedFinalizationFailed
    );
}

#[test]
fn requested_cancellation_stops_operation_before_first_side_effect() {
    let cancellation = CooperativeCancellation::default();
    cancellation.request();
    let side_effect_observed = AtomicBool::new(false);

    let result = (|| {
        ensure_generic_operation_running(&cancellation).map_err(Box::new)?;
        side_effect_observed.store(true, Ordering::Release);
        Ok::<_, Box<GenericOperationCancelled>>(())
    })();

    let error = GenericCommandError::from(*result.expect_err("操作首次执行时必须观察已有取消请求"));
    assert!(!side_effect_observed.load(Ordering::Acquire));
    let report =
        GenericCommandRunReport::from_driven(Driven::Finished(Err(error)), Vec::new(), None);
    assert!(matches!(
        report.result,
        GenericCommandRunResult::Interrupted
    ));
}

#[test]
fn running_cpu_preparation_cancellation_becomes_interrupted() {
    let temporary = tempfile::tempdir().expect("应该可建立临时目录");
    let source_root = temporary.path().join("source");
    fs::create_dir(&source_root).expect("应该可建立输入目录");
    let group_count = rayon::current_num_threads().saturating_mul(4).max(8);
    let mut input = String::new();
    for index in 0..group_count {
        input.push_str(&format!(
            "{{\"id\":\"group-{index}\",\"kind\":\"dialogue\",\"units\":[\
             {{\"id\":\"unit-{index}\",\"text\":\"こんにちは\"}}]}}\n"
        ));
    }
    fs::write(source_root.join("scene.jsonl"), input).expect("应该可写入 Generic 输入");
    let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
        project_name: "cpu-cancel".parse().expect("项目名应该合法"),
        workspace_root: temporary.path().join("project"),
        source_root: Some(source_root),
        source_language: Some(LanguageId::parse("ja").expect("源语言应该合法")),
        target_language: Some(LanguageId::parse("zh-Hans").expect("目标语言应该合法")),
    })
    .expect("Generic 项目应该可初始化");
    store.extract().expect("Generic 输入应该可提取");
    let snapshot = store.load_snapshot().expect("应该可读取 Generic 快照");
    let rules = GenericPlaceholderService::default()
        .compile(Vec::new())
        .expect("空 Placeholder 规则应该合法");
    let terminology = Arc::new(CompiledTerminology::empty());
    let (started_sender, started_receiver) = mpsc::sync_channel(0);
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let analysis_count = Arc::new(AtomicUsize::new(0));
    let language_module: Arc<dyn LanguageModule> = Arc::new(BlockingLanguageModule {
        inner: JapaneseLanguageModule::new(
            JapaneseResidualPolicy::new(NonZeroUsize::MIN, Vec::new())
                .expect("日文残留策略应该合法"),
        ),
        started: Mutex::new(Some(started_sender)),
        release: Arc::clone(&release),
        analysis_count: Arc::clone(&analysis_count),
    });
    let cancellation = CooperativeCancellation::default();
    let operation_cancellation = cancellation.clone();
    let operation = std::thread::spawn(move || {
        prepare_generic_translation(
            &snapshot,
            terminology,
            &rules,
            &GenericPlaceholderRuleSource::ProjectSnapshot,
            language_module,
            NonZeroUsize::new(10_000).expect("常量应该非零"),
            false,
            &operation_cancellation,
        )
    });

    started_receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("CPU 工作必须先进入语言分析");
    cancellation.request();
    let (released, release_signal) = release.as_ref();
    *released.lock().expect("释放信号锁不应中毒") = true;
    release_signal.notify_all();
    let source = operation
        .join()
        .expect("CPU 测试线程不应 panic")
        .err()
        .expect("运行中的 CPU 工作必须观察取消");
    assert!(source.is_cancelled());

    let error = generic_preparation_failure(source);
    let report =
        GenericCommandRunReport::from_driven(Driven::Finished(Err(error)), Vec::new(), None);
    assert!(matches!(
        report.result,
        GenericCommandRunResult::Interrupted
    ));
    assert!(
        analysis_count.load(Ordering::Acquire) < group_count,
        "取消后不得继续遍历剩余 Group"
    );
}

#[test]
fn generic_placeholder_failure_keeps_rule_source_unit_locator_and_match_range() {
    let temporary = tempfile::tempdir().expect("应该可建立临时目录");
    let source_root = temporary.path().join("source");
    fs::create_dir(&source_root).expect("应该可建立输入目录");
    fs::write(
        source_root.join("a.jsonl"),
        concat!(
            r#"{"id":"group-a","kind":"dialogue","units":["#,
            r#"{"id":"safe","text":"こんにちは"},"#,
            r#"{"id":"unit-a","text":"甲触发缺组乙"}]}"#,
            "\n"
        ),
    )
    .expect("应该可写入首个 Generic 输入");
    fs::write(
        source_root.join("b.jsonl"),
        concat!(
            r#"{"id":"group-b","kind":"dialogue","units":["#,
            r#"{"id":"unit-b","text":"丙触发缺组丁"}]}"#,
            "\n"
        ),
    )
    .expect("应该可写入第二个 Generic 输入");
    let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
        project_name: "placeholder-locator".parse().expect("项目名应该合法"),
        workspace_root: temporary.path().join("project"),
        source_root: Some(source_root),
        source_language: Some(LanguageId::parse("ja").expect("源语言应该合法")),
        target_language: Some(LanguageId::parse("zh-Hans").expect("目标语言应该合法")),
    })
    .expect("Generic 项目应该可初始化");
    store.extract().expect("Generic 输入应该可提取");
    let snapshot = store.load_snapshot().expect("应该可读取 Generic 快照");
    let rules = GenericPlaceholderService::default()
        .compile(vec![GenericPlaceholderRuleDefinition::new(
            Some(vec!["dialogue".to_owned()]),
            r"(?:(?<text>保留)|触发缺组)",
        )])
        .expect("Placeholder 规则应该可编译");
    let external_rules = temporary.path().join("rules/placeholders.toml");
    let language_module: Arc<dyn LanguageModule> = Arc::new(JapaneseLanguageModule::new(
        JapaneseResidualPolicy::new(NonZeroUsize::MIN, Vec::new()).expect("日文残留策略应该合法"),
    ));

    for expected_rule_source in [
        GenericPlaceholderRuleSource::ExternalFile(external_rules),
        GenericPlaceholderRuleSource::ProjectSnapshot,
    ] {
        let error = match prepare_generic_translation(
            &snapshot,
            Arc::new(CompiledTerminology::empty()),
            &rules,
            &expected_rule_source,
            Arc::clone(&language_module),
            NonZeroUsize::new(10_000).expect("常量应该非零"),
            false,
            &CooperativeCancellation::default(),
        ) {
            Err(error) => error,
            Ok(_) => panic!("最早的缺少 text 捕获应该阻止规划"),
        };

        let report =
            generic_placeholder_protection_report(&error, GenericDiagnosticStage::Translate)
                .expect("Placeholder 叶子失败必须直接投影为结构化报告");
        let wire = serde_json::to_value(&report).expect("诊断报告必须可序列化");
        assert_eq!(
            wire["primary"]["code"],
            "translation.placeholder.missing_text_capture"
        );
        assert_eq!(wire["primary"]["stage"], "translate");
        assert_eq!(wire["primary"]["resolution"], "fix_placeholder_rules");
        assert_eq!(wire["effect"], "unchanged");
        assert_eq!(
            wire["primary"]["issue"]["details"]["unit"]["relative_path"],
            "a.jsonl"
        );
        assert_eq!(
            wire["primary"]["issue"]["details"]["unit"]["group_id"],
            "group-a"
        );
        assert_eq!(
            wire["primary"]["issue"]["details"]["unit"]["unit_id"],
            "unit-a"
        );
        assert_eq!(
            wire["primary"]["issue"]["details"]["unit"]["role"],
            "dialogue"
        );
        assert_eq!(
            wire["primary"]["issue"]["details"]["problem"]["rule_number"],
            1
        );
        assert_eq!(
            wire["primary"]["issue"]["details"]["problem"]["match_range"],
            serde_json::json!({
                "start": "甲".len(),
                "end": "甲触发缺组".len(),
            })
        );
        if expected_rule_source == GenericPlaceholderRuleSource::ProjectSnapshot {
            let report =
                generic_placeholder_protection_report(&error, GenericDiagnosticStage::WriteBack)
                    .expect("WriteBack Placeholder 失败必须保留 Unit 定位");
            let wire = serde_json::to_value(report).expect("诊断报告必须可序列化");
            assert_eq!(
                wire["primary"]["code"],
                "generic.write_back.placeholder.missing_text_capture"
            );
            assert_eq!(wire["primary"]["stage"], "write_back");
            assert_eq!(wire["primary"]["resolution"], "fix_placeholder_rules");
            assert_eq!(
                wire["primary"]["issue"]["details"]["operation"],
                "build_write_back_candidate"
            );
            assert_eq!(
                wire["primary"]["issue"]["details"]["problem"]["unit"]["role"],
                "dialogue"
            );
            assert_eq!(
                wire["primary"]["issue"]["details"]["problem"]["problem"]["side"],
                "source"
            );
        }

        match error {
            GenericPreparationError::PlaceholderProtection {
                rule_source,
                locator,
                source:
                    PlaceholderProtectionError::MissingTextCapture {
                        rule_number,
                        whole_match_start_byte,
                        whole_match_end_byte,
                    },
            } => {
                assert_eq!(rule_source, expected_rule_source);
                assert_eq!(locator.relative_path(), Path::new("a.jsonl"));
                assert_eq!(locator.group_id(), "group-a");
                assert_eq!(locator.unit_id(), "unit-a");
                assert_eq!(locator.role(), "dialogue");
                assert_eq!(rule_number, 1);
                assert_eq!(whole_match_start_byte, "甲".len());
                assert_eq!(whole_match_end_byte, "甲触发缺组".len());
            }
            other => panic!("应返回完整 Generic Placeholder 定位，实际为 {other:?}"),
        }
    }
}

#[test]
fn cancelled_cpu_schedule_becomes_interrupted() {
    let source: CpuTaskExecutionError<CpuExecutorUnavailable> = CpuTaskExecutionError::Cancelled;
    let error = generic_cpu_execution_failure(source);
    let report =
        GenericCommandRunReport::from_driven(Driven::Finished(Err(error)), Vec::new(), None);

    assert!(matches!(
        report.result,
        GenericCommandRunResult::Interrupted
    ));
}

#[test]
fn cancelled_lease_file_and_resource_boundaries_become_interrupted() {
    fn assert_interrupted(error: GenericCommandError) {
        let report =
            GenericCommandRunReport::from_driven(Driven::Finished(Err(error)), Vec::new(), None);
        assert!(matches!(
            report.result,
            GenericCommandRunResult::Interrupted
        ));
    }

    let cancelled_fs = || SystemFileSystemError::Cancelled {
        operation: "test_wait",
        path: PathBuf::from("cancelled"),
    };
    assert_interrupted(generic_project_lease_failure(
        ProjectCommandLeaseError::Unavailable {
            project: "lease-cancel".parse().expect("项目名应该合法"),
            source: Box::new(cancelled_fs()),
        },
    ));
    assert_interrupted(generic_read_file_failure(
        ReadFileError::Io {
            path: PathBuf::from("script.lua"),
            source: cancelled_fs(),
        },
        FileSystemDiagnosticStage::CommandPreparation,
    ));
    assert_interrupted(generic_prompt_resource_failure(
        PromptResourceLoadError::Read(ReadFileError::Io {
            path: PathBuf::from("system.md"),
            source: SystemFileSystemError::Windows(WindowsFsError::LockCancelled {
                path: PathBuf::from("system.md"),
            }),
        }),
    ));
    assert_interrupted(generic_translation_resource_failure(
        TranslationPlanningResourceReadingError::ReadTerminology {
            path: PathBuf::from("terms.json"),
            source: ReadFileError::Io {
                path: PathBuf::from("terms.json"),
                source: cancelled_fs(),
            },
        },
    ));
    assert_interrupted(generic_translation_resource_failure(
        TranslationPlanningResourceReadingError::ParsePlaceholderRulesCompute {
            path: None,
            source: CpuTaskExecutionError::Cancelled,
        },
    ));
}

#[test]
fn translation_resource_worker_start_is_an_internal_failure() {
    type ResourceError =
        TranslationPlanningResourceReadingError<SystemFileSystemError, CpuExecutorUnavailable>;

    let failures = [
        (
            ResourceError::InvalidTerminology {
                path: Some(PathBuf::from("terms.toml")),
                source: TerminologyDefinitionError::StartWorker {
                    operation: "att-term-matcher",
                    source: io::Error::from_raw_os_error(8),
                },
            },
            "translation.terminology.worker_start",
        ),
        (
            ResourceError::InvalidPlaceholderRules {
                path: Some(PathBuf::from("placeholders.toml")),
                source: PlaceholderDefinitionError::StartWorker {
                    operation: "att-placeholder-toml",
                    source: io::Error::from_raw_os_error(8),
                },
            },
            "translation.placeholder_definition.worker_start",
        ),
    ];

    for (source, expected_code) in failures {
        let GenericCommandError::Operation { failure } =
            generic_translation_resource_failure(source)
        else {
            panic!("worker 启动失败必须保留为普通失败");
        };
        let diagnostic = failure.report();
        assert_eq!(diagnostic.effect(), StateEffect::Unchanged);
        assert_eq!(diagnostic.primary().code(), expected_code);
        let wire = serde_json::to_string(diagnostic).expect("worker 诊断必须可序列化");
        assert!(wire.contains("\"raw_os_code\":8"));
    }
}

#[test]
fn lease_cleanup_failure_is_not_hidden_as_cancellation() {
    let error = generic_project_lease_failure(ProjectCommandLeaseError::Unavailable {
        project: "lease-cleanup".parse().expect("项目名应该合法"),
        source: Box::new(SystemFileSystemError::DirectChildRollbackFailed {
            path: PathBuf::from("lease"),
            operation: Box::new(SystemFileSystemError::Cancelled {
                operation: "lease",
                path: PathBuf::from("lease"),
            }),
            rollback: Box::new(SystemFileSystemError::Io {
                operation: "rollback",
                path: PathBuf::from("lease"),
                source: io::Error::from_raw_os_error(5),
            }),
        }),
    });

    assert!(matches!(error, GenericCommandError::Operation { .. }));
}

#[tokio::test]
async fn real_project_lease_wait_cancellation_becomes_interrupted() {
    let temporary = tempfile::tempdir().expect("应该可建立临时目录");
    let projects_root = temporary.path().join("projects");
    let owner_file_system = SystemFileSystem::new().expect("应该可建立租约所有者文件能力");
    let contender_file_system = SystemFileSystem::new().expect("应该可建立租约竞争者文件能力");
    let owner = ProjectCommandLeaseService::new(
        projects_root.clone(),
        GENERIC_ENGINE_NAME,
        owner_file_system.clone(),
    );
    let contender = ProjectCommandLeaseService::new(
        projects_root,
        GENERIC_ENGINE_NAME,
        contender_file_system.clone(),
    );
    let project: ProjectName = "lease-race".parse().expect("项目名应该合法");
    let held = owner
        .acquire(&project)
        .await
        .expect("所有者应该取得项目租约");
    let waiting_project = project.clone();
    let waiting = tokio::spawn(async move { contender.acquire(&waiting_project).await });
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(!waiting.is_finished(), "竞争者应该等待真实项目租约");

    contender_file_system.cancel_waits();
    let source = tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .expect("取消应该及时唤醒租约等待")
        .expect("租约竞争任务不应 panic")
        .err()
        .expect("取消后不得取得项目租约");
    let error = generic_project_lease_failure(source);
    let report =
        GenericCommandRunReport::from_driven(Driven::Interrupted(Err(error)), Vec::new(), None);
    assert!(matches!(
        report.result,
        GenericCommandRunResult::Interrupted
    ));

    drop(held);
    contender_file_system
        .shutdown()
        .await
        .expect("竞争者文件能力应该可终结");
    owner_file_system
        .shutdown()
        .await
        .expect("所有者文件能力应该可终结");
}

#[test]
fn write_back_publication_gate_allows_exactly_one_terminal_decision() {
    let cancelled_first = GenericWriteBackPublicationGate::default();
    assert!(cancelled_first.request_cancellation());
    assert!(!cancelled_first.begin_publication());

    let published_first = GenericWriteBackPublicationGate::default();
    assert!(published_first.begin_publication());
    assert!(!published_first.request_cancellation());

    for _ in 0..32 {
        let gate = GenericWriteBackPublicationGate::default();
        let barrier = Arc::new(Barrier::new(3));
        let cancellation_gate = gate.clone();
        let cancellation_barrier = Arc::clone(&barrier);
        let cancellation = std::thread::spawn(move || {
            cancellation_barrier.wait();
            cancellation_gate.request_cancellation()
        });
        let publication_gate = gate;
        let publication_barrier = Arc::clone(&barrier);
        let publication = std::thread::spawn(move || {
            publication_barrier.wait();
            publication_gate.begin_publication()
        });
        barrier.wait();

        let cancellation_won = cancellation.join().expect("取消竞争线程不应 panic");
        let publication_won = publication.join().expect("发布竞争线程不应 panic");
        assert_ne!(
            cancellation_won, publication_won,
            "取消与发布必须恰好一个取得终态决定"
        );
    }
}

#[test]
fn cancelled_publication_gate_does_not_report_or_enter_publishing() {
    let gate = GenericWriteBackPublicationGate::default();
    assert!(gate.request_cancellation());
    let observed = Arc::new(AtomicBool::new(false));
    let callback_observed = Arc::clone(&observed);

    let began_publication = begin_generic_write_back_publication(&gate, move || {
        callback_observed.store(true, Ordering::Release);
    });

    assert!(!began_publication);
    assert!(!observed.load(Ordering::Acquire));
}

#[test]
fn write_back_signal_after_publication_keeps_the_real_terminal_result() {
    let output = GenericCommandOutput::Lua {
        project: "published-before-signal".parse().expect("项目名应该有效"),
    };
    let report = GenericCommandRunReport::from_driven(
        write_back_signal_result(false, Ok(output)),
        Vec::new(),
        None,
    );
    assert!(matches!(
        report.result,
        GenericCommandRunResult::Succeeded(GenericCommandOutput::Lua { .. })
    ));

    let cancelled = GenericCommandRunReport::from_driven(
        write_back_signal_result(
            true,
            Ok(GenericCommandOutput::Lua {
                project: "cancelled-before-publish".parse().expect("项目名应该有效"),
            }),
        ),
        Vec::new(),
        None,
    );
    assert!(matches!(
        cancelled.result,
        GenericCommandRunResult::Interrupted
    ));
}

#[test]
fn cancelled_directory_prepare_maps_to_interrupted_without_cleanup_failure() {
    let cancellation = CooperativeCancellation::default();
    cancellation.request();
    let error = generic_prepare_error(
        &cancellation,
        DirectoryPrepareError::NotPrepared {
            target_root: PathBuf::from("write_back"),
            source: Box::new(SystemFileSystemError::Cancelled {
                operation: "prepare_directory_candidate",
                path: PathBuf::from("write_back"),
            }),
            cleanup_failure: None,
        },
    );
    assert!(error.is_cancelled());

    let report =
        GenericCommandRunReport::from_driven(Driven::Interrupted(Err(error)), Vec::new(), None);
    assert!(matches!(
        report.result,
        GenericCommandRunResult::Interrupted
    ));
}

#[tokio::test]
async fn real_publish_lock_cancellation_maps_to_interrupted_and_removes_scratch() {
    let temporary = tempfile::tempdir().expect("应该可建立临时目录");
    let source_root = temporary.path().join("source");
    fs::create_dir(&source_root).expect("应该可建立候选来源");
    fs::write(source_root.join("source.jsonl"), b"source").expect("应该可建立候选文件");
    let target_root = temporary.path().join("write_back");
    let lock_directory = temporary.path().join("publish-locks");
    let publisher_config =
        crate::runtime::filesystem::DirectoryPublisherConfig::production(lock_directory)
            .expect("发布锁配置应该合法");
    let owner_file_system = SystemFileSystem::new().expect("应该可建立锁所有者文件能力");
    let contender_file_system = SystemFileSystem::new().expect("应该可建立锁竞争者文件能力");
    let owner = owner_file_system.directory_publisher(publisher_config.clone());
    let contender = contender_file_system.directory_publisher(publisher_config);
    let stage_request = || {
        let mapping = DirectorySourceMapping::new(source_root.clone(), PathBuf::new())
            .expect("候选来源映射应该合法");
        DirectoryStageRequest::new(
            target_root.clone(),
            DirectoryPublishIntent::CreateNew,
            vec![mapping],
            Vec::new(),
            Vec::new(),
        )
        .expect("候选请求应该合法")
    };
    let staged = owner
        .prepare(stage_request())
        .await
        .expect("所有者应该持有目标发布锁");

    let waiting = tokio::spawn({
        let contender = contender.clone();
        let request = stage_request();
        async move { contender.prepare(request).await }
    });
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(!waiting.is_finished(), "竞争者应该持续等待真实目标锁");

    let workspace_root = temporary.path().join("project");
    fs::create_dir(&workspace_root).expect("应该可建立项目工作区");
    let scratch_root = workspace_root.join(WRITE_BACK_SCRATCH_NAME);
    fs::create_dir(&scratch_root).expect("应该可建立待清理 scratch");
    fs::write(scratch_root.join("residual"), b"candidate").expect("应该可建立 scratch 内容");
    let cancellation = CooperativeCancellation::default();
    cancellation.request();
    contender_file_system.cancel_waits();
    let prepare_result = tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .expect("取消应该及时唤醒发布锁等待")
        .expect("竞争任务不应 panic");
    let prepare_error = match prepare_result {
        Ok(_) => panic!("取消后不得伪造候选已准备"),
        Err(error) => error,
    };
    assert!(directory_prepare_cancelled_without_cleanup(&prepare_error));

    let error =
        generic_prepare_failure(&cancellation, prepare_error, &workspace_root, &scratch_root);
    assert!(error.is_cancelled());
    assert!(!scratch_root.exists(), "受控取消后必须删除 scratch");
    let report =
        GenericCommandRunReport::from_driven(Driven::Interrupted(Err(error)), Vec::new(), None);
    assert!(matches!(
        report.result,
        GenericCommandRunResult::Interrupted
    ));

    owner.discard(staged).await.expect("应该可丢弃所有者候选");
    contender_file_system
        .shutdown()
        .await
        .expect("竞争者文件能力应该可终结");
    owner_file_system
        .shutdown()
        .await
        .expect("所有者文件能力应该可终结");
}

#[test]
fn cancelled_directory_prepare_with_cleanup_failure_remains_a_failure() {
    let cancellation = CooperativeCancellation::default();
    cancellation.request();
    let error = generic_prepare_error(
        &cancellation,
        DirectoryPrepareError::NotPrepared {
            target_root: PathBuf::from("write_back"),
            source: Box::new(SystemFileSystemError::Cancelled {
                operation: "prepare_directory_candidate",
                path: PathBuf::from("write_back"),
            }),
            cleanup_failure: Some(StagingCleanupFailure::new(
                PathBuf::from("candidate"),
                Box::new(SystemFileSystemError::Cancelled {
                    operation: "cleanup_directory_candidate",
                    path: PathBuf::from("candidate"),
                }),
            )),
        },
    );
    assert!(!error.is_cancelled());
    assert!(matches!(error, GenericCommandError::Operation { .. }));
}

#[test]
fn directory_prepare_preserves_recovery_and_unknown_effects() {
    for (source, expected_effect) in [
        (
            SystemFileSystemError::JournalCorrupt {
                path: PathBuf::from(".directory-publish/write_back/journal"),
                violation: crate::diagnostic::FileSystemJournalViolation::NotRegularFile,
            },
            StateEffect::RecoveryRequired,
        ),
        (
            SystemFileSystemError::OutcomeUnknown {
                target_root: PathBuf::from("write_back"),
                artifacts: vec![PathBuf::from(".directory-publish/write_back/journal")],
                violation: crate::diagnostic::FileSystemRecoveryViolation::ObservationFailed,
            },
            StateEffect::OutcomeUnknown,
        ),
    ] {
        let error = generic_prepare_error(
            &CooperativeCancellation::default(),
            DirectoryPrepareError::NotPrepared {
                target_root: PathBuf::from("write_back"),
                source: Box::new(source),
                cleanup_failure: None,
            },
        );
        assert_eq!(
            generic_command_error_report(&error).effect(),
            expected_effect
        );
    }
}

#[test]
fn directory_publish_cleanup_is_a_related_recovery_failure() {
    let source = DirectoryPublishError::TargetAlreadyExists {
        target_root: PathBuf::from("write_back"),
        cleanup_failure: Some(StagingCleanupFailure::new(
            PathBuf::from(".directory-publish/write_back/stage"),
            Box::new(SystemFileSystemError::Closed),
        )),
    };
    let report = source.diagnostic_report();
    assert_eq!(report.effect(), StateEffect::RecoveryRequired);
    assert_eq!(report.related().len(), 1);
    assert_eq!(
        report.related()[0].relation(),
        RelatedFailureRelation::Cleanup
    );
    assert_eq!(
        report.related()[0].report().effect(),
        StateEffect::RecoveryRequired
    );

    let error = GenericCommandError::reported(source, report);
    assert_eq!(
        generic_command_error_report(&error).effect(),
        StateEffect::RecoveryRequired
    );
}

#[test]
fn directory_request_failure_keeps_output_root_and_invalid_path() {
    let output_root = PathBuf::from("project/write_back");
    let invalid_path = PathBuf::from("../outside");
    let error = generic_publication_request_failure(
        &output_root,
        DirectoryStageRequestError::InvalidRelativePath {
            path: invalid_path.clone(),
        },
    );
    let report = generic_command_error_report(&error);

    assert_eq!(report.effect(), StateEffect::Unchanged);
    assert_eq!(
        report.primary().code(),
        "publication.request.invalid_relative_path"
    );
    let wire = serde_json::to_string(&report).expect("发布请求诊断必须可序列化");
    assert!(wire.contains(&output_root.to_string_lossy().to_string()));
    assert!(wire.contains(&invalid_path.to_string_lossy().to_string()));
}

#[test]
fn cancelled_scratch_cleanup_failure_keeps_primary_and_recovery_path() {
    let scratch_root = PathBuf::from("project/.write_back.tmp");
    let error = generic_scratch_command_error(GenericScratchError::CleanupAfterFailure {
        operation: Box::new(GenericScratchError::Cancelled),
        cleanup: Box::new(GenericScratchError::Io {
            operation: FileSystemOperation::Remove,
            path: scratch_root.clone(),
            source: io::Error::from_raw_os_error(5),
        }),
    });

    let report = generic_command_error_report(&error);
    assert_eq!(report.effect(), StateEffect::RecoveryRequired);
    assert_eq!(report.primary().code(), "runtime.cancelled");
    assert_eq!(report.related().len(), 1);
    assert_eq!(
        report.related()[0].relation(),
        RelatedFailureRelation::Discard
    );
    assert_eq!(
        report.related()[0].report().effect(),
        StateEffect::RecoveryRequired
    );
    let wire = serde_json::to_string(&report).expect("scratch 诊断必须可序列化");
    assert!(wire.contains(&scratch_root.to_string_lossy().to_string()));

    match error {
        GenericCommandError::PublishDiscard { operation, .. } => {
            assert!(operation.is_cancelled());
        }
        other => panic!("取消后的清理失败必须保留双错误，实际为 {other}"),
    }
}

#[test]
fn failed_scratch_materialization_cleanup_keeps_primary_and_recovery_path() {
    let scratch_root = PathBuf::from("project/.write_back.tmp");
    let source_file = scratch_root.join("dialogue.jsonl");
    let error = generic_scratch_command_error(GenericScratchError::CleanupAfterFailure {
        operation: Box::new(GenericScratchError::Io {
            operation: FileSystemOperation::Write,
            path: source_file,
            source: io::Error::from_raw_os_error(5),
        }),
        cleanup: Box::new(GenericScratchError::Io {
            operation: FileSystemOperation::Remove,
            path: scratch_root.clone(),
            source: io::Error::from_raw_os_error(5),
        }),
    });

    let report = generic_command_error_report(&error);
    assert_eq!(report.effect(), StateEffect::RecoveryRequired);
    assert_eq!(report.primary().code(), "filesystem.io");
    assert_eq!(report.related().len(), 1);
    assert_eq!(
        report.related()[0].relation(),
        RelatedFailureRelation::Discard
    );
    let related = report.related()[0].report();
    assert_eq!(related.effect(), StateEffect::RecoveryRequired);
    assert_eq!(related.primary().code(), "filesystem.io");
    let wire = serde_json::to_string(&report).expect("scratch 诊断必须可序列化");
    assert!(wire.contains("\"operation\":\"write\""));
    assert!(wire.contains("\"operation\":\"remove\""));
    assert!(wire.contains("\"raw_os_code\":5"));
    assert!(wire.contains(&scratch_root.to_string_lossy().to_string()));

    let primary_source = Error::source(&error).expect("双重失败必须以主业务错误为 source");
    assert!(
        primary_source
            .downcast_ref::<GenericCommandError>()
            .is_some()
    );

    match &error {
        GenericCommandError::PublishDiscard { operation, discard } => {
            assert!(!operation.is_cancelled());
            assert!(matches!(
                operation.as_ref(),
                GenericCommandError::Operation { .. }
            ));
            let cleanup = discard
                .source_error()
                .downcast_ref::<GenericScratchError>()
                .expect("应保留真实 scratch 清理错误");
            let io_source = Error::source(cleanup)
                .and_then(|source| source.downcast_ref::<io::Error>())
                .expect("scratch 清理错误链应保留 io::Error");
            assert_eq!(io_source.raw_os_error(), Some(5));
        }
        other => panic!("暂存建立与清理双重失败必须保留主错误和恢复路径，实际为 {other}"),
    }

    assert_eq!(
        generic_command_error_report(&error).effect(),
        StateEffect::RecoveryRequired
    );
}

#[test]
fn directory_discard_preserves_typed_io_failure_as_related_diagnostic() {
    let stage_root = PathBuf::from("project/.directory-publish/write_back/stage");
    let source = DirectoryDiscardError::new(
        stage_root.clone(),
        Box::new(SystemFileSystemError::Io {
            operation: "discard_directory_candidate",
            path: stage_root.clone(),
            source: io::Error::from_raw_os_error(32),
        }),
    );
    let discard = generic_directory_discard_failure(source);
    let operation_report = DiagnosticReport::new(
        StateEffect::Unchanged,
        Diagnostic::generic(GenericIssue::project(
            GenericDiagnosticStage::WriteBack,
            GenericProblem::WriteBackSourceChanged,
        )),
    );
    let error = GenericCommandError::PublishDiscard {
        operation: Box::new(GenericCommandError::reported(
            io::Error::other("input changed"),
            operation_report,
        )),
        discard,
    };

    assert!(matches!(
        Error::source(&error),
        Some(source) if source.downcast_ref::<GenericCommandError>().is_some()
    ));
    let report = generic_command_error_report(&error);
    assert_eq!(report.effect(), StateEffect::RecoveryRequired);
    assert_eq!(report.related().len(), 1);
    assert_eq!(
        report.related()[0].relation(),
        RelatedFailureRelation::Discard
    );
    let related = report.related()[0].report();
    assert_eq!(related.effect(), StateEffect::RecoveryRequired);
    assert_eq!(related.primary().code(), "publication.discard_failed");
    let wire = serde_json::to_string(&report).expect("丢弃诊断必须可序列化");
    assert!(wire.contains("\"operation\":\"remove\""));
    assert!(!wire.contains("discard_directory_candidate"));
    assert!(wire.contains("\"raw_os_code\":32"));
    assert!(wire.contains(&stage_root.to_string_lossy().to_string()));

    let GenericCommandError::PublishDiscard { discard, .. } = &error else {
        unreachable!("上方已建立 PublishDiscard")
    };
    let discard_source = discard
        .source_error()
        .downcast_ref::<DirectoryDiscardError<Box<SystemFileSystemError>>>()
        .expect("应保留真实目录丢弃错误");
    let SystemFileSystemError::Io { source, .. } = discard_source.source().as_ref() else {
        panic!("目录丢弃错误应保留文件系统 I/O 原因")
    };
    assert_eq!(source.raw_os_error(), Some(32));

    assert_eq!(
        generic_command_error_report(&error).effect(),
        StateEffect::RecoveryRequired
    );
}

#[test]
fn profile_is_remembered_separately_only_without_committed_translation() {
    let only_conflicts = GenericTranslationSummary {
        conflicted_units: 3,
        ..GenericTranslationSummary::default()
    };
    assert!(
        should_remember_profile_separately(&only_conflicts),
        "没有译文成功写入时，即使存在冲突也仍要保存本轮 Profile"
    );

    let committed_with_conflicts = GenericTranslationSummary {
        written_units: 1,
        conflicted_units: 3,
        ..GenericTranslationSummary::default()
    };
    assert!(
        !should_remember_profile_separately(&committed_with_conflicts),
        "成功的译文提交已在同一事务保存 Profile，不应再启动独立写事务"
    );
}

#[test]
fn zero_requests_with_only_current_rejected_units_is_incomplete() {
    let summary = GenericTranslationSummary {
        total_tasks: 0,
        started_tasks: 0,
        planned_units: 1,
        remaining_units: 1,
        rejected_units: 1,
        ..GenericTranslationSummary::default()
    };

    assert!(summary.is_incomplete());
    assert_eq!(
        summary.total_tasks, 0,
        "默认重跑不得为 current Rejected 发请求"
    );
}

#[test]
fn reused_translation_keeps_quote_style_when_reprotected_for_model_context() {
    let temporary = tempfile::tempdir().expect("应该可建立临时目录");
    let source_root = temporary.path().join("source");
    fs::create_dir_all(&source_root).expect("应该可建立输入目录");
    fs::write(
        source_root.join("scene.jsonl"),
        concat!(
            r#"{"id":"current","kind":"dialogue","units":[{"id":"unit","text":"「こんにちは {name}」"}]}"#,
            "\n",
            r#"{"id":"reuse","kind":"dialogue","units":[{"id":"unit","text":"「こんにちは {name}」"}]}"#,
            "\n",
            r#"{"id":"model","kind":"dialogue","units":[{"id":"unit","text":"さようなら"}]}"#,
            "\n"
        ),
    )
    .expect("应该可写入 Generic 输入");
    let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
        project_name: "reuse-context-test".parse().expect("项目名应该合法"),
        workspace_root: temporary.path().join("project"),
        source_root: Some(source_root),
        source_language: Some(LanguageId::parse("ja").expect("源语言应该合法")),
        target_language: Some(LanguageId::parse("zh-Hans").expect("目标语言应该合法")),
    })
    .expect("Generic 项目应该可初始化");
    store.extract().expect("Generic 输入应该可提取");
    let snapshot = store.load_snapshot().expect("应该可读取 Generic 快照");
    let placeholder_rules = GenericPlaceholderService::default()
        .compile(vec![GenericPlaceholderRuleDefinition::new(
            Some(vec!["dialogue".to_owned()]),
            r"\{[^}]+\}",
        )])
        .expect("Placeholder 规则应该合法");
    let current_group = &snapshot.files()[0].groups()[0];
    let current_unit = &current_group.units()[0];
    apply_test_manual_translation(
        &store,
        TestManualTranslation {
            id: "scene.jsonl:line1:unit1:text",
            relative_path: "scene.jsonl",
            group_id: current_group.id(),
            unit_id: current_unit.id(),
            kind: current_group.kind(),
            source: current_unit.source_text(),
            translation: "“你好 {name}”",
        },
    );
    let snapshot = store.load_snapshot().expect("应该可重读 Generic 快照");
    let language_module: Arc<dyn LanguageModule> = Arc::new(JapaneseLanguageModule::new(
        JapaneseResidualPolicy::new(NonZeroUsize::MIN, Vec::new()).expect("日文残留策略应该合法"),
    ));
    let terminology = Arc::new(CompiledTerminology::empty());
    let prepared = prepare_generic_translation(
        &snapshot,
        Arc::clone(&terminology),
        &placeholder_rules,
        &GenericPlaceholderRuleSource::ProjectSnapshot,
        language_module,
        NonZeroUsize::new(10_000).expect("常量应该非零"),
        false,
        &CooperativeCancellation::default(),
    )
    .expect("翻译任务应该可规划");

    assert_eq!(prepared.plan().reused().len(), 1);
    assert_eq!(prepared.plan().reused()[0].key().group_id(), "reuse");
    assert_eq!(
        prepared.plan().reused()[0].translation(),
        "“你好 {name}”",
        "Translate 验收不得改写合格译文的引号风格"
    );
    let task = &prepared.plan().tasks()[0];
    assert_eq!(task.groups().len(), 3);
    let current_context = task.groups()[0].units()[0].text();
    let reuse_context = task.groups()[1].units()[0].text();
    assert!(current_context.starts_with("“你好 ") && current_context.ends_with('”'));
    assert!(reuse_context.starts_with("“你好 ") && reuse_context.ends_with('”'));
    assert!(placeholder_token::contains_reserved_prefix(reuse_context));
    assert_eq!(task.groups()[1].units()[0].output_id(), None);
    assert_eq!(task.groups()[2].units()[0].output_id(), Some(task_id(0)));
    let message = render_generic_user_message(task, terminology.as_ref());
    let wire = compact_json(&message);
    assert!(wire.contains("“你好 "));
    assert!(wire.contains("\"id\":\"0\""));
    assert!(!message.contains("{name}"));
}

#[test]
fn write_back_uses_applicable_translations_without_a_model_profile() {
    let temporary = tempfile::tempdir().expect("应该可建立临时目录");
    let source_root = temporary.path().join("source");
    fs::create_dir_all(&source_root).expect("应该可建立输入目录");
    fs::write(
        source_root.join("scene.jsonl"),
        concat!(
            r#"{"id":"group","kind":"dialogue","units":["#,
            r#"{"id":"manual","text":"手動"},"#,
            r#"{"id":"automatic","text":"自動"}]}"#,
            "\n"
        ),
    )
    .expect("应该可写入 Generic 输入");
    let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
        project_name: "current-test".parse().expect("项目名应该合法"),
        workspace_root: temporary.path().join("project"),
        source_root: Some(source_root),
        source_language: Some(LanguageId::parse("ja").expect("源语言应该合法")),
        target_language: Some(LanguageId::parse("zh-Hans").expect("目标语言应该合法")),
    })
    .expect("Generic 项目应该可初始化");
    store.extract().expect("Generic 输入应该可提取");
    let snapshot = store.load_snapshot().expect("应该可读取 Generic 快照");
    assert!(snapshot.project().last_profile_id().is_none());
    let group = &snapshot.files()[0].groups()[0];
    let manual = &group.units()[0];
    apply_test_manual_translation(
        &store,
        TestManualTranslation {
            id: "scene.jsonl:line1:unit1:text",
            relative_path: "scene.jsonl",
            group_id: group.id(),
            unit_id: manual.id(),
            kind: group.kind(),
            source: manual.source_text(),
            translation: "人工译文",
        },
    );
    let automatic = &group.units()[1];
    let automatic_state = automatic_translation_state_fingerprint(
        snapshot.project().language_pair(),
        &GenericUnitKey::new(group.id().to_owned(), automatic.id().to_owned()),
        automatic.source_text(),
        group.context_fingerprint(),
    );
    store
        .commit_translations(
            snapshot
                .project()
                .extracted_raw_fingerprint()
                .expect("Extract 应保存原始指纹"),
            &[crate::generic::TranslationWrite {
                group_id: group.id().to_owned(),
                unit_id: automatic.id().to_owned(),
                expected_source_text: automatic.source_text().to_owned(),
                expected_group_context: group.context_fingerprint(),
                translation: "自动译文".to_owned(),
                state_fingerprint: automatic_state,
                expected_translation: None,
                was_current_rejected: false,
            }],
        )
        .expect("应该可保存测试译文");
    let (stored, live) = store.ensure_input_current().expect("输入应该仍为 Current");
    let current =
        collect_generic_current_translations(&stored, &CooperativeCancellation::default())
            .expect("人工 Current 应可独立计算");
    let manual_key = GenericUnitKey::new("group".to_owned(), "manual".to_owned());
    assert_eq!(
        current
            .get_with_cancellation(&manual_key, || { Ok::<_, std::convert::Infallible>(()) })
            .unwrap_or_else(|never| match never {})
            .map(GenericCurrentTranslation::text),
        Some("人工译文")
    );
    let automatic_key = GenericUnitKey::new("group".to_owned(), "automatic".to_owned());
    assert_eq!(
        current
            .get_with_cancellation(&automatic_key, || { Ok::<_, std::convert::Infallible>(()) })
            .unwrap_or_else(|never| match never {})
            .map(GenericCurrentTranslation::text),
        Some("自动译文")
    );
    let candidate =
        build_write_back_candidate(&stored, &live, &current).expect("Partial 应允许写回");
    assert_eq!(candidate.translated_units(), 2);
    assert_eq!(candidate.retained_source_units(), 0);
}

#[test]
fn write_back_rejects_manual_translation_when_placeholder_rules_change() {
    let temporary = tempfile::tempdir().expect("应该可建立临时目录");
    let source_root = temporary.path().join("source");
    fs::create_dir_all(&source_root).expect("应该可建立输入目录");
    fs::write(
        source_root.join("scene.jsonl"),
        concat!(
            r#"{"id":"group","kind":"dialogue","units":["#,
            r#"{"id":"unit","text":"Open [A]."}]}"#,
            "\n"
        ),
    )
    .expect("应该可写入 Generic 输入");
    let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
        project_name: "current-placeholder-test".parse().expect("项目名应该合法"),
        workspace_root: temporary.path().join("project"),
        source_root: Some(source_root),
        source_language: Some(LanguageId::parse("en").expect("源语言应该合法")),
        target_language: Some(LanguageId::parse("zh-Hans").expect("目标语言应该合法")),
    })
    .expect("Generic 项目应该可初始化");
    store.extract().expect("Generic 输入应该可提取");
    let snapshot = store.load_snapshot().expect("应该可读取 Generic 快照");
    let group = &snapshot.files()[0].groups()[0];
    let unit = &group.units()[0];
    let rules = GenericPlaceholderService::default()
        .compile(vec![GenericPlaceholderRuleDefinition::new(
            Some(vec!["dialogue".to_owned()]),
            r"\[[^]]+\]",
        )])
        .expect("Placeholder 规则应该合法");
    apply_test_manual_translation(
        &store,
        TestManualTranslation {
            id: "scene.jsonl:line1:unit1:text",
            relative_path: "scene.jsonl",
            group_id: group.id(),
            unit_id: unit.id(),
            kind: group.kind(),
            source: unit.source_text(),
            translation: "打开 [B]。",
        },
    );
    let (stored, live) = store
        .ensure_input_current()
        .expect("应该可重新读取当前 Generic 输入");
    let current =
        collect_generic_current_translations(&stored, &CooperativeCancellation::default())
            .expect("应该可读取数据库中的人工译文");
    let key = GenericUnitKey::new("group".to_owned(), "unit".to_owned());
    current
        .get_with_cancellation(&key, || Ok::<_, std::convert::Infallible>(()))
        .unwrap_or_else(|never| match never {})
        .expect("人工译文应该仍为 Current");
    let layout_rules = compile_generic_layout_rules(
        &live,
        &LayoutRuleSet::from_canonical_json("[]").expect("空排版规则必须有效"),
    )
    .expect("空排版规则必须适用于任意 Generic 输入");
    let error = build_write_back_candidate_with_cancellation(
        &stored,
        &live,
        &current,
        &rules,
        &layout_rules,
        GenericWriteBackTextOptions::new(true, true),
        &CooperativeCancellation::default(),
    );
    assert!(
        matches!(
            error,
            Err(GenericWriteBackError::PlaceholderBindingMismatch { .. })
        ),
        "WriteBack 必须使用与 Translate 相同的强校验，不能因译文来源是 Manual 而跳过"
    );
}

#[tokio::test]
async fn publish_recheck_rejects_source_changed_after_candidate_and_preserves_previous_output() {
    let temporary = tempfile::tempdir().expect("应该可建立临时目录");
    let source_root = temporary.path().join("source");
    fs::create_dir_all(&source_root).expect("应该可建立输入目录");
    fs::write(
        source_root.join("scene.jsonl"),
        concat!(
            r#"{"id":"group","kind":"dialogue","units":["#,
            r#"{"id":"unit","text":"原文"}]}"#,
            "\n"
        ),
    )
    .expect("应该可写入 Generic 输入");
    let workspace_root = temporary.path().join("project");
    let project_name: ProjectName = "publish-recheck".parse().expect("项目名应该合法");
    let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
        project_name: project_name.clone(),
        workspace_root: workspace_root.clone(),
        source_root: Some(source_root.clone()),
        source_language: Some(LanguageId::parse("ja").expect("源语言应该合法")),
        target_language: Some(LanguageId::parse("zh-Hans").expect("目标语言应该合法")),
    })
    .expect("Generic 项目应该可初始化");
    store.extract().expect("Generic 输入应该可提取");
    let (stored, live) = store.ensure_input_current().expect("首次复查应该通过");
    let project = stored.project().clone();
    let candidate = build_write_back_candidate(&stored, &live, &GenericUnitMap::new())
        .expect("应该可用首次复查快照建立候选");
    let output_root = project.write_back_root();
    fs::create_dir_all(&output_root).expect("应该可建立上一次输出");
    fs::write(output_root.join("previous.txt"), b"previous").expect("应该可写入上一次输出");

    fs::write(
        source_root.join("scene.jsonl"),
        concat!(
            r#"{"id":"group","kind":"dialogue","units":["#,
            r#"{"id":"unit","text":"候选后变化"}]}"#,
            "\n"
        ),
    )
    .expect("应该可在候选建立后修改外部输入");
    let file_system = SystemFileSystem::new().expect("应该可建立文件运行能力");
    let publisher = file_system.directory_publisher(
        crate::runtime::filesystem::DirectoryPublisherConfig::production(
            temporary.path().join("publish-locks"),
        )
        .expect("发布锁配置应该合法"),
    );

    let result = publish_generic_write_back(
        publisher,
        project_name,
        project,
        candidate,
        CooperativeCancellation::default(),
        GenericWriteBackPublicationGate::default(),
        None,
        generic_terminal_occurrence_slot(),
        || {},
    )
    .await;

    assert!(result.is_err(), "发布前输入变化必须拒绝发布");
    assert_eq!(
        fs::read(output_root.join("previous.txt")).expect("上一次输出应该仍可读取"),
        b"previous"
    );
    assert_eq!(
        fs::read_dir(&output_root)
            .expect("应该可列举上一次输出")
            .count(),
        1,
        "失败不得把候选内容混入上一次输出"
    );
    assert!(
        fs::read_dir(&workspace_root)
            .expect("应该可列举项目工作区")
            .filter_map(Result::ok)
            .all(|entry| entry.file_name().to_string_lossy() != WRITE_BACK_SCRATCH_NAME),
        "发布前复查失败后不应残留 Generic 写回暂存目录"
    );
    file_system
        .shutdown()
        .await
        .expect("文件运行能力应该可终结");
}

#[tokio::test]
async fn publish_setup_failure_removes_materialized_scratch() {
    let temporary = tempfile::tempdir().expect("应该可建立临时目录");
    let source_root = temporary.path().join("source");
    fs::create_dir_all(&source_root).expect("应该可建立输入目录");
    fs::write(
        source_root.join("scene.jsonl"),
        concat!(
            r#"{"id":"group","kind":"dialogue","units":["#,
            r#"{"id":"unit","text":"原文"}]}"#,
            "\n"
        ),
    )
    .expect("应该可写入 Generic 输入");
    let workspace_root = temporary.path().join("project");
    let project_name: ProjectName = "publish-setup".parse().expect("项目名应该合法");
    let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
        project_name: project_name.clone(),
        workspace_root: workspace_root.clone(),
        source_root: Some(source_root),
        source_language: Some(LanguageId::parse("ja").expect("源语言应该合法")),
        target_language: Some(LanguageId::parse("zh-Hans").expect("目标语言应该合法")),
    })
    .expect("Generic 项目应该可初始化");
    store.extract().expect("Generic 输入应该可提取");
    let (stored, live) = store.ensure_input_current().expect("输入复查应该通过");
    let project = stored.project().clone();
    let candidate = build_write_back_candidate(&stored, &live, &GenericUnitMap::new())
        .expect("应该可建立写回候选");
    let output_root = project.write_back_root();
    fs::write(&output_root, b"occupied").expect("应该可建立非目录发布目标");
    let file_system = SystemFileSystem::new().expect("应该可建立文件运行能力");
    let publisher = file_system.directory_publisher(
        crate::runtime::filesystem::DirectoryPublisherConfig::production(
            temporary.path().join("publish-locks"),
        )
        .expect("发布锁配置应该合法"),
    );

    let result = publish_generic_write_back(
        publisher,
        project_name,
        project,
        candidate,
        CooperativeCancellation::default(),
        GenericWriteBackPublicationGate::default(),
        None,
        generic_terminal_occurrence_slot(),
        || {},
    )
    .await;

    assert!(result.is_err(), "非目录目标必须阻止发布");
    assert_eq!(
        fs::read(&output_root).expect("原目标文件应该保持"),
        b"occupied"
    );
    assert!(
        fs::read_dir(&workspace_root)
            .expect("应该可列举项目工作区")
            .filter_map(Result::ok)
            .all(|entry| entry.file_name().to_string_lossy() != WRITE_BACK_SCRATCH_NAME),
        "发布请求建立失败后不应残留 Generic 写回暂存目录"
    );
    file_system
        .shutdown()
        .await
        .expect("文件运行能力应该可终结");
}

#[test]
fn review_only_committed_task_has_one_complete_terminal_for_all_consumers() {
    let review = generic_task_response_diagnostic(
        0,
        1,
        GenericTaskResponseProblem::ResponseReview {
            finding: GenericResponseReviewFinding::NonStopFinish,
        },
    );
    let result = GenericCommittedTaskFinalResult::new(true, vec![0], 1, vec![review]);

    assert!(result.is_complete());
    assert!(matches!(result.terminal, GenericTaskTerminal::Complete));
    assert_eq!(result.diagnostics.len(), 1, "Review 诊断仍应保留");
    let record = result.task_record_state();
    assert_eq!(record.code_for_test(), "complete");
}
