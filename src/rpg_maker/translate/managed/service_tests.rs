use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tokio::sync::{Notify, Semaphore};

use crate::execution::cpu::CpuTaskExecutionError;
use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::llm::{ChatMessage, ChatMessageRole, LlmFinishReason, LlmResponse};
use crate::rpg_maker::ProjectName;
use crate::rpg_maker::lua::runtime::{
    TrustedLuaPreparedTranslationAcceptance, TrustedLuaPreparedTranslationStatus,
    TrustedLuaTranslationTerm,
};
use crate::rpg_maker::project::test_layout_profile;
use crate::rpg_maker::translate::task_record::{
    ManagedTranslationTaskRecordDocument, TranslationTaskRecordDocument,
};

use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
struct ServiceTestError(String);

impl ServiceTestError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ServiceTestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for ServiceTestError {}

impl SafeDiagnosticSource for ServiceTestError {
    fn safe_diagnostic_source(
        &self,
        stage: DiagnosticStage,
        impact: DiagnosticImpact,
        fallback_action: DiagnosticAction,
    ) -> SafeDiagnostic {
        SafeDiagnostic::new(
            DiagnosticCode::SqliteOperation,
            stage,
            DiagnosticSubject::component("managed_service_test_repository"),
            DiagnosticReason::failure(DiagnosticFailureKind::TransactionRolledBack),
            impact,
            fallback_action,
        )
    }
}

impl LlmRequestDiagnosticSource for ServiceTestError {
    fn request_diagnostic(
        &self,
        _retry_after: Option<Duration>,
        impact: DiagnosticImpact,
    ) -> SafeDiagnostic {
        SafeDiagnostic::new(
            DiagnosticCode::ModelRequest,
            DiagnosticStage::ModelRequest,
            DiagnosticSubject::component("managed_service_test_llm"),
            DiagnosticReason::failure(DiagnosticFailureKind::TransportFailed),
            impact,
            DiagnosticAction::Retry,
        )
    }
}

#[derive(Clone)]
struct ServiceSemantics;

impl TrustedLuaTranslationSemantics for ServiceSemantics {
    fn system_prompt(&self) -> &str {
        "system"
    }

    fn source_language(&self) -> &str {
        "ja"
    }

    fn target_language(&self) -> &str {
        "zh-Hans"
    }

    fn prepare_translation(
        &self,
        kind: TextGroupKind,
        original: String,
        semantic_context: String,
    ) -> Result<Arc<dyn TrustedLuaPreparedTranslation>, TrustedLuaHostCallError> {
        Ok(Arc::new(ServicePrepared::new(
            kind,
            original,
            semantic_context,
        )))
    }

    fn prepare_translation_lines(
        &self,
        kind: TextGroupKind,
        original: Vec<String>,
        semantic_context: String,
    ) -> Result<Arc<dyn TrustedLuaPreparedTranslation>, TrustedLuaHostCallError> {
        Ok(Arc::new(ServicePrepared::new(
            kind,
            original.join("\n"),
            semantic_context,
        )))
    }
}

struct ServicePrepared {
    model_text: String,
    fingerprint: Sha256Fingerprint,
    terms: Vec<TrustedLuaTranslationTerm>,
}

impl ServicePrepared {
    fn new(kind: TextGroupKind, model_text: String, semantic_context: String) -> Self {
        let mut hasher = Sha256FramedHasher::new(b"att.test.managed.service.prepared");
        hasher
            .frame(1, kind.storage_name().as_bytes())
            .frame(2, model_text.as_bytes())
            .frame(3, semantic_context.as_bytes());
        Self {
            model_text,
            fingerprint: hasher.finish(),
            terms: Vec::new(),
        }
    }
}

impl TrustedLuaPreparedTranslation for ServicePrepared {
    fn status(&self) -> TrustedLuaPreparedTranslationStatus {
        TrustedLuaPreparedTranslationStatus::Active
    }

    fn model_text(&self) -> &str {
        &self.model_text
    }

    fn terms(&self) -> &[TrustedLuaTranslationTerm] {
        &self.terms
    }

