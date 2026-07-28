//! RPG Maker 对共享 Lua 托管翻译内核的窄适配边界。
//!
//! 本模块只把 RPG Maker 的 TextGroupKind/Placeholder 语义、项目 SQLite checkpoint、
//! 项目日志和 task-record 投影接到引擎无关内核；全局去重、TaskBlock、模型协议、
//! 并发重试、逐 ID 验收、结果传播和自然序提交均由根 Managed 内核唯一拥有。

#[cfg(test)]
#[path = "managed/service_tests.rs"]
mod service_tests;

use std::collections::HashMap;
use std::error::Error;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, RecoveryFact, SafeDiagnostic, SafeDiagnosticSource,
};
use crate::execution::CooperativeCancellation;
use crate::execution::cpu::CpuTaskExecutor;
use crate::execution::llm_request::AsyncDelay;
#[cfg(test)]
use crate::llm::LlmRequestError;
use crate::llm::{LlmClientConcurrency, LlmRequestDiagnosticSource, LlmRequestExecutor};
use crate::managed_translation::{
    ManagedTranslationCheckpointMode as RootManagedCheckpointMode,
    ManagedTranslationKernel as RootManagedKernel,
    ManagedTranslationKernelConfiguration as RootManagedKernelConfiguration,
    ManagedTranslationObserver as RootManagedObserver,
    ManagedTranslationProtocolDiagnostic as RootManagedProtocolDiagnostic,
    ManagedTranslationReplacement as RootManagedReplacement,
    ManagedTranslationResponseRecord as RootManagedResponseRecord,
    ManagedTranslationSemantics as RootManagedSemantics,
    ManagedTranslationStore as RootManagedStore,
    ManagedTranslationStoreCheckpoint as RootManagedStoreCheckpoint,
    ManagedTranslationTaskCheckpointState as RootManagedCheckpointState,
    ManagedTranslationTaskObservation as RootManagedTaskObservation,
    ManagedUnitIdentity as RootManagedUnitIdentity, ManagedUnitResult as RootManagedUnitResult,
};
use crate::rpg_maker::lua::runtime::{
    TrustedLuaHostCallError, TrustedLuaManagedTranslateHostCalls,
    TrustedLuaManagedTranslationCollection as LuaManagedCollection,
    TrustedLuaManagedTranslationContent as LuaManagedContent, TrustedLuaManagedTranslationReader,
    TrustedLuaManagedTranslationReport, TrustedLuaManagedTranslationResult,
    TrustedLuaManagedTranslationResultStatus, TrustedLuaManagedTranslationShape as LuaManagedShape,
    TrustedLuaManagedTranslationUnit as LuaManagedUnit, TrustedLuaManagedTranslationUnitStatus,
    TrustedLuaPreparedTranslation, TrustedLuaTranslationSemantics,
};
use crate::rpg_maker::managed_translation::{
    ManagedTranslationCheckpoint, ManagedTranslationCheckpointError,
    ManagedTranslationCheckpointOutcome, ManagedTranslationCollection, ManagedTranslationContent,
    ManagedTranslationReplacement, ManagedTranslationRepository, ManagedTranslationShape,
    ManagedTranslationSnapshot, ManagedTranslationUnit,
};
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::text::TextGroupKind;
#[cfg(test)]
use crate::translation_protocol::TranslationResponseEnvelope;

use super::lua::ManagedLuaTranslationFactory;
use super::profile::{
    RpgMakerSystemPrompt, RpgMakerTranslationPlanningConfiguration,
    RpgMakerTranslationRequestConfiguration,
};
use super::standard::{
    StandardTranslationLog, StandardTranslationLogEvent, StandardTranslationLogTaskOutcome,
    StandardTranslationTaskIndex, TranslationProtocolDiagnostic,
};
use super::task_record::{
    ManagedTranslationTaskCheckpointState, ManagedTranslationTaskRecordDocument,
    ManagedTranslationTaskRecordFinalState, ManagedTranslationTaskUnitIdentity,
    ManagedTranslationTaskUnitResult, ManagedTranslationTaskUnitTarget,
    RunWideTranslationTaskIndex, TranslationAssistantEntry, TranslationTaskExecutionEvidence,
    TranslationTaskRecordSink, TranslationTaskResponseRecord,
};
use crate::rpg_maker::write_back::lua::ManagedWriteBackTranslationReaderFactory;

/// 由组合根构造、可为每次 Lua Translate 绑定项目与语义快照的托管服务。
#[derive(Clone)]
pub(crate) struct ManagedTranslationService<L, D, C, S, K> {
    llm: L,
    delay: D,
    cpu: C,
    repository: S,
    planning: RpgMakerTranslationPlanningConfiguration,
    request: RpgMakerTranslationRequestConfiguration,
    managed_system_prompt: RpgMakerSystemPrompt,
    task_records: K,
    task_log: Option<Arc<dyn StandardTranslationLog>>,
    cancellation: CooperativeCancellation,
}

impl<L, D, C, S, K> ManagedTranslationService<L, D, C, S, K> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        llm: L,
        delay: D,
        cpu: C,
        repository: S,
        planning: RpgMakerTranslationPlanningConfiguration,
        request: RpgMakerTranslationRequestConfiguration,
        managed_system_prompt: RpgMakerSystemPrompt,
        task_records: K,
        cancellation: CooperativeCancellation,
    ) -> Self {
        Self {
            llm,
            delay,
            cpu,
            repository,
            planning,
            request,
            managed_system_prompt,
            task_records,
            task_log: None,
            cancellation,
        }
    }

    pub(crate) fn with_task_log(mut self, log: Arc<dyn StandardTranslationLog>) -> Self {
        self.task_log = Some(log);
        self
    }
}

impl<L, D, C, S, K> ManagedLuaTranslationFactory<L::Client>
    for ManagedTranslationService<L, D, C, S, K>
where
    L: LlmRequestExecutor + Clone + 'static,
    L::Client: LlmClientConcurrency,
    L::Error: LlmRequestDiagnosticSource,
    D: AsyncDelay + Clone + 'static,
    C: CpuTaskExecutor + Clone + 'static,
    S: ManagedTranslationRepository + Clone + 'static,
    S::DriverError: SafeDiagnosticSource,
    S::Error: SafeDiagnosticSource,
    K: TranslationTaskRecordSink + Clone + 'static,
{
    fn bind(
        &self,
        project: &OpenedProject,
        llm_client: Arc<L::Client>,
        semantics: Arc<dyn TrustedLuaTranslationSemantics>,
        standard_task_count: usize,
    ) -> Arc<dyn TrustedLuaManagedTranslateHostCalls> {
        Arc::new(BoundManagedTranslationHost {
            llm: self.llm.clone(),
            delay: self.delay.clone(),
            cpu: self.cpu.clone(),
            repository: self.repository.clone(),
            planning: self.planning.clone(),
            request: self.request.clone(),
            managed_system_prompt: self.managed_system_prompt.clone(),
            task_records: self.task_records.clone(),
            task_log: self.task_log.clone(),
            cancellation: self.cancellation.clone(),
            project: project.clone(),
            llm_client,
            semantics,
            standard_task_count,
            opened: Arc::new(Mutex::new(None)),
        })
    }
}