    fn semantic_fingerprint(&self) -> Sha256Fingerprint {
        self.fingerprint
    }

    fn is_current(
        &self,
        translation: String,
        state: Sha256Fingerprint,
    ) -> Result<bool, TrustedLuaHostCallError> {
        Ok(state == accepted_state(&translation))
    }

    fn accept(
        &self,
        candidate: String,
    ) -> Result<TrustedLuaPreparedTranslationAcceptance, TrustedLuaHostCallError> {
        let state = accepted_state(&candidate);
        Ok(TrustedLuaPreparedTranslationAcceptance::accepted(
            candidate, state,
        ))
    }
}

fn accepted_state(value: &str) -> Sha256Fingerprint {
    let mut hasher = Sha256FramedHasher::new(b"att.test.managed.service.accepted");
    hasher.frame(1, value.as_bytes());
    hasher.finish()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CheckpointScript {
    Applied,
    NotApplied,
    OutcomeUnknown,
}

struct RepositoryState {
    snapshot: ManagedTranslationSnapshot,
    task_scripts: VecDeque<CheckpointScript>,
    checkpoint_calls: usize,
    task_checkpoint_calls: usize,
    committed_keys: Vec<String>,
}

#[derive(Clone)]
struct ServiceRepository {
    state: Arc<Mutex<RepositoryState>>,
}

impl ServiceRepository {
    fn new(snapshot: ManagedTranslationSnapshot, task_scripts: Vec<CheckpointScript>) -> Self {
        Self {
            state: Arc::new(Mutex::new(RepositoryState {
                snapshot,
                task_scripts: task_scripts.into(),
                checkpoint_calls: 0,
                task_checkpoint_calls: 0,
                committed_keys: Vec::new(),
            })),
        }
    }

    fn snapshot(&self) -> ManagedTranslationSnapshot {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot
            .clone()
    }

    fn checkpoint_calls(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .checkpoint_calls
    }

    fn task_checkpoint_calls(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .task_checkpoint_calls
    }

    fn committed_keys(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .committed_keys
            .clone()
    }
}

impl ManagedTranslationRepository for ServiceRepository {
    type DriverError = ServiceTestError;
    type Error = ServiceTestError;

    async fn load(
        &self,
        _project: &OpenedProject,
    ) -> Result<Option<ManagedTranslationSnapshot>, Self::Error> {
        Ok(Some(self.snapshot()))
    }

    fn is_source_stale(_error: &Self::Error) -> bool {
        false
    }

    async fn checkpoint(
        &self,
        _project: &OpenedProject,
        checkpoint: ManagedTranslationCheckpoint,
    ) -> Result<
        ManagedTranslationCheckpointOutcome<Self::DriverError>,
        ManagedTranslationCheckpointError<Self::DriverError>,
    > {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let expected = checkpoint
            .expected_snapshot(&state.snapshot)
            .expect("测试仓库收到的 checkpoint 必须匹配当前快照");
        let changed_keys = changed_translation_keys(&state.snapshot, &expected);
        let is_preflight = state.checkpoint_calls == 0;
        state.checkpoint_calls += 1;
        if is_preflight {
            assert!(
                changed_keys.is_empty(),
                "无既有译文的服务测试中预检 guard 不应改变快照"
            );
            state.snapshot = expected;
            return Ok(ManagedTranslationCheckpointOutcome::Applied);
        }

        state.task_checkpoint_calls += 1;
        match state
            .task_scripts
            .pop_front()
            .unwrap_or(CheckpointScript::Applied)
        {
            CheckpointScript::Applied => {
                state.snapshot = expected;
                state.committed_keys.extend(changed_keys);
                Ok(ManagedTranslationCheckpointOutcome::Applied)
            }
            CheckpointScript::NotApplied => Ok(ManagedTranslationCheckpointOutcome::NotApplied),
            CheckpointScript::OutcomeUnknown => {
                Ok(ManagedTranslationCheckpointOutcome::OutcomeUnknown(
                    ServiceTestError::new("checkpoint outcome unknown"),
                ))
            }
        }
    }
}

fn changed_translation_keys(
    before: &ManagedTranslationSnapshot,
    after: &ManagedTranslationSnapshot,
) -> Vec<String> {
    let mut changed = Vec::new();
    for collection in before.collections() {
        let after_collection = after
            .collection(collection.name())
            .expect("checkpoint 投影不得删除 collection");
        for unit in collection.units() {
            let after_unit = after_collection
                .unit(unit.key())
                .expect("checkpoint 投影不得删除 unit");
            if unit.translation() != after_unit.translation() {
                changed.push(unit.key().to_owned());
            }
        }
    }
    changed
}

#[derive(Clone, Copy)]
struct InlineCpu;

impl CpuTaskExecutor for InlineCpu {
    type Error = ServiceTestError;

    async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        Ok(task())
    }
}

#[derive(Clone, Default)]
struct RecordingDelay {
    waits: Arc<Mutex<Vec<Duration>>>,
}

impl RecordingDelay {
    fn waits(&self) -> Vec<Duration> {
        self.waits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl AsyncDelay for RecordingDelay {
    async fn wait(&self, duration: Duration) {
        self.waits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(duration);
        tokio::task::yield_now().await;
    }
}

#[derive(Clone)]
struct ServiceClient {
    maximum: NonZeroUsize,
}

impl LlmClientConcurrency for ServiceClient {
    fn max_concurrent_requests(&self) -> NonZeroUsize {
        self.maximum
    }
}

#[derive(Clone, Copy, Debug)]
enum RequestBehavior {
    Success,
    RetryOnce { retry_after: Option<Duration> },
    AlwaysRetry { retry_after: Option<Duration> },
    Fatal,
}

#[derive(Clone)]
struct UnitRequestScript {
    behavior: RequestBehavior,
    gate: Option<Arc<Semaphore>>,
}

impl UnitRequestScript {
    fn immediate(behavior: RequestBehavior) -> Self {
        Self {
            behavior,
            gate: None,
        }
    }

    fn gated(behavior: RequestBehavior, gate: Arc<Semaphore>) -> Self {
        Self {
            behavior,
            gate: Some(gate),
        }
    }
}

struct ScriptedLlmState {
    scripts: Vec<UnitRequestScript>,
    attempts: Mutex<Vec<usize>>,
    started_units: Mutex<Vec<usize>>,
    system_prompts: Mutex<Vec<String>>,
    started_count: AtomicUsize,
    completed_count: AtomicUsize,
    active: AtomicUsize,
    maximum_active: AtomicUsize,
    started_notification: Notify,
    completed_notification: Notify,
}

#[derive(Clone)]
struct ScriptedLlm {
    state: Arc<ScriptedLlmState>,
}

impl ScriptedLlm {
    fn new(scripts: Vec<UnitRequestScript>) -> Self {
        let unit_count = scripts.len();
        Self {
            state: Arc::new(ScriptedLlmState {
                scripts,
                attempts: Mutex::new(vec![0; unit_count]),
                started_units: Mutex::new(Vec::new()),
                system_prompts: Mutex::new(Vec::new()),
                started_count: AtomicUsize::new(0),
                completed_count: AtomicUsize::new(0),
                active: AtomicUsize::new(0),
                maximum_active: AtomicUsize::new(0),
                started_notification: Notify::new(),
                completed_notification: Notify::new(),
            }),
        }
    }

    fn started_count(&self) -> usize {
        self.state.started_count.load(Ordering::Acquire)
    }

    fn completed_count(&self) -> usize {
        self.state.completed_count.load(Ordering::Acquire)
    }

    fn active(&self) -> usize {
        self.state.active.load(Ordering::Acquire)
    }

    fn maximum_active(&self) -> usize {
        self.state.maximum_active.load(Ordering::Acquire)
    }

    fn system_prompts(&self) -> Vec<String> {
        self.state
            .system_prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn started_units(&self) -> Vec<usize> {
        self.state
            .started_units
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    async fn wait_for_started(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let notified = self.state.started_notification.notified();
                if self.started_count() >= expected {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("模型请求未在预期时间内准入");
    }

    async fn wait_for_completed(&self, expected: usize) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let notified = self.state.completed_notification.notified();
                if self.completed_count() >= expected {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("模型请求未在预期时间内结束");
    }
}

struct ActiveRequest<'a> {
    state: &'a ScriptedLlmState,
}

impl Drop for ActiveRequest<'_> {
    fn drop(&mut self) {
        self.state.active.fetch_sub(1, Ordering::AcqRel);
        self.state.completed_count.fetch_add(1, Ordering::AcqRel);
        self.state.completed_notification.notify_waiters();
    }
}

impl LlmRequestExecutor for ScriptedLlm {
    type Client = ServiceClient;
    type Error = ServiceTestError;

    async fn request<'a>(
        &'a self,
        _client: &'a Self::Client,
        messages: &'a [ChatMessage],
    ) -> Result<LlmResponse, LlmRequestError<Self::Error>> {
        let system_prompt = messages
            .iter()
            .find(|message| message.role() == ChatMessageRole::System)
            .expect("Managed 请求必须包含 System message")
            .content()
            .to_owned();
        self.state
            .system_prompts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(system_prompt);
        let unit = request_unit_index(messages);
        let script = self
            .state
            .scripts
            .get(unit)
            .unwrap_or_else(|| panic!("未给 source-{unit:02} 配置模型脚本"))
            .clone();
        let attempt = {
            let mut attempts = self
                .state
                .attempts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let attempt = attempts[unit];
            attempts[unit] += 1;
            attempt
        };
        self.state
            .started_units
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(unit);
        self.state.started_count.fetch_add(1, Ordering::AcqRel);
        let active = self.state.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.state
            .maximum_active
            .fetch_max(active, Ordering::AcqRel);
        self.state.started_notification.notify_waiters();
        let _active_request = ActiveRequest { state: &self.state };

        if let Some(gate) = script.gate {
            gate.acquire()
                .await
                .expect("测试模型请求闸门不应关闭")
                .forget();
        }

        match script.behavior {
            RequestBehavior::Success => Ok(success_response(unit)),
            RequestBehavior::RetryOnce { retry_after } if attempt == 0 => {
                Err(LlmRequestError::Retryable {
                    source: ServiceTestError::new(format!("retry-source-{unit:02}")),
                    retry_after,
                })
            }
            RequestBehavior::RetryOnce { .. } => Ok(success_response(unit)),
            RequestBehavior::AlwaysRetry { retry_after } => Err(LlmRequestError::Retryable {
                source: ServiceTestError::new(format!("retry-source-{unit:02}")),
                retry_after,
            }),
            RequestBehavior::Fatal => Err(LlmRequestError::Fatal(ServiceTestError::new(format!(
                "fatal-source-{unit:02}"
            )))),
        }
    }
}

fn request_unit_index(messages: &[ChatMessage]) -> usize {
    let user = messages
        .iter()
        .find(|message| message.role() == ChatMessageRole::User)
        .expect("Managed 请求必须包含 User message")
        .content();
    let marker = "source-";
    let start = user
        .find(marker)
        .unwrap_or_else(|| panic!("User message 缺少测试原文标记：{user}"))
        + marker.len();
    let digits = user[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    digits.parse().expect("测试原文标记必须包含数字序号")
}

fn success_response(unit: usize) -> LlmResponse {
    LlmResponse::new(
        serde_json::to_string(&json!({"1": [format!("target-{unit:02}")]}))
            .expect("测试响应必须可编码"),
        LlmFinishReason::Stop,
        None,
        None,
        None,
    )
}

#[derive(Clone, Default)]
struct DisabledTaskRecords {
    submitted: Arc<AtomicUsize>,
    declared_totals: Arc<Mutex<Vec<usize>>>,
}

impl DisabledTaskRecords {
    fn submitted(&self) -> usize {
        self.submitted.load(Ordering::Acquire)
    }
}

impl TranslationTaskRecordSink for DisabledTaskRecords {
    fn enabled(&self) -> bool {
        false
    }

    fn declare_total_tasks(&self, total_tasks: usize) {
        self.declared_totals
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(total_tasks);
    }

    fn submit(&self, _document: TranslationTaskRecordDocument) {
        self.submitted.fetch_add(1, Ordering::AcqRel);
    }

    fn submit_managed(&self, _document: ManagedTranslationTaskRecordDocument) {
        self.submitted.fetch_add(1, Ordering::AcqRel);
    }
}

#[derive(Default)]
struct CapturingTaskLog {
    events: Mutex<Vec<StandardTranslationLogEvent>>,
}

impl CapturingTaskLog {
    fn events(&self) -> Vec<StandardTranslationLogEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl StandardTranslationLog for CapturingTaskLog {
    fn emit(&self, event: StandardTranslationLogEvent) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
    }
}

type ServiceExecution = BoundManagedTranslationExecution<
    ScriptedLlm,
    RecordingDelay,
    InlineCpu,
    ServiceRepository,
    DisabledTaskRecords,
>;

struct ServiceHarness {
    execution: ServiceExecution,
    llm: ScriptedLlm,
    delay: RecordingDelay,
    repository: ServiceRepository,
    records: DisabledTaskRecords,
}

#[allow(clippy::too_many_arguments)]
fn service_harness(
    project: OpenedProject,
    snapshot: ManagedTranslationSnapshot,
    scripts: Vec<UnitRequestScript>,
    task_checkpoints: Vec<CheckpointScript>,
    concurrency: usize,
    retry_delays: Vec<Duration>,
    maximum_retry_after: Duration,
    cancellation: CooperativeCancellation,
    task_log: Option<Arc<CapturingTaskLog>>,
) -> ServiceHarness {
    let llm = ScriptedLlm::new(scripts);
    let delay = RecordingDelay::default();
    let repository = ServiceRepository::new(snapshot, task_checkpoints);
    let records = DisabledTaskRecords::default();
    let task_log = task_log.map(|task_log| task_log as Arc<dyn StandardTranslationLog>);
    let managed_system_prompt = RpgMakerSystemPrompt::new(
        project.language_pair().clone(),
        "managed system".to_owned(),
        TranslationResponseEnvelope::JsonOnly,
    )
    .expect("测试 Managed Prompt 应合法");
    let execution = BoundManagedTranslationExecution {
        llm: llm.clone(),
        delay: delay.clone(),
        cpu: InlineCpu,
        repository: repository.clone(),
        planning: RpgMakerTranslationPlanningConfiguration::new(NonZeroUsize::MIN),
        request: RpgMakerTranslationRequestConfiguration::new(retry_delays, maximum_retry_after),
        managed_system_prompt,
        task_records: records.clone(),
        task_log,
        cancellation,
        project,
        llm_client: Arc::new(ServiceClient {
            maximum: NonZeroUsize::new(concurrency).expect("测试模型并发上限必须大于零"),
        }),
        semantics: Arc::new(ServiceSemantics),
        standard_task_count: 0,
    };
    ServiceHarness {
        execution,
        llm,
        delay,
        repository,
        records,
    }
}

fn project() -> OpenedProject {
    OpenedProject::new(
        "managed-service-tests"
            .parse::<ProjectName>()
            .expect("测试项目名应合法"),
        PathBuf::from("C:/projects/managed-service-tests"),
        PathBuf::from("C:/projects/managed-service-tests/project.db"),
        "ja".to_owned(),
        "zh-Hans".to_owned(),
        test_layout_profile(),
    )
}

fn snapshot(project: &OpenedProject, unit_count: usize) -> ManagedTranslationSnapshot {
    let units = (0..unit_count)
        .map(|index| {
            ManagedTranslationUnit::new(
                format!("key-{index:02}"),
                "plugin_parameter",
                ManagedTranslationShape::Single,
                ManagedTranslationContent::scalar(format!("source-{index:02}")),
                "",
                None,
            )
            .expect("测试托管 unit 应合法")
        })
        .collect();
    let collection = ManagedTranslationCollection::new("quests", "translate", units)
        .expect("测试托管 collection 应合法");
    ManagedTranslationSnapshot::new(project.source_snapshot_fingerprint(), vec![collection])
        .expect("测试托管快照应合法")
}

fn immediate_successes(count: usize) -> Vec<UnitRequestScript> {
    (0..count)
        .map(|_| UnitRequestScript::immediate(RequestBehavior::Success))
        .collect()
}

fn report_unit<'a>(
    report: &'a TrustedLuaManagedTranslationReport,
    key: &str,
) -> &'a TrustedLuaManagedTranslationResult {
    report
        .units()
        .iter()
        .find(|unit| unit.key() == key)
        .unwrap_or_else(|| panic!("报告缺少 unit：{key}"))
}

fn persisted_translation(repository: &ServiceRepository, key: &str) -> Option<String> {
    repository
        .snapshot()
        .collection("quests")
        .expect("测试集合应存在")
        .unit(key)
        .expect("测试 unit 应存在")
        .translation()
        .and_then(|pair| match pair.content() {
            ManagedTranslationContent::Scalar(value) => Some(value.clone()),
            ManagedTranslationContent::Array(_) => None,
        })
}

fn error_chain_contains(error: &(dyn Error + 'static), expected: &str) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if error.to_string().contains(expected) {
            return true;
        }
        current = error.source();
    }
    false
}