struct BoundManagedTranslationHost<L, D, C, S, K>
where
    L: LlmRequestExecutor,
    S: ManagedTranslationRepository,
{
    llm: L,
    delay: D,
    cpu: C,
    repository: S,
    planning: RpgMakerTranslationPlanningConfiguration,
    request: RpgMakerTranslationRequestConfiguration,
    managed_system_prompt: RpgMakerSystemPrompt,
    task_records: K,
    task_log: Option<Arc<dyn StandardTranslationLog>>,
    cancellation: CooperativeCancellation,
    project: OpenedProject,
    llm_client: Arc<L::Client>,
    semantics: Arc<dyn TrustedLuaTranslationSemantics>,
    standard_task_count: usize,
    opened: Arc<Mutex<Option<Vec<LuaManagedCollection>>>>,
}

impl<L, D, C, S, K> TrustedLuaManagedTranslateHostCalls
    for BoundManagedTranslationHost<L, D, C, S, K>
where
    L: LlmRequestExecutor + Clone + 'static,
    L::Client: LlmClientConcurrency,
    L::Error: LlmRequestDiagnosticSource,
    D: AsyncDelay + Clone + 'static,
    C: CpuTaskExecutor + Clone + 'static,
    S: ManagedTranslationRepository + Clone + 'static,
    S::DriverError: SafeDiagnosticSource,
    S::Error: SafeDiagnosticSource,
    K: TranslationTaskRecordSink + Clone + 'static,
{
    fn translate(
        &self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<TrustedLuaManagedTranslationReport, TrustedLuaHostCallError>,
                > + Send
                + 'static,
        >,
    > {
        let service = BoundManagedTranslationExecution {
            llm: self.llm.clone(),
            delay: self.delay.clone(),
            cpu: self.cpu.clone(),
            repository: self.repository.clone(),
            planning: self.planning.clone(),
            request: self.request.clone(),
            managed_system_prompt: self.managed_system_prompt.clone(),
            task_records: self.task_records.clone(),
            task_log: self.task_log.clone(),
            cancellation: self.cancellation.clone(),
            project: self.project.clone(),
            llm_client: Arc::clone(&self.llm_client),
            semantics: Arc::clone(&self.semantics),
            standard_task_count: self.standard_task_count,
        };
        let opened = Arc::clone(&self.opened);
        Box::pin(async move {
            let (report, collections) = service.run().await?;
            *opened
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(collections);
            Ok(report)
        })
    }

    fn open(
        &self,
        name: String,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<Option<LuaManagedCollection>, TrustedLuaHostCallError>,
                > + Send
                + 'static,
        >,
    > {
        let collection = self
            .opened
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .and_then(|collections| {
                collections
                    .iter()
                    .find(|collection| collection.name() == name)
                    .cloned()
            });
        Box::pin(async move { Ok(collection) })
    }
}

#[derive(Clone)]
struct BoundManagedTranslationExecution<L, D, C, S, K>
where
    L: LlmRequestExecutor,
    S: ManagedTranslationRepository,
{
    llm: L,
    delay: D,
    cpu: C,
    repository: S,
    planning: RpgMakerTranslationPlanningConfiguration,
    request: RpgMakerTranslationRequestConfiguration,
    managed_system_prompt: RpgMakerSystemPrompt,
    task_records: K,
    task_log: Option<Arc<dyn StandardTranslationLog>>,
    cancellation: CooperativeCancellation,
    project: OpenedProject,
    llm_client: Arc<L::Client>,
    semantics: Arc<dyn TrustedLuaTranslationSemantics>,
    standard_task_count: usize,
}

#[derive(Clone)]
struct RpgManagedSemanticsAdapter {
    semantics: Arc<dyn TrustedLuaTranslationSemantics>,
    managed_system_prompt: RpgMakerSystemPrompt,
}

impl RootManagedSemantics for RpgManagedSemanticsAdapter {
    fn engine_semantic_identity(&self) -> &str {
        "rpg_maker"
    }

    fn system_prompt(&self) -> &str {
        self.managed_system_prompt.markdown()
    }

    fn source_language(&self) -> &str {
        self.semantics.source_language()
    }

    fn target_language(&self) -> &str {
        self.semantics.target_language()
    }

    fn prepare_translation(
        &self,
        kind: &str,
        shape: ManagedTranslationShape,
        original: &ManagedTranslationContent,
        semantic_context: &str,
    ) -> Result<Arc<dyn TrustedLuaPreparedTranslation>, TrustedLuaHostCallError> {
        let kind = TextGroupKind::from_storage_name(kind).ok_or_else(|| {
            managed_project_state_error(
                "unknown_kind",
                format!("托管翻译使用未知 kind：{kind}"),
                DiagnosticFailureKind::InvalidValue,
                "rerun_extract_for_managed_translations",
            )
        })?;
        match (shape, original) {
            (ManagedTranslationShape::Lines, ManagedTranslationContent::Array(values)) => self
                .semantics
                .prepare_translation_lines(kind, values.clone(), semantic_context.to_owned()),
            (
                ManagedTranslationShape::Single
                | ManagedTranslationShape::Reflow
                | ManagedTranslationShape::Items,
                ManagedTranslationContent::Scalar(value),
            ) => {
                self.semantics
                    .prepare_translation(kind, value.clone(), semantic_context.to_owned())
            }
            _ => Err(managed_host_error(
                "invalid_semantic_input",
                "RPG Maker 托管翻译语义输入与 shape 不一致",
                managed_internal_diagnostic("rpg_maker_managed_semantics"),
            )),
        }
    }
}

#[derive(Clone)]
struct RpgManagedStoreAdapter<S> {
    repository: S,
    project: OpenedProject,
}

impl<S> RootManagedStore for RpgManagedStoreAdapter<S>
where
    S: ManagedTranslationRepository + Clone + 'static,
    S::DriverError: SafeDiagnosticSource,
    S::Error: SafeDiagnosticSource,
{
    type Snapshot = ManagedTranslationSnapshot;

    async fn load(&self) -> Result<Option<Self::Snapshot>, TrustedLuaHostCallError> {
        self.repository.load(&self.project).await.map_err(|source| {
            managed_repository_source_error::<S>("load_failed", source, &self.project)
        })
    }

    async fn checkpoint(
        &self,
        baseline: &Self::Snapshot,
        replacements: Vec<RootManagedReplacement>,
        mode: RootManagedCheckpointMode,
    ) -> RootManagedStoreCheckpoint<Self::Snapshot> {
        let replacements = replacements
            .into_iter()
            .map(|replacement| {
                let (collection, key, pair) = replacement.into_parts();
                ManagedTranslationReplacement::new(collection, key, pair)
            })
            .collect();
        let checkpoint = match mode {
            RootManagedCheckpointMode::CompleteGuard => {
                ManagedTranslationCheckpoint::guarded(baseline, replacements)
            }
            RootManagedCheckpointMode::Targeted => {
                ManagedTranslationCheckpoint::new(baseline, replacements)
            }
        };
        let checkpoint = match checkpoint {
            Ok(checkpoint) => checkpoint,
            Err(source) => {
                return RootManagedStoreCheckpoint::PreparationFailed(
                    managed_internal_source_error(
                        "checkpoint_invalid",
                        source,
                        "managed_checkpoint_preparation",
                    ),
                );
            }
        };
        let expected = match checkpoint.expected_snapshot(baseline) {
            Ok(expected) => expected,
            Err(source) => {
                return RootManagedStoreCheckpoint::PreparationFailed(
                    managed_internal_source_error(
                        "checkpoint_invalid",
                        source,
                        "managed_checkpoint_projection",
                    ),
                );
            }
        };
        match self.repository.checkpoint(&self.project, checkpoint).await {
            Ok(ManagedTranslationCheckpointOutcome::Applied) => {
                RootManagedStoreCheckpoint::Applied(expected)
            }
            Ok(ManagedTranslationCheckpointOutcome::NotApplied) => {
                RootManagedStoreCheckpoint::NotApplied(managed_checkpoint_not_applied_error(
                    &self.project,
                    "checkpoint_not_applied",
                    "托管翻译 checkpoint 因项目状态变化未应用",
                ))
            }
            Ok(ManagedTranslationCheckpointOutcome::OutcomeUnknown(source)) => {
                RootManagedStoreCheckpoint::OutcomeUnknown(
                    managed_checkpoint_outcome_unknown_error(&self.project, source),
                )
            }
            Err(source) => RootManagedStoreCheckpoint::Failed(managed_checkpoint_source_error(
                &self.project,
                source,
            )),
        }
    }
}

#[derive(Clone)]
struct RpgManagedObserver<K> {
    task_records: K,
    task_log: Option<Arc<dyn StandardTranslationLog>>,
}

impl<K> RootManagedObserver for RpgManagedObserver<K>
where
    K: TranslationTaskRecordSink,
{
    fn recording_enabled(&self) -> bool {
        self.task_records.enabled()
    }

    fn declare_total_tasks(&self, total_tasks: usize) {
        self.task_records.declare_total_tasks(total_tasks);
    }

    fn task_started(&self, run_wide_ordinal: usize, total_tasks: usize) {
        if let Some(log) = &self.task_log {
            log.emit(StandardTranslationLogEvent::TaskStarted {
                task_index: StandardTranslationTaskIndex::new(run_wide_ordinal),
                total_tasks,
            });
        }
    }

    fn task_finished(&self, observation: RootManagedTaskObservation) {
        let RootManagedTaskObservation {
            total_tasks,
            run_wide_ordinal,
            collection,
            messages,
            identities,
            evidence,
            unit_results,
            protocol_diagnostics,
            checkpoint,
            confirmed_committed_units,
            diagnostic,
        } = observation;
        let attempts = NonZeroUsize::new(evidence.attempt_count());
        let retry_exhausted = unit_results
            .iter()
            .any(|result| result.is_rejected_for("request_retry_exhausted"));
        if let Some(log) = &self.task_log {
            log.emit(StandardTranslationLogEvent::TaskFinished {
                task_index: StandardTranslationTaskIndex::new(run_wide_ordinal),
                outcome: root_checkpoint_log_outcome(checkpoint),
                attempts,
                retry_exhausted,
                diagnostic: diagnostic.clone(),
            });
        }
        if !self.task_records.enabled() {
            return;
        }

        let (started_at, task_started, attempt_count, attempts, response) = evidence.into_parts();
        let evidence = TranslationTaskExecutionEvidence::from_execution(
            started_at,
            task_started,
            attempt_count,
            attempts,
            response.map(project_root_response_record),
        );
        let identities = identities
            .into_iter()
            .map(|identity| {
                ManagedTranslationTaskUnitIdentity::new(
                    identity.id(),
                    identity
                        .targets()
                        .iter()
                        .map(|target| {
                            ManagedTranslationTaskUnitTarget::new(target.collection(), target.key())
                        })
                        .collect(),
                )
            })
            .collect();
        let unit_results = unit_results
            .into_iter()
            .map(|result| {
                let (id, accepted, reason, details) = result.into_parts();
                if accepted {
                    ManagedTranslationTaskUnitResult::accepted(id)
                } else {
                    ManagedTranslationTaskUnitResult::rejected(
                        id,
                        reason.as_deref().unwrap_or("unavailable"),
                        details,
                    )
                }
            })
            .collect();
        let protocol_diagnostics = protocol_diagnostics
            .into_iter()
            .map(project_root_protocol_diagnostic)
            .collect();
        self.task_records
            .submit_managed(ManagedTranslationTaskRecordDocument::new(
                total_tasks,
                RunWideTranslationTaskIndex::new(run_wide_ordinal),
                &collection,
                messages,
                identities,
                evidence,
                ManagedTranslationTaskRecordFinalState::new(
                    project_root_checkpoint_state(checkpoint),
                    unit_results,
                    confirmed_committed_units,
                    diagnostic,
                )
                .with_protocol_diagnostics(protocol_diagnostics),
            ));
    }
}

fn project_root_response_record(
    response: RootManagedResponseRecord,
) -> TranslationTaskResponseRecord {
    match response {
        RootManagedResponseRecord::Parsed {
            raw_assistant,
            thinking,
            entries,
        } => TranslationTaskResponseRecord::parsed(
            raw_assistant,
            thinking,
            entries
                .into_iter()
                .map(|entry| {
                    let (id, value, canonical_id, value_error) = entry.into_parts();
                    TranslationAssistantEntry::projected(id, value, canonical_id, value_error)
                })
                .collect(),
        ),
        RootManagedResponseRecord::Invalid {
            raw_assistant,
            error,
        } => TranslationTaskResponseRecord::invalid(raw_assistant, error),
        RootManagedResponseRecord::Unprocessed { raw_assistant } => {
            TranslationTaskResponseRecord::unprocessed(raw_assistant)
        }
    }
}

fn project_root_protocol_diagnostic(
    diagnostic: RootManagedProtocolDiagnostic,
) -> TranslationProtocolDiagnostic {
    match diagnostic {
        RootManagedProtocolDiagnostic::NonStopFinish { reason } => {
            TranslationProtocolDiagnostic::NonStopFinish { reason }
        }
        RootManagedProtocolDiagnostic::InvalidResponse { message } => {
            TranslationProtocolDiagnostic::InvalidResponse { message }
        }
        RootManagedProtocolDiagnostic::InvalidId { item_index } => {
            TranslationProtocolDiagnostic::InvalidId { item_index }
        }
        RootManagedProtocolDiagnostic::UnknownId { item_index, id } => {
            TranslationProtocolDiagnostic::UnknownId { item_index, id }
        }
    }
}

const fn project_root_checkpoint_state(
    state: RootManagedCheckpointState,
) -> ManagedTranslationTaskCheckpointState {
    match state {
        RootManagedCheckpointState::Complete => ManagedTranslationTaskCheckpointState::Complete,
        RootManagedCheckpointState::Partial => ManagedTranslationTaskCheckpointState::Partial,
        RootManagedCheckpointState::Unavailable => {
            ManagedTranslationTaskCheckpointState::Unavailable
        }
        RootManagedCheckpointState::ExecutionFailed => {
            ManagedTranslationTaskCheckpointState::ExecutionFailed
        }
        RootManagedCheckpointState::CommitPreparationFailed => {
            ManagedTranslationTaskCheckpointState::CommitPreparationFailed
        }
        RootManagedCheckpointState::CommitNotApplied => {
            ManagedTranslationTaskCheckpointState::CommitNotApplied
        }
        RootManagedCheckpointState::OutcomeUnknown => {
            ManagedTranslationTaskCheckpointState::OutcomeUnknown
        }
        RootManagedCheckpointState::EarlierFailure => {
            ManagedTranslationTaskCheckpointState::EarlierFailure
        }
        RootManagedCheckpointState::Cancelled => ManagedTranslationTaskCheckpointState::Cancelled,
    }
}

const fn root_checkpoint_log_outcome(
    state: RootManagedCheckpointState,
) -> StandardTranslationLogTaskOutcome {
    match state {
        RootManagedCheckpointState::Complete => StandardTranslationLogTaskOutcome::Complete,
        RootManagedCheckpointState::Partial => StandardTranslationLogTaskOutcome::Partial,
        RootManagedCheckpointState::Unavailable => StandardTranslationLogTaskOutcome::Unavailable,
        RootManagedCheckpointState::ExecutionFailed => {
            StandardTranslationLogTaskOutcome::ExecutionFailed
        }
        RootManagedCheckpointState::CommitPreparationFailed
        | RootManagedCheckpointState::CommitNotApplied
        | RootManagedCheckpointState::OutcomeUnknown => {
            StandardTranslationLogTaskOutcome::CommitFailed
        }
        RootManagedCheckpointState::EarlierFailure | RootManagedCheckpointState::Cancelled => {
            StandardTranslationLogTaskOutcome::NotCommitted
        }
    }
}

impl<L, D, C, S, K> BoundManagedTranslationExecution<L, D, C, S, K>
where
    L: LlmRequestExecutor + Clone + 'static,
    L::Client: LlmClientConcurrency,
    L::Error: LlmRequestDiagnosticSource,
    D: AsyncDelay + Clone + 'static,
    C: CpuTaskExecutor + Clone + 'static,
    S: ManagedTranslationRepository + Clone + 'static,
    S::DriverError: SafeDiagnosticSource,
    S::Error: SafeDiagnosticSource,
    K: TranslationTaskRecordSink + Clone + 'static,
{
    async fn run(
        self,
    ) -> Result<
        (
            TrustedLuaManagedTranslationReport,
            Vec<LuaManagedCollection>,
        ),
        TrustedLuaHostCallError,
    > {
        let configuration = RootManagedKernelConfiguration::new(
            self.planning.target_user_message_characters().get(),
            self.request.network_retry_delays().to_vec(),
            self.request.max_network_retry_after(),
            self.managed_system_prompt.response_envelope(),
            self.standard_task_count,
        );
        let store = RpgManagedStoreAdapter {
            repository: self.repository,
            project: self.project,
        };
        let observer = RpgManagedObserver {
            task_records: self.task_records,
            task_log: self.task_log,
        };
        let semantics: Arc<dyn RootManagedSemantics> = Arc::new(RpgManagedSemanticsAdapter {
            semantics: self.semantics,
            managed_system_prompt: self.managed_system_prompt,
        });
        let kernel = RootManagedKernel::new(
            self.llm,
            self.delay,
            self.cpu,
            store,
            observer,
            configuration,
            self.cancellation,
            self.llm_client,
            semantics,
        );
        let Some(output) = kernel.run().await? else {
            return Ok((
                TrustedLuaManagedTranslationReport::new(Vec::new()),
                Vec::new(),
            ));
        };
        let (snapshot, results) = output.into_parts();
        Ok(build_lua_results(&snapshot, &results))
    }
}

fn build_lua_results(
    snapshot: &ManagedTranslationSnapshot,
    results: &HashMap<RootManagedUnitIdentity, RootManagedUnitResult>,
) -> (
    TrustedLuaManagedTranslationReport,
    Vec<LuaManagedCollection>,
) {
    let mut report_units = Vec::new();
    let mut lua_collections = Vec::with_capacity(snapshot.collections().len());
    for collection in snapshot.collections() {
        let mut lua_units = Vec::with_capacity(collection.units().len());
        for unit in collection.units() {
            let identity = RootManagedUnitIdentity::new(collection.name(), unit.key());
            let result = results
                .get(&identity)
                .expect("规划必须为快照中的每个托管 unit 建立结果");
            let persisted_translation = unit.translation().map(|pair| lua_content(pair.content()));
            let status = match result.status() {
                TrustedLuaManagedTranslationResultStatus::Current
                | TrustedLuaManagedTranslationResultStatus::Translated => {
                    TrustedLuaManagedTranslationUnitStatus::Current
                }
                TrustedLuaManagedTranslationResultStatus::NotApplicable => {
                    TrustedLuaManagedTranslationUnitStatus::NotApplicable
                }
                TrustedLuaManagedTranslationResultStatus::Unavailable => {
                    TrustedLuaManagedTranslationUnitStatus::Unavailable
                }
            };
            report_units.push(TrustedLuaManagedTranslationResult::new(
                collection.name().to_owned(),
                unit.key().to_owned(),
                result.status(),
                result.translation().map(lua_content),
                result.reason().map(str::to_owned),
                result
                    .details()
                    .map(|details| serde_json::to_string(details).expect("JSON Value 必须可编码")),
            ));
            lua_units.push(lua_unit(unit, persisted_translation, status));
        }
        lua_collections.push(LuaManagedCollection::new(
            collection.name().to_owned(),
            collection.instruction().to_owned(),
            lua_units,
        ));
    }
    (
        TrustedLuaManagedTranslationReport::new(report_units),
        lua_collections,
    )
}

fn lua_collection_from_persisted(
    collection: &ManagedTranslationCollection,
) -> LuaManagedCollection {
    LuaManagedCollection::new(
        collection.name().to_owned(),
        collection.instruction().to_owned(),
        collection
            .units()
            .iter()
            .map(|unit| {
                let translation = unit.translation().map(|pair| lua_content(pair.content()));
                let status = if translation.is_some() {
                    TrustedLuaManagedTranslationUnitStatus::Current
                } else {
                    TrustedLuaManagedTranslationUnitStatus::Missing
                };
                lua_unit(unit, translation, status)
            })
            .collect(),
    )
}

fn lua_unit(
    unit: &ManagedTranslationUnit,
    translation: Option<LuaManagedContent>,
    status: TrustedLuaManagedTranslationUnitStatus,
) -> LuaManagedUnit {
    LuaManagedUnit::new(
        unit.key().to_owned(),
        lua_kind(unit.kind()).to_owned(),
        lua_shape(unit.shape()),
        lua_content(unit.original()),
        unit.context().to_owned(),
        unit.metadata()
            .map(|metadata| metadata.canonical_json().to_owned()),
        translation,
        status,
    )
}

fn lua_kind(kind: &str) -> &str {
    match kind {
        "event_dialogue" => "dialogue",
        "event_choices" => "choices",
        "event_scrolling_text" => "scrolling_text",
        _ => kind,
    }
}

const fn lua_shape(shape: ManagedTranslationShape) -> LuaManagedShape {
    match shape {
        ManagedTranslationShape::Single => LuaManagedShape::Single,
        ManagedTranslationShape::Reflow => LuaManagedShape::Reflow,
        ManagedTranslationShape::Lines => LuaManagedShape::Lines,
        ManagedTranslationShape::Items => LuaManagedShape::Items,
    }
}

fn lua_content(content: &ManagedTranslationContent) -> LuaManagedContent {
    match content {
        ManagedTranslationContent::Scalar(value) => LuaManagedContent::scalar(value),
        ManagedTranslationContent::Array(values) => LuaManagedContent::array(values.clone()),
    }
}

/// WriteBack 为一次 Lua VM 冻结最后已提交的托管快照。
#[derive(Clone)]
pub(crate) struct ManagedTranslationReadService<S> {
    repository: S,
}

impl<S> ManagedTranslationReadService<S> {
    pub(crate) fn new(repository: S) -> Self {
        Self { repository }
    }
}

impl<S> ManagedWriteBackTranslationReaderFactory for ManagedTranslationReadService<S>
where
    S: ManagedTranslationRepository + Clone + 'static,
    S::Error: SafeDiagnosticSource,
{
    fn bind(&self, project: &OpenedProject) -> Arc<dyn TrustedLuaManagedTranslationReader> {
        Arc::new(BoundManagedTranslationReader {
            repository: self.repository.clone(),
            project: project.clone(),
            snapshot: Arc::new(tokio::sync::OnceCell::new()),
        })
    }
}

struct BoundManagedTranslationReader<S> {
    repository: S,
    project: OpenedProject,
    snapshot: Arc<tokio::sync::OnceCell<Option<ManagedTranslationSnapshot>>>,
}

impl<S> TrustedLuaManagedTranslationReader for BoundManagedTranslationReader<S>
where
    S: ManagedTranslationRepository + Clone + 'static,
    S::Error: SafeDiagnosticSource,
{
    fn open(
        &self,
        name: String,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<Option<LuaManagedCollection>, TrustedLuaHostCallError>,
                > + Send
                + 'static,
        >,
    > {
        let repository = self.repository.clone();
        let project = self.project.clone();
        let snapshot = Arc::clone(&self.snapshot);
        Box::pin(async move {
            let snapshot = snapshot
                .get_or_try_init(|| async move {
                    repository.load(&project).await.map_err(|source| {
                        managed_open_repository_source_error::<S>("load_failed", source, &project)
                    })
                })
                .await?;
            Ok(snapshot.as_ref().and_then(|snapshot| {
                snapshot
                    .collection(&name)
                    .map(lua_collection_from_persisted)
            }))
        })
    }
}

fn managed_host_error(
    kind: &'static str,
    message: impl Into<String>,
    diagnostic: SafeDiagnostic,
) -> TrustedLuaHostCallError {
    TrustedLuaHostCallError::new("translations", kind, message, None, None)
        .with_operation("translations.translate")
        .with_safe_diagnostic(diagnostic)
}

fn managed_host_source_error<E>(
    kind: &'static str,
    message: impl Into<String>,
    source: E,
    diagnostic: SafeDiagnostic,
) -> TrustedLuaHostCallError
where
    E: Error + Send + Sync + 'static,
{
    TrustedLuaHostCallError::new("translations", kind, message, None, Some(Arc::new(source)))
        .with_operation("translations.translate")
        .with_safe_diagnostic(diagnostic)
}

fn managed_internal_diagnostic(component: &'static str) -> SafeDiagnostic {
    SafeDiagnostic::new(
        DiagnosticCode::InternalOperation,
        DiagnosticStage::Translate,
        DiagnosticSubject::component(component),
        DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
        DiagnosticImpact::ProgressPreserved,
        DiagnosticAction::ReportBug,
    )
    .with_recovery(RecoveryFact::component(
        "committed_managed_translation_prefix_preserved",
    ))
}

fn managed_internal_source_error<E>(
    kind: &'static str,
    source: E,
    component: &'static str,
) -> TrustedLuaHostCallError
where
    E: Error + Send + Sync + 'static,
{
    managed_host_source_error(
        kind,
        "托管翻译内部状态不一致",
        source,
        managed_internal_diagnostic(component),
    )
}

fn managed_project_state_error(
    kind: &'static str,
    message: impl Into<String>,
    failure: DiagnosticFailureKind,
    recovery: &'static str,
) -> TrustedLuaHostCallError {
    managed_host_error(
        kind,
        message,
        SafeDiagnostic::new(
            DiagnosticCode::ProjectState,
            DiagnosticStage::Translate,
            DiagnosticSubject::component("managed_translations"),
            DiagnosticReason::failure(failure),
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::CheckProjectState,
        )
        .with_recovery(RecoveryFact::component(recovery)),
    )
}

fn managed_checkpoint_not_applied_error(
    project: &OpenedProject,
    kind: &'static str,
    message: impl Into<String>,
) -> TrustedLuaHostCallError {
    managed_host_error(
        kind,
        message,
        SafeDiagnostic::new(
            DiagnosticCode::ProjectState,
            DiagnosticStage::Translate,
            DiagnosticSubject::path(project.database_path()),
            DiagnosticReason::failure(DiagnosticFailureKind::StateMismatch),
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::CheckProjectState,
        )
        .with_recovery(RecoveryFact::transaction("not_applied"))
        .with_recovery(RecoveryFact::component(
            "reload_managed_translation_snapshot",
        )),
    )
}

fn managed_checkpoint_outcome_unknown_error<E>(
    project: &OpenedProject,
    source: E,
) -> TrustedLuaHostCallError
where
    E: Error + Send + Sync + SafeDiagnosticSource + 'static,
{
    let mut diagnostic = source.safe_diagnostic_source(
        DiagnosticStage::Translate,
        DiagnosticImpact::OutcomeUnknown,
        DiagnosticAction::PreserveRecoveryArtifacts,
    );
    diagnostic.stage = DiagnosticStage::Translate;
    diagnostic.subject = DiagnosticSubject::path(project.database_path());
    diagnostic.impact = DiagnosticImpact::OutcomeUnknown;
    diagnostic.action = DiagnosticAction::PreserveRecoveryArtifacts;
    diagnostic = diagnostic
        .with_recovery(RecoveryFact::path(project.database_path()))
        .with_recovery(RecoveryFact::transaction("outcome_unknown"));
    managed_host_source_error(
        "checkpoint_outcome_unknown",
        "托管翻译 checkpoint 的提交终态未知",
        source,
        diagnostic,
    )
}

fn managed_checkpoint_source_error<E>(
    project: &OpenedProject,
    source: ManagedTranslationCheckpointError<E>,
) -> TrustedLuaHostCallError
where
    E: Error + Send + Sync + SafeDiagnosticSource + 'static,
{
    let diagnostic = source.safe_diagnostic_source(
        DiagnosticStage::Translate,
        DiagnosticImpact::ProgressPreserved,
        DiagnosticAction::Retry,
    );
    managed_host_source_error(
        "checkpoint_failed",
        format!(
            "托管翻译 checkpoint 未提交；项目数据库：{}",
            project.database_path().display()
        ),
        source,
        diagnostic,
    )
}

fn managed_repository_source_error<S>(
    kind: &'static str,
    source: S::Error,
    project: &OpenedProject,
) -> TrustedLuaHostCallError
where
    S: ManagedTranslationRepository,
    S::Error: SafeDiagnosticSource,
{
    if S::is_source_stale(&source) {
        let diagnostic = SafeDiagnostic::new(
            DiagnosticCode::ProjectState,
            DiagnosticStage::Translate,
            DiagnosticSubject::path(project.database_path()),
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::StateMismatch,
                "managed_translation_source_stale",
            ),
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::CheckProjectState,
        )
        .with_recovery(RecoveryFact::component(
            "rerun_extract_for_managed_translations",
        ));
        return TrustedLuaHostCallError::new(
            "translations",
            "stale_snapshot",
            "托管翻译来源已过期，必须重新 Extract",
            None,
            Some(Arc::new(source)),
        )
        .with_operation("translations.translate")
        .with_safe_diagnostic(diagnostic);
    }
    let mut diagnostic = source.safe_diagnostic_source(
        DiagnosticStage::Translate,
        DiagnosticImpact::ProgressPreserved,
        DiagnosticAction::Retry,
    );
    diagnostic.stage = DiagnosticStage::Translate;
    diagnostic.impact = DiagnosticImpact::ProgressPreserved;
    managed_host_source_error(kind, "读取托管翻译快照失败", source, diagnostic)
}

fn managed_open_repository_source_error<S>(
    kind: &'static str,
    source: S::Error,
    project: &OpenedProject,
) -> TrustedLuaHostCallError
where
    S: ManagedTranslationRepository,
    S::Error: SafeDiagnosticSource,
{
    if S::is_source_stale(&source) {
        let diagnostic = SafeDiagnostic::new(
            DiagnosticCode::ProjectState,
            DiagnosticStage::WriteBack,
            DiagnosticSubject::path(project.database_path()),
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::StateMismatch,
                "managed_translation_source_stale",
            ),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        )
        .with_recovery(RecoveryFact::component(
            "rerun_extract_for_managed_translations",
        ));
        return TrustedLuaHostCallError::new(
            "translations",
            "stale_snapshot",
            "托管翻译来源已过期，必须重新 Extract",
            None,
            Some(Arc::new(source)),
        )
        .with_operation("translations.open")
        .with_safe_diagnostic(diagnostic);
    }
    let mut diagnostic = source.safe_diagnostic_source(
        DiagnosticStage::WriteBack,
        DiagnosticImpact::Unchanged,
        DiagnosticAction::CheckProjectState,
    );
    diagnostic.stage = DiagnosticStage::WriteBack;
    diagnostic.impact = DiagnosticImpact::Unchanged;
    TrustedLuaHostCallError::new(
        "translations",
        kind,
        "读取托管翻译快照失败",
        None,
        Some(Arc::new(source)),
    )
    .with_operation("translations.open")
    .with_safe_diagnostic(diagnostic)
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fmt;
    use std::num::NonZeroUsize;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use crate::execution::cpu::CpuTaskExecutionError;
    use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
    use crate::llm::{ChatMessage, LlmResponse};
    use crate::managed_translation::{
        TrustedLuaPreparedTranslationAcceptance, TrustedLuaPreparedTranslationStatus,
        TrustedLuaTranslationTerm,
    };
    use crate::rpg_maker::ProjectName;
    use crate::rpg_maker::managed_translation::ManagedTranslationCheckpointError;
    use crate::rpg_maker::project::test_layout_profile;
    use crate::rpg_maker::translate::task_record::NoOpTranslationTaskRecordSink;

    use super::*;

    #[derive(Clone)]
    struct FakeSemantics;

    impl TrustedLuaTranslationSemantics for FakeSemantics {
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
            Ok(Arc::new(FakePrepared::new(
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
            Ok(Arc::new(FakePrepared::new(
                kind,
                original.join("\n"),
                semantic_context,
            )))
        }
    }

    struct FakePrepared {
        model_text: String,
        fingerprint: Sha256Fingerprint,
        terms: Vec<TrustedLuaTranslationTerm>,
    }

    impl FakePrepared {
        fn new(kind: TextGroupKind, model_text: String, semantic_context: String) -> Self {
            let mut hasher = Sha256FramedHasher::new(b"att.test.managed.prepared");
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

    impl TrustedLuaPreparedTranslation for FakePrepared {
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
        let mut hasher = Sha256FramedHasher::new(b"att.test.managed.accepted");
        hasher.frame(1, value.as_bytes());
        hasher.finish()
    }

    fn scalar_unit(key: &str, context: &str) -> ManagedTranslationUnit {
        unit(
            key,
            ManagedTranslationShape::Single,
            ManagedTranslationContent::scalar("原文"),
            context,
        )
    }

    fn unit(
        key: &str,
        shape: ManagedTranslationShape,
        original: ManagedTranslationContent,
        context: &str,
    ) -> ManagedTranslationUnit {
        ManagedTranslationUnit::new(key, "plugin_parameter", shape, original, context, None)
            .expect("测试托管 unit 应合法")
    }

    fn collection(
        name: &str,
        instruction: &str,
        units: Vec<ManagedTranslationUnit>,
    ) -> ManagedTranslationCollection {
        ManagedTranslationCollection::new(name, instruction, units).expect("测试 collection 应合法")
    }

    #[derive(Clone, Copy, Debug)]
    struct TestSqliteError;

    impl fmt::Display for TestSqliteError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("test sqlite error")
        }
    }

    impl Error for TestSqliteError {}

    impl SafeDiagnosticSource for TestSqliteError {
        fn safe_diagnostic_source(
            &self,
            stage: DiagnosticStage,
            impact: DiagnosticImpact,
            fallback_action: DiagnosticAction,
        ) -> SafeDiagnostic {
            SafeDiagnostic::new(
                DiagnosticCode::SqliteOperation,
                stage,
                DiagnosticSubject::component("test_sqlite"),
                DiagnosticReason::failure(DiagnosticFailureKind::TransactionRolledBack),
                impact,
                fallback_action,
            )
        }
    }

    impl LlmRequestDiagnosticSource for TestSqliteError {
        fn request_diagnostic(
            &self,
            _retry_after: Option<Duration>,
            _impact: DiagnosticImpact,
        ) -> SafeDiagnostic {
            panic!("零请求快照门禁不得生成 LLM 诊断")
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct StaleSourceError;

    impl fmt::Display for StaleSourceError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("managed source stale")
        }
    }

    impl Error for StaleSourceError {}

    impl SafeDiagnosticSource for StaleSourceError {
        fn safe_diagnostic_source(
            &self,
            stage: DiagnosticStage,
            impact: DiagnosticImpact,
            fallback_action: DiagnosticAction,
        ) -> SafeDiagnostic {
            SafeDiagnostic::new(
                DiagnosticCode::ProjectState,
                stage,
                DiagnosticSubject::component("stale_managed_source"),
                DiagnosticReason::failure(DiagnosticFailureKind::StateMismatch),
                impact,
                fallback_action,
            )
        }
    }

    #[derive(Clone, Copy)]
    struct StaleRepository;

    impl ManagedTranslationRepository for StaleRepository {
        type DriverError = TestSqliteError;
        type Error = StaleSourceError;

        async fn load(
            &self,
            _project: &OpenedProject,
        ) -> Result<Option<ManagedTranslationSnapshot>, Self::Error> {
            Err(StaleSourceError)
        }

        fn is_source_stale(_error: &Self::Error) -> bool {
            true
        }

        async fn checkpoint(
            &self,
            _project: &OpenedProject,
            _checkpoint: ManagedTranslationCheckpoint,
        ) -> Result<
            ManagedTranslationCheckpointOutcome<Self::DriverError>,
            ManagedTranslationCheckpointError<Self::DriverError>,
        > {
            panic!("stale 快照必须在 checkpoint 前失败")
        }
    }

    #[derive(Clone)]
    struct SnapshotChangingRepository {
        first: ManagedTranslationSnapshot,
        second: ManagedTranslationSnapshot,
        loads: Arc<AtomicUsize>,
        checkpoints: Arc<AtomicUsize>,
    }

    impl ManagedTranslationRepository for SnapshotChangingRepository {
        type DriverError = TestSqliteError;
        type Error = TestSqliteError;

        async fn load(
            &self,
            _project: &OpenedProject,
        ) -> Result<Option<ManagedTranslationSnapshot>, Self::Error> {
            let load = self.loads.fetch_add(1, Ordering::SeqCst);
            Ok(Some(if load == 0 {
                self.first.clone()
            } else {
                self.second.clone()
            }))
        }

        fn is_source_stale(_error: &Self::Error) -> bool {
            false
        }

        async fn checkpoint(
            &self,
            _project: &OpenedProject,
            _checkpoint: ManagedTranslationCheckpoint,
        ) -> Result<
            ManagedTranslationCheckpointOutcome<Self::DriverError>,
            ManagedTranslationCheckpointError<Self::DriverError>,
        > {
            self.checkpoints.fetch_add(1, Ordering::SeqCst);
            Ok(ManagedTranslationCheckpointOutcome::Applied)
        }
    }

    #[derive(Clone)]
    struct CountingLlm {
        calls: Arc<AtomicUsize>,
    }

    impl LlmRequestExecutor for CountingLlm {
        type Client = SnapshotGateClient;
        type Error = TestSqliteError;

        async fn request<'a>(
            &'a self,
            _client: &'a Self::Client,
            _messages: &'a [ChatMessage],
        ) -> Result<LlmResponse, LlmRequestError<Self::Error>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(LlmRequestError::Fatal(TestSqliteError))
        }
    }

    struct SnapshotGateClient;

    impl LlmClientConcurrency for SnapshotGateClient {
        fn max_concurrent_requests(&self) -> NonZeroUsize {
            NonZeroUsize::MIN
        }
    }

    #[derive(Clone, Copy)]
    struct ImmediateDelay;

    impl AsyncDelay for ImmediateDelay {
        async fn wait(&self, _duration: Duration) {}
    }

    #[derive(Clone, Copy)]
    struct InlineCpu;

    impl CpuTaskExecutor for InlineCpu {
        type Error = TestSqliteError;

        async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            Ok(task())
        }
    }

    #[derive(Default)]
    struct CapturingTranslationLog {
        events: Mutex<Vec<StandardTranslationLogEvent>>,
    }

    impl StandardTranslationLog for CapturingTranslationLog {
        fn emit(&self, event: StandardTranslationLogEvent) {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
        }
    }

    fn project() -> OpenedProject {
        OpenedProject::new(
            "managed-tests"
                .parse::<ProjectName>()
                .expect("项目名应合法"),
            PathBuf::from("C:/projects/managed-tests"),
            PathBuf::from("C:/projects/managed-tests/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            test_layout_profile(),
        )
    }

    fn test_managed_system_prompt(project: &OpenedProject) -> RpgMakerSystemPrompt {
        RpgMakerSystemPrompt::new(
            project.language_pair().clone(),
            "managed system".to_owned(),
            TranslationResponseEnvelope::JsonOnly,
        )
        .expect("测试 Managed Prompt 应合法")
    }

    fn assert_failure(
        diagnostic: &SafeDiagnostic,
        expected_code: DiagnosticCode,
        expected_failure: DiagnosticFailureKind,
        expected_impact: DiagnosticImpact,
        expected_action: DiagnosticAction,
    ) {
        assert_eq!(diagnostic.code, expected_code);
        assert_eq!(diagnostic.stage, DiagnosticStage::Translate);
        assert_eq!(diagnostic.impact, expected_impact);
        assert_eq!(diagnostic.action, expected_action);
        assert!(matches!(
            diagnostic.reason,
            DiagnosticReason::Failure {
                failure
            } if failure == expected_failure
        ));
    }

    #[test]
    fn commit_preparation_failure_is_an_internal_progress_preserving_terminal() {
        let error = managed_internal_source_error(
            "checkpoint_invalid",
            TestSqliteError,
            "managed_checkpoint_preparation",
        );
        let diagnostic = error
            .safe_diagnostic()
            .expect("checkpoint 构造失败必须有安全诊断");
        assert_failure(
            diagnostic,
            DiagnosticCode::InternalOperation,
            DiagnosticFailureKind::InternalInvariant,
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::ReportBug,
        );
        assert!(diagnostic.recovery.contains(&RecoveryFact::component(
            "committed_managed_translation_prefix_preserved"
        )));
    }

    #[test]
    fn repository_load_and_open_failures_keep_typed_driver_diagnostics() {
        let project = project();
        let translate = managed_repository_source_error::<SnapshotChangingRepository>(
            "load_failed",
            TestSqliteError,
            &project,
        );
        let diagnostic = translate
            .safe_diagnostic()
            .expect("Translate 快照读取失败必须有安全诊断");
        assert_failure(
            diagnostic,
            DiagnosticCode::SqliteOperation,
            DiagnosticFailureKind::TransactionRolledBack,
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::Retry,
        );

        let open = managed_open_repository_source_error::<SnapshotChangingRepository>(
            "load_failed",
            TestSqliteError,
            &project,
        );
        assert_eq!(open.operation(), Some("translations.open"));
        let diagnostic = open
            .safe_diagnostic()
            .expect("WriteBack open 读取失败必须有安全诊断");
        assert_eq!(diagnostic.code, DiagnosticCode::SqliteOperation);
        assert_eq!(diagnostic.stage, DiagnosticStage::WriteBack);
        assert_eq!(diagnostic.impact, DiagnosticImpact::Unchanged);
        assert_eq!(diagnostic.action, DiagnosticAction::CheckProjectState);
    }

    #[test]
    fn checkpoint_terminal_failures_have_distinct_commit_diagnostics() {
        let project = project();

        let not_applied = managed_checkpoint_not_applied_error(
            &project,
            "checkpoint_not_applied",
            "checkpoint 未应用",
        );
        let diagnostic = not_applied
            .safe_diagnostic()
            .expect("NotApplied 必须有安全诊断");
        assert_failure(
            diagnostic,
            DiagnosticCode::ProjectState,
            DiagnosticFailureKind::StateMismatch,
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::CheckProjectState,
        );
        assert!(
            diagnostic
                .recovery
                .contains(&RecoveryFact::transaction("not_applied"))
        );

        let outcome_unknown = managed_checkpoint_outcome_unknown_error(&project, TestSqliteError);
        let diagnostic = outcome_unknown
            .safe_diagnostic()
            .expect("OutcomeUnknown 必须有安全诊断");
        assert_failure(
            diagnostic,
            DiagnosticCode::SqliteOperation,
            DiagnosticFailureKind::TransactionRolledBack,
            DiagnosticImpact::OutcomeUnknown,
            DiagnosticAction::PreserveRecoveryArtifacts,
        );
        assert!(
            diagnostic
                .recovery
                .contains(&RecoveryFact::path(project.database_path()))
        );
        assert!(
            diagnostic
                .recovery
                .contains(&RecoveryFact::transaction("outcome_unknown"))
        );

        let database_not_found = managed_checkpoint_source_error(
            &project,
            ManagedTranslationCheckpointError::<TestSqliteError>::DatabaseNotFound {
                database_path: project.database_path().to_path_buf(),
            },
        );
        let diagnostic = database_not_found
            .safe_diagnostic()
            .expect("DatabaseNotFound 必须有安全诊断");
        assert_failure(
            diagnostic,
            DiagnosticCode::ProjectUnavailable,
            DiagnosticFailureKind::NotFound,
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::CheckProjectState,
        );
        assert!(
            diagnostic
                .recovery
                .contains(&RecoveryFact::component("project_database"))
        );

        let not_committed = managed_checkpoint_source_error(
            &project,
            ManagedTranslationCheckpointError::NotCommitted {
                database_path: project.database_path().to_path_buf(),
                source: TestSqliteError,
            },
        );
        let diagnostic = not_committed
            .safe_diagnostic()
            .expect("NotCommitted 必须有安全诊断");
        assert_failure(
            diagnostic,
            DiagnosticCode::SqliteOperation,
            DiagnosticFailureKind::TransactionRolledBack,
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::Retry,
        );
        assert!(
            diagnostic
                .recovery
                .contains(&RecoveryFact::transaction("rolled_back"))
        );
    }

    #[test]
    fn disabled_task_records_do_not_suppress_task_log_diagnostic() {
        let project = project();
        let task_log = Arc::new(CapturingTranslationLog::default());
        let observer = RpgManagedObserver {
            task_records: NoOpTranslationTaskRecordSink,
            task_log: Some(task_log.clone()),
        };
        let error = managed_checkpoint_not_applied_error(
            &project,
            "checkpoint_not_applied",
            "checkpoint 未应用",
        );
        let diagnostic = error
            .safe_diagnostic()
            .cloned()
            .expect("提交失败必须有安全诊断");

        observer.task_finished(RootManagedTaskObservation {
            total_tasks: 1,
            run_wide_ordinal: 0,
            collection: "quests".to_owned(),
            messages: Vec::new(),
            identities: Vec::new(),
            evidence: crate::managed_translation::ManagedTranslationTaskEvidence::empty_for_test(),
            unit_results: Vec::new(),
            protocol_diagnostics: Vec::new(),
            checkpoint: RootManagedCheckpointState::CommitNotApplied,
            confirmed_committed_units: Some(0),
            diagnostic: Some(diagnostic.clone()),
        });

        let events = task_log
            .events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(events.len(), 1);
        let StandardTranslationLogEvent::TaskFinished {
            outcome,
            diagnostic: logged,
            ..
        } = &events[0]
        else {
            panic!("关闭 task-record 后仍应发出 TaskFinished");
        };
        assert_eq!(*outcome, StandardTranslationLogTaskOutcome::CommitFailed);
        assert_eq!(logged.as_ref(), Some(&diagnostic));
    }

    #[tokio::test]
    async fn translate_stale_snapshot_fails_before_any_llm_request() {
        let llm_calls = Arc::new(AtomicUsize::new(0));
        let project = project();
        let execution = BoundManagedTranslationExecution {
            llm: CountingLlm {
                calls: Arc::clone(&llm_calls),
            },
            delay: ImmediateDelay,
            cpu: InlineCpu,
            repository: StaleRepository,
            planning: RpgMakerTranslationPlanningConfiguration::new(NonZeroUsize::MIN),
            request: RpgMakerTranslationRequestConfiguration::new(Vec::new(), Duration::ZERO),
            managed_system_prompt: test_managed_system_prompt(&project),
            task_records: NoOpTranslationTaskRecordSink,
            task_log: None,
            cancellation: CooperativeCancellation::default(),
            project,
            llm_client: Arc::new(SnapshotGateClient),
            semantics: Arc::new(FakeSemantics),
            standard_task_count: 0,
        };

        let error = execution
            .run()
            .await
            .expect_err("stale 快照必须阻止 Translate");

        assert_eq!(error.domain(), "translations");
        assert_eq!(error.kind(), "stale_snapshot");
        assert_eq!(error.operation(), Some("translations.translate"));
        assert_eq!(llm_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn write_back_open_reports_stale_snapshot_explicitly() {
        let project = project();
        let reader = ManagedTranslationReadService::new(StaleRepository).bind(&project);

        let error = reader
            .open("quests".to_owned())
            .await
            .expect_err("stale 快照必须阻止 WriteBack open");

        assert_eq!(error.domain(), "translations");
        assert_eq!(error.kind(), "stale_snapshot");
        assert_eq!(error.operation(), Some("translations.open"));
        let diagnostic = error.safe_diagnostic().expect("stale open 必须有诊断");
        assert_eq!(diagnostic.stage, DiagnosticStage::WriteBack);
        assert_eq!(diagnostic.impact, DiagnosticImpact::Unchanged);
    }

    #[tokio::test]
    async fn changed_translation_pair_between_planning_and_admission_blocks_all_llm_requests() {
        let project = project();
        let first = ManagedTranslationSnapshot::new(
            project.source_snapshot_fingerprint(),
            vec![collection("quests", "", vec![scalar_unit("q:1", "")])],
        )
        .expect("首个快照应合法");
        let unit = first
            .collection("quests")
            .expect("集合应存在")
            .unit("q:1")
            .expect("unit 应存在");
        let external_pair = unit
            .translation_pair(
                ManagedTranslationContent::scalar("外部译文"),
                Sha256Fingerprint::from_bytes([0x52; 32]),
            )
            .expect("外部 pair 应合法");
        let external_checkpoint = ManagedTranslationCheckpoint::new(
            &first,
            vec![ManagedTranslationReplacement::new(
                "quests",
                "q:1",
                Some(external_pair),
            )],
        )
        .expect("外部 checkpoint 应合法");
        let second = external_checkpoint
            .expected_snapshot(&first)
            .expect("第二个快照应可投影");
        assert_eq!(
            first.manifest_fingerprint(),
            second.manifest_fingerprint(),
            "测试必须证明 manifest 相同但 translation/state pair 已变化"
        );
        assert_ne!(first, second);

        let loads = Arc::new(AtomicUsize::new(0));
        let checkpoints = Arc::new(AtomicUsize::new(0));
        let llm_calls = Arc::new(AtomicUsize::new(0));
        let execution = BoundManagedTranslationExecution {
            llm: CountingLlm {
                calls: Arc::clone(&llm_calls),
            },
            delay: ImmediateDelay,
            cpu: InlineCpu,
            repository: SnapshotChangingRepository {
                first,
                second,
                loads: Arc::clone(&loads),
                checkpoints: Arc::clone(&checkpoints),
            },
            planning: RpgMakerTranslationPlanningConfiguration::new(NonZeroUsize::MIN),
            request: RpgMakerTranslationRequestConfiguration::new(Vec::new(), Duration::ZERO),
            managed_system_prompt: test_managed_system_prompt(&project),
            task_records: NoOpTranslationTaskRecordSink,
            task_log: None,
            cancellation: CooperativeCancellation::default(),
            project,
            llm_client: Arc::new(SnapshotGateClient),
            semantics: Arc::new(FakeSemantics),
            standard_task_count: 0,
        };

        let error = match execution.run().await {
            Ok(_) => panic!("双次读取的 translation/state pair 不一致必须阻止请求准入"),
            Err(error) => error,
        };

        assert_eq!(error.domain(), "translations");
        assert_eq!(error.kind(), "snapshot_changed");
        let diagnostic = error
            .safe_diagnostic()
            .expect("执行前快照变化必须有安全诊断");
        assert_failure(
            diagnostic,
            DiagnosticCode::ProjectState,
            DiagnosticFailureKind::StateMismatch,
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::CheckProjectState,
        );
        assert_eq!(loads.load(Ordering::SeqCst), 2);
        assert_eq!(
            checkpoints.load(Ordering::SeqCst),
            1,
            "完整 guard 应在重读前取得明确 Applied 终态"
        );
        assert_eq!(
            llm_calls.load(Ordering::SeqCst),
            0,
            "快照 pair 变化后不得发出任何模型请求"
        );
    }
}