#[tokio::test]
async fn slow_first_task_refills_window_but_checkpoints_in_natural_order() {
    let project = project();
    let first_gate = Arc::new(Semaphore::new(0));
    let mut scripts = immediate_successes(8);
    scripts[0] = UnitRequestScript::gated(RequestBehavior::Success, Arc::clone(&first_gate));
    let harness = service_harness(
        project.clone(),
        snapshot(&project, 8),
        scripts,
        Vec::new(),
        2,
        Vec::new(),
        Duration::ZERO,
        CooperativeCancellation::default(),
        None,
    );
    let execution = harness.execution;
    let run = execution.run();
    let control = async {
        harness.llm.wait_for_started(6).await;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            harness.llm.started_count(),
            6,
            "慢首任务应允许 refill 到 3N 窗口，但不得越过窗口"
        );
        assert_eq!(harness.repository.task_checkpoint_calls(), 0);
        first_gate.add_permits(1);
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(2), async { tokio::join!(run, control) })
            .await
            .expect("Managed 服务未在闸门释放后结束");
    let (report, _) = result.expect("Managed 服务应成功");

    assert_eq!(harness.llm.started_count(), 8);
    assert!(
        harness
            .llm
            .system_prompts()
            .iter()
            .all(|prompt| prompt == "managed system"),
        "Managed 请求必须使用显式注入的 Managed Prompt，而不是低级 Lua 的 Standard Prompt"
    );
    assert_eq!(harness.llm.active(), 0, "所有已开始请求都必须 drain");
    assert_eq!(harness.llm.maximum_active(), 2);
    assert_eq!(harness.repository.task_checkpoint_calls(), 8);
    assert_eq!(
        harness.repository.committed_keys(),
        (0..8)
            .map(|index| format!("key-{index:02}"))
            .collect::<Vec<_>>(),
        "乱序完成不得改变自然序 checkpoint"
    );
    assert!(
        report
            .units()
            .iter()
            .all(|unit| unit.status() == TrustedLuaManagedTranslationResultStatus::Translated)
    );
}

#[tokio::test]
async fn retry_after_and_retry_budget_become_distinct_managed_results() {
    let project = project();
    let scripts = vec![
        UnitRequestScript::immediate(RequestBehavior::RetryOnce {
            retry_after: Some(Duration::from_millis(30)),
        }),
        UnitRequestScript::immediate(RequestBehavior::AlwaysRetry { retry_after: None }),
        UnitRequestScript::immediate(RequestBehavior::AlwaysRetry {
            retry_after: Some(Duration::from_millis(200)),
        }),
    ];
    let task_log = Arc::new(CapturingTaskLog::default());
    let harness = service_harness(
        project.clone(),
        snapshot(&project, 3),
        scripts,
        Vec::new(),
        3,
        vec![Duration::from_millis(5)],
        Duration::from_millis(100),
        CooperativeCancellation::default(),
        Some(Arc::clone(&task_log)),
    );

    let (report, _) = harness
        .execution
        .run()
        .await
        .expect("预算耗尽和 Retry-After 超限应是普通逐 unit 结果");

    assert_eq!(
        report_unit(&report, "key-00").status(),
        TrustedLuaManagedTranslationResultStatus::Translated
    );
    assert_eq!(
        report_unit(&report, "key-01").reason(),
        Some("request_retry_exhausted")
    );
    assert_eq!(
        report_unit(&report, "key-02").reason(),
        Some("retry_after_exceeds_maximum")
    );
    let retry_after_details = report_unit(&report, "key-02")
        .details_json()
        .expect("Retry-After 超限必须包含结构化详情");
    assert!(retry_after_details.contains("\"retry_after_ms\":200"));
    assert!(retry_after_details.contains("\"maximum_ms\":100"));

    let mut waits = harness.delay.waits();
    waits.sort_unstable();
    assert_eq!(
        waits,
        [Duration::from_millis(5), Duration::from_millis(30)],
        "实际等待应取配置退避与 Retry-After 的较大值"
    );
    assert_eq!(harness.llm.started_count(), 5);
    assert_eq!(harness.repository.task_checkpoint_calls(), 3);
    assert_eq!(
        persisted_translation(&harness.repository, "key-00").as_deref(),
        Some("target-00")
    );
    assert_eq!(persisted_translation(&harness.repository, "key-01"), None);
    assert_eq!(persisted_translation(&harness.repository, "key-02"), None);
    assert_eq!(harness.records.submitted(), 0);

    let events = task_log.events();
    assert!(events.iter().any(|event| matches!(
        event,
        StandardTranslationLogEvent::TaskFinished {
            retry_exhausted: true,
            ..
        }
    )));
}

#[tokio::test]
async fn cancellation_stops_admission_and_drains_started_requests_without_checkpointing() {
    let project = project();
    let first_gate = Arc::new(Semaphore::new(0));
    let second_gate = Arc::new(Semaphore::new(0));
    let mut scripts = immediate_successes(6);
    scripts[0] = UnitRequestScript::gated(RequestBehavior::Success, Arc::clone(&first_gate));
    scripts[1] = UnitRequestScript::gated(RequestBehavior::Success, Arc::clone(&second_gate));
    let cancellation = CooperativeCancellation::default();
    let task_log = Arc::new(CapturingTaskLog::default());
    let harness = service_harness(
        project.clone(),
        snapshot(&project, 6),
        scripts,
        Vec::new(),
        2,
        Vec::new(),
        Duration::ZERO,
        cancellation.clone(),
        Some(Arc::clone(&task_log)),
    );
    let execution = harness.execution;
    let run = execution.run();
    let control = async {
        harness.llm.wait_for_started(2).await;
        cancellation.request();
        first_gate.add_permits(1);
        second_gate.add_permits(1);
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(2), async { tokio::join!(run, control) })
            .await
            .expect("取消后的 Managed 服务必须 drain");
    let error = result.expect_err("合作取消必须返回 cancelled Host 错误");

    assert_eq!(error.kind(), "cancelled");
    assert_eq!(
        harness.llm.started_count(),
        2,
        "取消后不得准入尚未开始的模型请求"
    );
    assert_eq!(harness.llm.completed_count(), 2);
    assert_eq!(harness.llm.active(), 0);
    assert_eq!(
        harness.repository.checkpoint_calls(),
        1,
        "取消只允许此前已经确认的预检 checkpoint"
    );
    assert_eq!(harness.repository.task_checkpoint_calls(), 0);
    assert_eq!(harness.records.submitted(), 0);

    let events = task_log.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, StandardTranslationLogEvent::TaskStarted { .. }))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                StandardTranslationLogEvent::TaskFinished {
                    outcome: StandardTranslationLogTaskOutcome::NotCommitted,
                    ..
                }
            ))
            .count(),
        2,
        "每个已开始请求都必须形成无副作用终态"
    );
}

#[tokio::test]
async fn earliest_natural_fatal_failure_wins_and_disabled_records_keep_log_diagnostics() {
    let project = project();
    let first_gate = Arc::new(Semaphore::new(0));
    let mut scripts = immediate_successes(6);
    scripts[0] = UnitRequestScript::gated(RequestBehavior::Fatal, Arc::clone(&first_gate));
    scripts[1] = UnitRequestScript::immediate(RequestBehavior::Fatal);
    let task_log = Arc::new(CapturingTaskLog::default());
    let harness = service_harness(
        project.clone(),
        snapshot(&project, 6),
        scripts,
        Vec::new(),
        2,
        Vec::new(),
        Duration::ZERO,
        CooperativeCancellation::default(),
        Some(Arc::clone(&task_log)),
    );
    let execution = harness.execution;
    let run = execution.run();
    let control = async {
        harness.llm.wait_for_started(2).await;
        harness.llm.wait_for_completed(1).await;
        first_gate.add_permits(1);
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(2), async { tokio::join!(run, control) })
            .await
            .expect("Fatal 后 Managed 服务必须 drain");
    let error = result.expect_err("Fatal 模型失败必须终止 Managed 服务");

    assert_eq!(error.kind(), "request_failed");
    assert!(
        error_chain_contains(&error, "fatal-source-00"),
        "墙钟更晚但自然序更早的底层失败必须保留在主错误 source 链：{error}"
    );
    assert_eq!(
        harness.llm.started_units(),
        [0, 1],
        "任一 fatal 完成后必须停止新请求准入"
    );
    assert_eq!(harness.llm.active(), 0);
    assert_eq!(harness.repository.task_checkpoint_calls(), 0);
    assert_eq!(harness.records.submitted(), 0);

    let events = task_log.events();
    let finished = events
        .iter()
        .filter_map(|event| match event {
            StandardTranslationLogEvent::TaskFinished {
                outcome,
                diagnostic,
                ..
            } => Some((*outcome, diagnostic.as_ref())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(finished.len(), 2);
    assert_eq!(
        finished[0].0,
        StandardTranslationLogTaskOutcome::ExecutionFailed
    );
    assert_eq!(
        finished[1].0,
        StandardTranslationLogTaskOutcome::NotCommitted
    );
    assert!(
        finished.iter().all(|(_, diagnostic)| diagnostic.is_some()),
        "关闭 task-records 不得吞掉任务日志中的安全诊断"
    );
}

async fn assert_checkpoint_failure_preserves_applied_prefix(
    failing_outcome: CheckpointScript,
    expected_kind: &str,
) {
    let project = project();
    let harness = service_harness(
        project.clone(),
        snapshot(&project, 5),
        immediate_successes(5),
        vec![CheckpointScript::Applied, failing_outcome],
        3,
        Vec::new(),
        Duration::ZERO,
        CooperativeCancellation::default(),
        None,
    );

    let error = harness
        .execution
        .run()
        .await
        .expect_err("第二个任务 checkpoint 的失败终态必须终止 Managed 服务");

    assert_eq!(error.kind(), expected_kind);
    assert_eq!(
        harness.repository.task_checkpoint_calls(),
        2,
        "失败 checkpoint 之后不得继续提交更晚自然序任务"
    );
    assert_eq!(
        harness.repository.committed_keys(),
        ["key-00"],
        "只保留失败前已经确认 Applied 的自然序前缀"
    );
    assert_eq!(
        persisted_translation(&harness.repository, "key-00").as_deref(),
        Some("target-00")
    );
    for index in 1..5 {
        assert_eq!(
            persisted_translation(&harness.repository, &format!("key-{index:02}")),
            None
        );
    }
    assert_eq!(harness.llm.active(), 0);
}

#[tokio::test]
async fn checkpoint_not_applied_preserves_only_confirmed_prefix() {
    assert_checkpoint_failure_preserves_applied_prefix(
        CheckpointScript::NotApplied,
        "checkpoint_not_applied",
    )
    .await;
}

#[tokio::test]
async fn checkpoint_outcome_unknown_stops_after_confirmed_prefix() {
    assert_checkpoint_failure_preserves_applied_prefix(
        CheckpointScript::OutcomeUnknown,
        "checkpoint_outcome_unknown",
    )
    .await;
}
