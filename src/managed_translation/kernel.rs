//! 引擎无关的 Managed 翻译规划、协议、执行与 checkpoint 协调内核。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, RecoveryFact, SafeDiagnostic,
};
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::execution::llm_request::{
    AsyncDelay, LlmRequestAttemptEvidence, LlmRequestAttemptRecord, LlmRequestCancellationPoint,
    LlmRequestExecutionOutcome, LlmRequestRetryPolicy, LlmRequestTerminalFailureClassification,
    execute_llm_request_with_retry,
};
use crate::execution::ordered::{
    OrderedExecutionError, OrderedExecutionHandler, OrderedExecutionLimits,
    OrderedFinalizationDisposition, OrderedTaskResult, execute_ordered,
};
use crate::execution::{CooperativeCancellation, OperationCompletion};
use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::llm::{
    ChatMessage, ChatMessageRole, LlmClientConcurrency, LlmFinishReason,
    LlmRequestDiagnosticSource, LlmRequestExecutor, LlmResponse,
};
use crate::lua_host::TrustedLuaHostCallError;
use crate::translation_protocol::{
    ParsedTranslationAssistantEntry, TranslationAssistantValueError, TranslationResponseEnvelope,
    TranslationTaskResponseParseError, parse_translation_response,
};

#[cfg(test)]
use super::managed_translation_system_prompt_fragment;
use super::{
    MANAGED_REFLOW_WIRE_MARKER, ManagedPreparedContent, ManagedPreparedContentAcceptance,
    ManagedPreparedContentError, ManagedPreparedContentRejection, ManagedTranslationCollection,
    ManagedTranslationContent, ManagedTranslationMetadata, ManagedTranslationPair,
    ManagedTranslationSemantics, ManagedTranslationShape, ManagedTranslationTerm,
    ManagedTranslationUnit,
};
#[cfg(test)]
use super::{
    ManagedPreparedTranslation, ManagedPreparedTranslationAcceptance,
    ManagedPreparedTranslationStatus,
};

const IN_FLIGHT_WINDOW_MULTIPLIER: NonZeroUsize = NonZeroUsize::new(3).unwrap();

/// 引擎适配器提供给共享内核的一致冻结快照。
pub(crate) trait ManagedTranslationSnapshotView: Clone + Eq + Send + Sync + 'static {
    fn collections(&self) -> &[ManagedTranslationCollection];
}

/// checkpoint 是否必须覆盖规划读取的完整快照。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedTranslationCheckpointMode {
    CompleteGuard,
    Targeted,
}

/// 存储适配器完成 checkpoint 准备与提交后的明确终态。
pub(crate) enum ManagedTranslationStoreCheckpoint<S> {
    Applied(S),
    PreparationFailed(TrustedLuaHostCallError),
    NotApplied(TrustedLuaHostCallError),
    OutcomeUnknown(TrustedLuaHostCallError),
    Failed(TrustedLuaHostCallError),
}

/// 引擎项目存储向共享内核暴露的最小冻结读取与 guarded CAS。
pub(crate) trait ManagedTranslationStore: Send + Sync {
    type Snapshot: ManagedTranslationSnapshotView;

    fn load(
        &self,
    ) -> impl Future<Output = Result<Option<Self::Snapshot>, TrustedLuaHostCallError>> + Send;

    fn checkpoint(
        &self,
        baseline: &Self::Snapshot,
        replacements: Vec<ManagedTranslationReplacement>,
        mode: ManagedTranslationCheckpointMode,
    ) -> impl Future<Output = ManagedTranslationStoreCheckpoint<Self::Snapshot>> + Send;
}

/// 一个 frozen snapshot 上的 checkpoint 替换项。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedTranslationReplacement {
    collection: String,
    key: String,
    replacement: Option<ManagedTranslationPair>,
}

impl ManagedTranslationReplacement {
    pub(crate) fn new(
        collection: impl Into<String>,
        key: impl Into<String>,
        replacement: Option<ManagedTranslationPair>,
    ) -> Self {
        Self {
            collection: collection.into(),
            key: key.into(),
            replacement,
        }
    }

    pub(crate) fn into_parts(self) -> (String, String, Option<ManagedTranslationPair>) {
        (self.collection, self.key, self.replacement)
    }
}

/// 一轮 Managed 执行所需的外部策略和运行内编号偏移。
#[derive(Clone, Debug)]
pub(crate) struct ManagedTranslationKernelConfiguration {
    target_user_message_characters: usize,
    retry_delays: Vec<Duration>,
    max_retry_after: Duration,
    response_envelope: TranslationResponseEnvelope,
    preceding_task_count: usize,
}

impl ManagedTranslationKernelConfiguration {
    pub(crate) fn new(
        target_user_message_characters: usize,
        retry_delays: Vec<Duration>,
        max_retry_after: Duration,
        response_envelope: TranslationResponseEnvelope,
        preceding_task_count: usize,
    ) -> Self {
        Self {
            target_user_message_characters,
            retry_delays,
            max_retry_after,
            response_envelope,
            preceding_task_count,
        }
    }
}

/// Managed 任务记录中的协议级事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ManagedTranslationProtocolDiagnostic {
    NonStopFinish { reason: String },
    InvalidResponse { message: String },
    InvalidId { item_index: usize },
    UnknownId { item_index: usize, id: usize },
}

/// 一个 Assistant 条目的单次解析投影。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedTranslationAssistantEntry {
    id: String,
    value: Value,
    canonical_id: Option<usize>,
    value_error: Option<TranslationAssistantValueError>,
}

impl ManagedTranslationAssistantEntry {
    fn projected(entry: ParsedTranslationAssistantEntry) -> Self {
        let (id, value, canonical_id, translation) = entry.into_parts();
        Self {
            id,
            value,
            canonical_id,
            value_error: translation.err(),
        }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        String,
        Value,
        Option<usize>,
        Option<TranslationAssistantValueError>,
    ) {
        (self.id, self.value, self.canonical_id, self.value_error)
    }
}

/// 唯一响应解析器建立的任务记录投影。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ManagedTranslationResponseRecord {
    Parsed {
        raw_assistant: String,
        thinking: Option<String>,
        entries: Vec<ManagedTranslationAssistantEntry>,
    },
    Invalid {
        raw_assistant: String,
        error: TranslationTaskResponseParseError,
    },
    Unprocessed {
        raw_assistant: String,
    },
}

/// 一个已启动任务从开始到请求/响应处理完成的旁路证据。
#[derive(Clone, Debug)]
pub(crate) struct ManagedTranslationTaskEvidence {
    started_at: Option<OffsetDateTime>,
    task_started: Option<Instant>,
    attempt_count: usize,
    attempts: Vec<LlmRequestAttemptRecord>,
    response: Option<ManagedTranslationResponseRecord>,
}

impl ManagedTranslationTaskEvidence {
    pub(crate) const fn attempt_count(&self) -> usize {
        self.attempt_count
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        Option<OffsetDateTime>,
        Option<Instant>,
        usize,
        Vec<LlmRequestAttemptRecord>,
        Option<ManagedTranslationResponseRecord>,
    ) {
        (
            self.started_at,
            self.task_started,
            self.attempt_count,
            self.attempts,
            self.response,
        )
    }

    #[cfg(test)]
    pub(crate) fn empty_for_test() -> Self {
        Self {
            started_at: None,
            task_started: None,
            attempt_count: 0,
            attempts: Vec::new(),
            response: None,
        }
    }
}

/// checkpoint 对一个 TaskBlock 的最终可观察终态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedTranslationTaskCheckpointState {
    Complete,
    Partial,
    Unavailable,
    ExecutionFailed,
    CommitPreparationFailed,
    CommitNotApplied,
    OutcomeUnknown,
    EarlierFailure,
    Cancelled,
}

/// 一个临时 ID 的逐项验收投影。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedTranslationTaskUnitResult {
    id: usize,
    accepted: bool,
    reason: Option<String>,
    details: Option<Value>,
}

impl ManagedTranslationTaskUnitResult {
    pub(crate) fn into_parts(self) -> (usize, bool, Option<String>, Option<Value>) {
        (self.id, self.accepted, self.reason, self.details)
    }

    pub(crate) fn is_rejected_for(&self, reason: &str) -> bool {
        !self.accepted && self.reason.as_deref() == Some(reason)
    }
}

/// 一个临时 ID 对应的全部 Lua 身份。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedTranslationTaskIdentity {
    id: usize,
    targets: Vec<ManagedUnitIdentity>,
}

impl ManagedTranslationTaskIdentity {
    pub(crate) fn id(&self) -> usize {
        self.id
    }

    pub(crate) fn targets(&self) -> &[ManagedUnitIdentity] {
        &self.targets
    }
}

/// root handler 向引擎 observer 一次性交付的完整记录投影。
pub(crate) struct ManagedTranslationTaskObservation {
    pub(crate) total_tasks: usize,
    pub(crate) run_wide_ordinal: usize,
    pub(crate) collection: String,
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) identities: Vec<ManagedTranslationTaskIdentity>,
    pub(crate) evidence: ManagedTranslationTaskEvidence,
    pub(crate) unit_results: Vec<ManagedTranslationTaskUnitResult>,
    pub(crate) protocol_diagnostics: Vec<ManagedTranslationProtocolDiagnostic>,
    pub(crate) checkpoint: ManagedTranslationTaskCheckpointState,
    pub(crate) confirmed_committed_units: Option<usize>,
    pub(crate) diagnostic: Option<SafeDiagnostic>,
}

/// 项目日志与 task-record 对共享内核证据的旁路消费端口。
pub(crate) trait ManagedTranslationObserver: Send + Sync {
    fn recording_enabled(&self) -> bool;
    fn declare_total_tasks(&self, total_tasks: usize);
    fn task_started(&self, run_wide_ordinal: usize, total_tasks: usize);
    fn task_finished(&self, observation: ManagedTranslationTaskObservation);
}

/// collection/key 的稳定 Lua 身份。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ManagedUnitIdentity {
    collection: String,
    key: String,
}

impl ManagedUnitIdentity {
    pub(crate) fn new(collection: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            collection: collection.into(),
            key: key.into(),
        }
    }

    pub(crate) fn collection(&self) -> &str {
        &self.collection
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }
}

/// 当前 Managed 人工候选会话中的稳定物理单元下标。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ManagedTranslationCandidateHandle(usize);

impl ManagedTranslationCandidateHandle {
    pub(crate) const fn new(value: usize) -> Self {
        Self(value)
    }

    pub(crate) const fn get(self) -> usize {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedTranslationCandidateUnitStatus {
    Current,
    Missing,
    Stale,
    NotApplicable,
    Unavailable,
}

impl ManagedTranslationCandidateUnitStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Missing => "missing",
            Self::Stale => "stale",
            Self::NotApplicable => "not_applicable",
            Self::Unavailable => "unavailable",
        }
    }
}

/// 人工候选会话向调用方投影的完整冻结单元。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedTranslationCandidateUnit {
    handle: ManagedTranslationCandidateHandle,
    collection: String,
    key: String,
    kind: String,
    shape: ManagedTranslationShape,
    original: ManagedTranslationContent,
    context: String,
    metadata: Option<ManagedTranslationMetadata>,
    translation: Option<ManagedTranslationContent>,
    model_content: ManagedTranslationContent,
    terms: Vec<ManagedTranslationTerm>,
    status: ManagedTranslationCandidateUnitStatus,
    family_size: usize,
}

impl ManagedTranslationCandidateUnit {
    pub(crate) const fn handle(&self) -> ManagedTranslationCandidateHandle {
        self.handle
    }

    pub(crate) fn collection(&self) -> &str {
        &self.collection
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }

    pub(crate) fn kind(&self) -> &str {
        &self.kind
    }

    pub(crate) const fn shape(&self) -> ManagedTranslationShape {
        self.shape
    }

    pub(crate) fn original(&self) -> &ManagedTranslationContent {
        &self.original
    }

    pub(crate) fn context(&self) -> &str {
        &self.context
    }

    pub(crate) fn metadata(&self) -> Option<&ManagedTranslationMetadata> {
        self.metadata.as_ref()
    }

    pub(crate) fn translation(&self) -> Option<&ManagedTranslationContent> {
        self.translation.as_ref()
    }

    pub(crate) fn model_content(&self) -> &ManagedTranslationContent {
        &self.model_content
    }

    pub(crate) fn terms(&self) -> &[ManagedTranslationTerm] {
        &self.terms
    }

    pub(crate) const fn status(&self) -> ManagedTranslationCandidateUnitStatus {
        self.status
    }

    pub(crate) const fn family_size(&self) -> usize {
        self.family_size
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedTranslationCandidateRequest {
    handle: ManagedTranslationCandidateHandle,
    candidate: ManagedTranslationContent,
    replace_current: bool,
}

impl ManagedTranslationCandidateRequest {
    pub(crate) fn new(
        handle: ManagedTranslationCandidateHandle,
        candidate: ManagedTranslationContent,
        replace_current: bool,
    ) -> Self {
        Self {
            handle,
            candidate,
            replace_current,
        }
    }
}

/// 人工候选普通拒绝携带的结构化补充事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ManagedTranslationCandidateRejectionDetails {
    Item { item: usize },
    ItemCount { expected: usize, actual: usize },
    Unavailable { detail: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ManagedTranslationCandidateAcceptance {
    Accepted {
        content: ManagedTranslationContent,
        changed_units: usize,
    },
    Rejected {
        reason: String,
        details: Option<ManagedTranslationCandidateRejectionDetails>,
    },
}

impl ManagedTranslationCandidateAcceptance {
    fn rejected(
        reason: impl Into<String>,
        details: Option<ManagedTranslationCandidateRejectionDetails>,
    ) -> Self {
        Self::Rejected {
            reason: reason.into(),
            details,
        }
    }
}

#[derive(Clone)]
struct ManagedTranslationCandidateWrite {
    unit_index: usize,
    expected: Option<ManagedTranslationPair>,
    replacement: ManagedTranslationPair,
}

/// 一批候选完成领域验收后形成的冻结 CAS 计划。
#[derive(Clone)]
pub(crate) struct PreparedManagedTranslationCandidateAcceptance<S> {
    baseline: S,
    results: Vec<ManagedTranslationCandidateAcceptance>,
    replacements: Vec<ManagedTranslationReplacement>,
    writes: Vec<ManagedTranslationCandidateWrite>,
}

impl<S> PreparedManagedTranslationCandidateAcceptance<S> {
    pub(crate) fn baseline(&self) -> &S {
        &self.baseline
    }

    pub(crate) fn results(&self) -> &[ManagedTranslationCandidateAcceptance] {
        &self.results
    }

    pub(crate) fn replacements(&self) -> &[ManagedTranslationReplacement] {
        &self.replacements
    }
}

struct ManagedTranslationCandidateUnitState {
    view: ManagedTranslationCandidateUnit,
    prepared: Option<PreparedManagedUnit>,
    family_index: Option<usize>,
    unavailable_reason: Option<String>,
}

struct ManagedTranslationCandidateSessionState<S> {
    baseline: S,
    units: Vec<ManagedTranslationCandidateUnitState>,
}

/// 一次 `open` 冻结的 Managed 语义、family 与项目快照。
pub(crate) struct ManagedTranslationCandidateSession<S>
where
    S: ManagedTranslationSnapshotView,
{
    families: Vec<Vec<usize>>,
    state: Mutex<ManagedTranslationCandidateSessionState<S>>,
}

/// 一项 unit 在本轮 Translate 结束时的业务结果。
#[derive(Clone)]
pub(crate) struct ManagedUnitResult {
    status: super::TrustedLuaManagedTranslationResultStatus,
    translation: Option<ManagedTranslationContent>,
    reason: Option<String>,
    details: Option<Value>,
}

impl ManagedUnitResult {
    fn current(content: ManagedTranslationContent) -> Self {
        Self {
            status: super::TrustedLuaManagedTranslationResultStatus::Current,
            translation: Some(content),
            reason: None,
            details: None,
        }
    }

    fn translated(content: ManagedTranslationContent) -> Self {
        Self {
            status: super::TrustedLuaManagedTranslationResultStatus::Translated,
            translation: Some(content),
            reason: None,
            details: None,
        }
    }

    fn not_applicable() -> Self {
        Self {
            status: super::TrustedLuaManagedTranslationResultStatus::NotApplicable,
            translation: None,
            reason: None,
            details: None,
        }
    }

    fn unavailable(reason: impl Into<String>, details: Option<Value>) -> Self {
        Self {
            status: super::TrustedLuaManagedTranslationResultStatus::Unavailable,
            translation: None,
            reason: Some(reason.into()),
            details,
        }
    }

    pub(crate) const fn status(&self) -> super::TrustedLuaManagedTranslationResultStatus {
        self.status
    }

    pub(crate) fn translation(&self) -> Option<&ManagedTranslationContent> {
        self.translation.as_ref()
    }

    pub(crate) fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    pub(crate) fn details(&self) -> Option<&Value> {
        self.details.as_ref()
    }
}

/// 内核完成后交给引擎投影 Lua report/open 的唯一状态。
pub(crate) struct ManagedTranslationKernelOutput<S> {
    snapshot: S,
    results: HashMap<ManagedUnitIdentity, ManagedUnitResult>,
}

impl<S> ManagedTranslationKernelOutput<S> {
    pub(crate) fn into_parts(self) -> (S, HashMap<ManagedUnitIdentity, ManagedUnitResult>) {
        (self.snapshot, self.results)
    }
}

#[derive(Clone, Copy)]
struct ManagedUnitStateContext(Sha256Fingerprint);

impl ManagedUnitStateContext {
    fn finish(self, content: &ManagedTranslationContent) -> Sha256Fingerprint {
        let mut hasher = Sha256FramedHasher::new(b"att.managed_translation.state");
        hasher
            .frame(1, self.0.as_bytes())
            .frame(2, content.canonical_json().as_bytes());
        hasher.finish()
    }
}

#[derive(Clone)]
struct PreparedManagedUnit {
    identity: ManagedUnitIdentity,
    kind: String,
    shape: ManagedTranslationShape,
    original: ManagedTranslationContent,
    instruction: String,
    context: String,
    semantics: ManagedPreparedContent,
    state_context: ManagedUnitStateContext,
    stored_translation: Option<ManagedTranslationPair>,
}

impl PreparedManagedUnit {
    fn active(&self) -> bool {
        self.semantics.is_active()
    }

    fn current_translation(&self) -> Option<&ManagedTranslationPair> {
        self.stored_translation
            .as_ref()
            .filter(|pair| pair.state() == self.state_context.finish(pair.content()))
    }

    fn model_content(&self) -> ManagedTranslationContent {
        self.semantics.model_content()
    }

    fn terms(&self) -> Vec<ManagedTranslationTerm> {
        self.semantics.terms()
    }
}

#[derive(Clone)]
struct ManagedFamily {
    representative: PreparedManagedUnit,
    members: Vec<PreparedManagedUnit>,
    current: Option<ManagedTranslationContent>,
}

#[derive(Clone)]
struct ManagedTaskExpected {
    id: usize,
    representative: PreparedManagedUnit,
    targets: Vec<ManagedUnitIdentity>,
}

#[derive(Clone)]
struct ManagedTaskBlock {
    collection: String,
    messages: Vec<ChatMessage>,
    expected: Vec<ManagedTaskExpected>,
}

struct ManagedTranslationPlan {
    preflight: Vec<ManagedTranslationReplacement>,
    tasks: Vec<ManagedTaskBlock>,
    initial_results: HashMap<ManagedUnitIdentity, ManagedUnitResult>,
}

#[derive(Clone)]
struct ManagedUnitPreparationInput {
    collection_name: String,
    instruction: String,
    unit: ManagedTranslationUnit,
}

fn managed_preparation_inputs<S>(snapshot: &S) -> Vec<ManagedUnitPreparationInput>
where
    S: ManagedTranslationSnapshotView,
{
    snapshot
        .collections()
        .iter()
        .flat_map(|collection| {
            collection
                .units()
                .iter()
                .cloned()
                .map(|unit| ManagedUnitPreparationInput {
                    collection_name: collection.name().to_owned(),
                    instruction: collection.instruction().to_owned(),
                    unit,
                })
        })
        .collect()
}

fn prepare_managed_input(
    input: ManagedUnitPreparationInput,
    semantics: &dyn ManagedTranslationSemantics,
    system_prompt_fingerprint: Sha256Fingerprint,
) -> Result<PreparedManagedUnit, TrustedLuaHostCallError> {
    let ManagedUnitPreparationInput {
        collection_name,
        instruction,
        unit,
    } = input;
    let base_context = managed_semantic_context(&instruction, unit.shape(), unit.context());
    let prepared = ManagedPreparedContent::prepare(
        semantics,
        unit.kind(),
        unit.shape(),
        unit.original(),
        &base_context,
    )
    .map_err(|source| match source {
        ManagedPreparedContentError::InvalidOriginal(source) => {
            managed_internal_source_error("invalid_original", source, "managed_preparation")
        }
        ManagedPreparedContentError::Semantics(source) => source,
    })?;
    let state_context = managed_state_context(
        semantics,
        &unit,
        &instruction,
        &prepared,
        system_prompt_fingerprint,
    );
    Ok(PreparedManagedUnit {
        identity: ManagedUnitIdentity::new(collection_name, unit.key()),
        kind: unit.kind().to_owned(),
        shape: unit.shape(),
        original: unit.original().clone(),
        instruction,
        context: unit.context().to_owned(),
        semantics: prepared,
        state_context,
        stored_translation: unit.translation().cloned(),
    })
}

fn managed_semantic_context(
    instruction: &str,
    shape: ManagedTranslationShape,
    context: &str,
) -> String {
    format!(
        "managed_translation\nshape={}\ninstruction={instruction}\ncontext={context}",
        shape.storage_name()
    )
}

fn managed_state_context(
    semantics: &dyn ManagedTranslationSemantics,
    unit: &ManagedTranslationUnit,
    instruction: &str,
    prepared: &ManagedPreparedContent,
    system_prompt_fingerprint: Sha256Fingerprint,
) -> ManagedUnitStateContext {
    let mut hasher = Sha256FramedHasher::new(b"att.managed_translation.context");
    hasher
        .frame(1, semantics.engine_semantic_identity().as_bytes())
        .frame(2, semantics.source_language().as_bytes())
        .frame(3, semantics.target_language().as_bytes())
        .frame(4, unit.kind().as_bytes())
        .frame(5, unit.shape().storage_name().as_bytes())
        .frame(6, unit.original().canonical_json().as_bytes())
        .frame(7, instruction.as_bytes())
        .frame(8, unit.context().as_bytes());
    prepared.frame_automatic_state_context(&mut hasher);
    hasher.frame(11, system_prompt_fingerprint.as_bytes());
    ManagedUnitStateContext(hasher.finish())
}

fn managed_system_prompt_fingerprint(system_prompt: &str) -> Sha256Fingerprint {
    let mut hasher = Sha256FramedHasher::new(b"att.managed_translation.system_prompt");
    hasher.frame(1, system_prompt.as_bytes());
    hasher.finish()
}

fn unit_store_pair(
    unit: &PreparedManagedUnit,
    content: ManagedTranslationContent,
) -> Result<ManagedTranslationPair, TrustedLuaHostCallError> {
    let model = ManagedTranslationUnit::new(
        &unit.identity.key,
        &unit.kind,
        unit.shape,
        unit.original.clone(),
        &unit.context,
        None,
    )
    .map_err(|source| {
        managed_internal_source_error("invalid_pair", source, "managed_translation_pair")
    })?;
    model
        .translation_pair(content.clone(), unit.state_context.finish(&content))
        .map_err(|source| {
            managed_internal_source_error("invalid_pair", source, "managed_translation_pair")
        })
}

impl<S> ManagedTranslationCandidateSession<S>
where
    S: ManagedTranslationSnapshotView,
{
    pub(crate) fn open(
        baseline: S,
        semantics: &dyn ManagedTranslationSemantics,
    ) -> Result<Self, TrustedLuaHostCallError> {
        let system_prompt_fingerprint =
            managed_system_prompt_fingerprint(semantics.system_prompt());
        let mut units = Vec::new();
        for input in managed_preparation_inputs(&baseline) {
            let source_unit = input.unit.clone();
            let collection = input.collection_name.clone();
            let prepared = prepare_managed_input(input, semantics, system_prompt_fingerprint);
            let handle = ManagedTranslationCandidateHandle::new(units.len());
            match prepared {
                Ok(prepared) => {
                    let status = if !prepared.active() {
                        ManagedTranslationCandidateUnitStatus::NotApplicable
                    } else if prepared.current_translation().is_some() {
                        ManagedTranslationCandidateUnitStatus::Current
                    } else if prepared.stored_translation.is_some() {
                        ManagedTranslationCandidateUnitStatus::Stale
                    } else {
                        ManagedTranslationCandidateUnitStatus::Missing
                    };
                    units.push(ManagedTranslationCandidateUnitState {
                        view: ManagedTranslationCandidateUnit {
                            handle,
                            collection,
                            key: source_unit.key().to_owned(),
                            kind: source_unit.kind().to_owned(),
                            shape: source_unit.shape(),
                            original: source_unit.original().clone(),
                            context: source_unit.context().to_owned(),
                            metadata: source_unit.metadata().cloned(),
                            translation: source_unit
                                .translation()
                                .map(|pair| pair.content().clone()),
                            model_content: prepared.model_content(),
                            terms: prepared.terms(),
                            status,
                            family_size: 1,
                        },
                        prepared: Some(prepared),
                        family_index: None,
                        unavailable_reason: None,
                    });
                }
                Err(source) => {
                    units.push(ManagedTranslationCandidateUnitState {
                        view: ManagedTranslationCandidateUnit {
                            handle,
                            collection,
                            key: source_unit.key().to_owned(),
                            kind: source_unit.kind().to_owned(),
                            shape: source_unit.shape(),
                            original: source_unit.original().clone(),
                            context: source_unit.context().to_owned(),
                            metadata: source_unit.metadata().cloned(),
                            translation: source_unit
                                .translation()
                                .map(|pair| pair.content().clone()),
                            model_content: source_unit.original().clone(),
                            terms: Vec::new(),
                            status: ManagedTranslationCandidateUnitStatus::Unavailable,
                            family_size: 1,
                        },
                        prepared: None,
                        family_index: None,
                        unavailable_reason: Some(source.to_string()),
                    });
                }
            }
        }

        let mut families = Vec::<Vec<usize>>::new();
        let mut family_by_context = HashMap::<Sha256Fingerprint, usize>::new();
        for (unit_index, unit) in units.iter_mut().enumerate() {
            let Some(prepared) = unit.prepared.as_ref().filter(|prepared| prepared.active()) else {
                continue;
            };
            let family_index = *family_by_context
                .entry(prepared.state_context.0)
                .or_insert_with(|| {
                    let index = families.len();
                    families.push(Vec::new());
                    index
                });
            unit.family_index = Some(family_index);
            families[family_index].push(unit_index);
        }

        for (family_index, members) in families.iter().enumerate() {
            let family_size = members.len();
            let mut current = None::<ManagedTranslationContent>;
            for &unit_index in members {
                let unit = &units[unit_index];
                let prepared = unit
                    .prepared
                    .as_ref()
                    .expect("active family member 必须保留 prepared");
                if let Some(pair) = prepared.current_translation() {
                    match &current {
                        None => current = Some(pair.content().clone()),
                        Some(existing) if existing == pair.content() => {}
                        Some(_) => {
                            return Err(managed_project_state_error(
                                "current_conflict",
                                format!(
                                    "同一托管翻译去重族存在冲突的 Current 译文：{}/{}",
                                    unit.view.collection, unit.view.key
                                ),
                                DiagnosticFailureKind::ConflictingValues,
                                "resolve_conflicting_managed_translations",
                            ));
                        }
                    }
                }
            }
            for &unit_index in members {
                units[unit_index].family_index = Some(family_index);
                units[unit_index].view.family_size = family_size;
            }
        }

        Ok(Self {
            families,
            state: Mutex::new(ManagedTranslationCandidateSessionState { baseline, units }),
        })
    }

    pub(crate) fn units(
        &self,
    ) -> Result<Vec<ManagedTranslationCandidateUnit>, ManagedTranslationCandidateSessionError> {
        Ok(self
            .state
            .lock()
            .map_err(|_| ManagedTranslationCandidateSessionError::SessionPoisoned)?
            .units
            .iter()
            .map(|unit| unit.view.clone())
            .collect())
    }

    pub(crate) fn get(
        &self,
        collection: &str,
        key: &str,
    ) -> Result<Option<ManagedTranslationCandidateUnit>, ManagedTranslationCandidateSessionError>
    {
        Ok(self
            .state
            .lock()
            .map_err(|_| ManagedTranslationCandidateSessionError::SessionPoisoned)?
            .units
            .iter()
            .find(|unit| unit.view.collection == collection && unit.view.key == key)
            .map(|unit| unit.view.clone()))
    }

    pub(crate) fn prepare_acceptance(
        &self,
        requests: Vec<ManagedTranslationCandidateRequest>,
    ) -> Result<
        PreparedManagedTranslationCandidateAcceptance<S>,
        ManagedTranslationCandidateSessionError,
    > {
        let state = self
            .state
            .lock()
            .map_err(|_| ManagedTranslationCandidateSessionError::SessionPoisoned)?;
        let mut results = vec![None; requests.len()];
        let mut requests_by_family = BTreeMap::<usize, Vec<usize>>::new();

        for (request_index, request) in requests.iter().enumerate() {
            let unit = state.units.get(request.handle.get()).ok_or(
                ManagedTranslationCandidateSessionError::InvalidHandle {
                    handle: request.handle.get(),
                    unit_count: state.units.len(),
                },
            )?;
            match unit.view.status {
                ManagedTranslationCandidateUnitStatus::NotApplicable => {
                    results[request_index] = Some(ManagedTranslationCandidateAcceptance::rejected(
                        "not_applicable",
                        None,
                    ));
                }
                ManagedTranslationCandidateUnitStatus::Unavailable => {
                    results[request_index] = Some(ManagedTranslationCandidateAcceptance::rejected(
                        "unavailable",
                        Some(ManagedTranslationCandidateRejectionDetails::Unavailable {
                            detail: unit
                                .unavailable_reason
                                .clone()
                                .unwrap_or_else(|| "managed_unit_unavailable".to_owned()),
                        }),
                    ));
                }
                ManagedTranslationCandidateUnitStatus::Current
                | ManagedTranslationCandidateUnitStatus::Missing
                | ManagedTranslationCandidateUnitStatus::Stale => {
                    let family_index = unit.family_index.ok_or(
                        ManagedTranslationCandidateSessionError::ActiveUnitMissingFamily {
                            handle: request.handle.get(),
                        },
                    )?;
                    requests_by_family
                        .entry(family_index)
                        .or_default()
                        .push(request_index);
                }
            }
        }

        let mut replacements = Vec::new();
        let mut writes = Vec::new();
        for (family_index, request_indices) in requests_by_family {
            let first = &requests[request_indices[0]];
            if request_indices.iter().skip(1).any(|&request_index| {
                let request = &requests[request_index];
                request.candidate != first.candidate
                    || request.replace_current != first.replace_current
            }) {
                for request_index in request_indices {
                    results[request_index] = Some(ManagedTranslationCandidateAcceptance::rejected(
                        "conflicting_candidate",
                        None,
                    ));
                }
                continue;
            }

            let members = self.families.get(family_index).ok_or(
                ManagedTranslationCandidateSessionError::InvalidFamily {
                    family: family_index,
                },
            )?;
            let unit = &state.units[first.handle.get()];
            let prepared = unit.prepared.as_ref().ok_or(
                ManagedTranslationCandidateSessionError::ActiveUnitMissingPrepared {
                    handle: first.handle.get(),
                },
            )?;
            let accepted = match prepared
                .semantics
                .accept(first.candidate.clone())
                .map_err(ManagedTranslationCandidateSessionError::Semantics)?
            {
                ManagedPreparedContentAcceptance::Accepted { content, .. } => content,
                ManagedPreparedContentAcceptance::Rejected { rejection } => {
                    let details = candidate_rejection_details(&rejection);
                    for request_index in request_indices {
                        results[request_index] =
                            Some(ManagedTranslationCandidateAcceptance::rejected(
                                rejection.reason(),
                                details.clone(),
                            ));
                    }
                    continue;
                }
            };

            let changes_current = members.iter().any(|&member_index| {
                let member = &state.units[member_index];
                member.view.status == ManagedTranslationCandidateUnitStatus::Current
                    && member.view.translation.as_ref() != Some(&accepted)
            });
            if changes_current && !first.replace_current {
                for request_index in request_indices {
                    results[request_index] = Some(ManagedTranslationCandidateAcceptance::rejected(
                        "current_replacement_required",
                        None,
                    ));
                }
                continue;
            }

            let mut family_writes = Vec::with_capacity(members.len());
            let mut changed_units = 0usize;
            for &member_index in members {
                let member = &state.units[member_index];
                let prepared = member.prepared.as_ref().ok_or(
                    ManagedTranslationCandidateSessionError::ActiveUnitMissingPrepared {
                        handle: member_index,
                    },
                )?;
                let replacement = unit_store_pair(prepared, accepted.clone())
                    .map_err(ManagedTranslationCandidateSessionError::Pair)?;
                let expected = prepared.stored_translation.clone();
                if expected.as_ref() != Some(&replacement) {
                    changed_units = changed_units.saturating_add(1);
                    replacements.push(ManagedTranslationReplacement::new(
                        &prepared.identity.collection,
                        &prepared.identity.key,
                        Some(replacement.clone()),
                    ));
                }
                family_writes.push(ManagedTranslationCandidateWrite {
                    unit_index: member_index,
                    expected,
                    replacement,
                });
            }
            writes.extend(family_writes);
            for request_index in request_indices {
                results[request_index] = Some(ManagedTranslationCandidateAcceptance::Accepted {
                    content: accepted.clone(),
                    changed_units,
                });
            }
        }

        Ok(PreparedManagedTranslationCandidateAcceptance {
            baseline: state.baseline.clone(),
            results: results
                .into_iter()
                .map(|result| result.expect("每项 Managed 人工候选必须得到普通结果"))
                .collect(),
            replacements,
            writes,
        })
    }

    /// 在外部存储确认应用 CAS 后，用重读快照推进同一个会话。
    pub(crate) fn apply_committed(
        &self,
        prepared: &PreparedManagedTranslationCandidateAcceptance<S>,
        committed_snapshot: S,
    ) -> Result<(), ManagedTranslationCandidateSessionError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ManagedTranslationCandidateSessionError::SessionPoisoned)?;
        if state.baseline != prepared.baseline {
            return Err(ManagedTranslationCandidateSessionError::BaselineChanged);
        }
        for write in &prepared.writes {
            let unit_count = state.units.len();
            let unit = state.units.get_mut(write.unit_index).ok_or(
                ManagedTranslationCandidateSessionError::InvalidHandle {
                    handle: write.unit_index,
                    unit_count,
                },
            )?;
            let prepared_unit = unit.prepared.as_mut().ok_or(
                ManagedTranslationCandidateSessionError::ActiveUnitMissingPrepared {
                    handle: write.unit_index,
                },
            )?;
            if prepared_unit.stored_translation != write.expected {
                return Err(
                    ManagedTranslationCandidateSessionError::FrozenExpectedChanged {
                        collection: unit.view.collection.clone(),
                        key: unit.view.key.clone(),
                    },
                );
            }
            let committed =
                snapshot_translation(&committed_snapshot, &unit.view.collection, &unit.view.key);
            if committed != Some(Some(&write.replacement)) {
                return Err(
                    ManagedTranslationCandidateSessionError::CommittedSnapshotMismatch {
                        collection: unit.view.collection.clone(),
                        key: unit.view.key.clone(),
                    },
                );
            }
            prepared_unit.stored_translation = Some(write.replacement.clone());
            unit.view.translation = Some(write.replacement.content().clone());
            unit.view.status = ManagedTranslationCandidateUnitStatus::Current;
        }
        state.baseline = committed_snapshot;
        Ok(())
    }
}

fn snapshot_translation<'a, S>(
    snapshot: &'a S,
    collection: &str,
    key: &str,
) -> Option<Option<&'a ManagedTranslationPair>>
where
    S: ManagedTranslationSnapshotView,
{
    snapshot
        .collections()
        .iter()
        .find(|candidate| candidate.name() == collection)
        .and_then(|collection| collection.unit(key))
        .map(ManagedTranslationUnit::translation)
}

fn candidate_rejection_details(
    rejection: &ManagedPreparedContentRejection,
) -> Option<ManagedTranslationCandidateRejectionDetails> {
    if let Some((expected, actual)) = rejection.expected_actual() {
        Some(ManagedTranslationCandidateRejectionDetails::ItemCount { expected, actual })
    } else {
        rejection
            .item_number()
            .map(|item| ManagedTranslationCandidateRejectionDetails::Item { item })
    }
}

#[derive(Debug)]
pub(crate) enum ManagedTranslationCandidateSessionError {
    SessionPoisoned,
    InvalidHandle { handle: usize, unit_count: usize },
    InvalidFamily { family: usize },
    ActiveUnitMissingFamily { handle: usize },
    ActiveUnitMissingPrepared { handle: usize },
    BaselineChanged,
    FrozenExpectedChanged { collection: String, key: String },
    CommittedSnapshotMismatch { collection: String, key: String },
    Semantics(TrustedLuaHostCallError),
    Pair(TrustedLuaHostCallError),
}

impl fmt::Display for ManagedTranslationCandidateSessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SessionPoisoned => formatter.write_str("Managed 人工候选会话互斥锁已中毒"),
            Self::InvalidHandle { handle, unit_count } => write!(
                formatter,
                "Managed 人工候选 handle 超出冻结会话范围：handle={handle}, unit_count={unit_count}"
            ),
            Self::InvalidFamily { family } => {
                write!(formatter, "Managed 人工候选 family 不存在：{family}")
            }
            Self::ActiveUnitMissingFamily { handle } => {
                write!(formatter, "活动 Managed 单元缺少 family：handle={handle}")
            }
            Self::ActiveUnitMissingPrepared { handle } => {
                write!(
                    formatter,
                    "活动 Managed 单元缺少 prepared 语义：handle={handle}"
                )
            }
            Self::BaselineChanged => {
                formatter.write_str("Managed 人工候选会话基线已在本次 CAS 计划后改变")
            }
            Self::FrozenExpectedChanged { collection, key } => write!(
                formatter,
                "Managed 人工候选冻结预期已改变：{collection}/{key}"
            ),
            Self::CommittedSnapshotMismatch { collection, key } => write!(
                formatter,
                "Managed 人工候选提交后快照与 replacement 不一致：{collection}/{key}"
            ),
            Self::Semantics(source) => write!(formatter, "Managed 人工候选语义验收失败：{source}"),
            Self::Pair(source) => write!(formatter, "Managed 人工候选无法建立受管译文：{source}"),
        }
    }
}

impl Error for ManagedTranslationCandidateSessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Semantics(source) | Self::Pair(source) => Some(source),
            _ => None,
        }
    }
}

fn finalize_managed_plan(
    prepared_units: Vec<PreparedManagedUnit>,
    system_prompt: &str,
    target_characters: usize,
) -> Result<ManagedTranslationPlan, TrustedLuaHostCallError> {
    let mut families = Vec::<ManagedFamily>::new();
    let mut family_by_context = HashMap::<Sha256Fingerprint, usize>::new();
    let mut initial_results = HashMap::with_capacity(prepared_units.len());
    let mut desired = HashMap::<ManagedUnitIdentity, Option<ManagedTranslationContent>>::new();

    for unit in &prepared_units {
        if !unit.active() {
            initial_results.insert(unit.identity.clone(), ManagedUnitResult::not_applicable());
            desired.insert(unit.identity.clone(), None);
            continue;
        }
        let family_index = *family_by_context
            .entry(unit.state_context.0)
            .or_insert_with(|| {
                let index = families.len();
                families.push(ManagedFamily {
                    representative: unit.clone(),
                    members: Vec::new(),
                    current: None,
                });
                index
            });
        families[family_index].members.push(unit.clone());
    }

    for family in &mut families {
        let mut current = None::<ManagedTranslationContent>;
        for member in &family.members {
            if let Some(pair) = member.current_translation() {
                match &current {
                    None => current = Some(pair.content().clone()),
                    Some(existing) if existing == pair.content() => {}
                    Some(_) => {
                        return Err(managed_project_state_error(
                            "current_conflict",
                            format!(
                                "同一托管翻译去重族存在冲突的 Current 译文：{}/{}",
                                family.representative.identity.collection,
                                family.representative.identity.key
                            ),
                            DiagnosticFailureKind::ConflictingValues,
                            "resolve_conflicting_managed_translations",
                        ));
                    }
                }
            }
        }
        family.current = current.clone();
        for member in &family.members {
            desired.insert(member.identity.clone(), current.clone());
            if let Some(current) = &current {
                initial_results.insert(
                    member.identity.clone(),
                    ManagedUnitResult::current(current.clone()),
                );
            } else {
                initial_results.insert(
                    member.identity.clone(),
                    ManagedUnitResult::unavailable("pending", None),
                );
            }
        }
    }

    let mut preflight = Vec::new();
    for unit in &prepared_units {
        let replacement = desired
            .get(&unit.identity)
            .expect("每个 prepared unit 都必须形成 preflight 目标");
        let pair = replacement
            .as_ref()
            .map(|content| unit_store_pair(unit, content.clone()))
            .transpose()?;
        if unit.stored_translation.as_ref() != pair.as_ref() {
            preflight.push(ManagedTranslationReplacement::new(
                &unit.identity.collection,
                &unit.identity.key,
                pair,
            ));
        }
    }

    let pending = families
        .into_iter()
        .filter(|family| family.current.is_none())
        .collect::<Vec<_>>();
    let tasks = pack_managed_tasks(pending, system_prompt, target_characters);
    Ok(ManagedTranslationPlan {
        preflight,
        tasks,
        initial_results,
    })
}

fn pack_managed_tasks(
    families: Vec<ManagedFamily>,
    system_prompt: &str,
    target_characters: usize,
) -> Vec<ManagedTaskBlock> {
    let mut tasks = Vec::new();
    let mut cursor = 0usize;
    while cursor < families.len() {
        let collection = families[cursor].representative.identity.collection.clone();
        let instruction = families[cursor].representative.instruction.clone();
        let mut current = Vec::<ManagedFamily>::new();
        let base_characters = managed_message_prefix_character_count(&instruction);
        let mut body_characters = 0usize;
        let mut terminology_characters = 0usize;
        let mut seen_terms = HashSet::<(String, String)>::new();
        while cursor < families.len()
            && families[cursor].representative.identity.collection == collection
        {
            let candidate = &families[cursor];
            let candidate_terms = candidate.representative.terms();
            let mut candidate_seen = HashSet::new();
            let new_terms = candidate_terms
                .iter()
                .filter(|term| {
                    let identity = (term.term().to_owned(), term.translation().to_owned());
                    !seen_terms.contains(&identity) && candidate_seen.insert(identity)
                })
                .collect::<Vec<_>>();
            let added_terminology_characters = new_terms
                .iter()
                .map(|term| managed_term_character_count(term))
                .sum::<usize>()
                .saturating_add(
                    usize::from(!new_terms.is_empty() && seen_terms.is_empty())
                        * "\nTerminology:\n\n".chars().count(),
                );
            let added_body_characters =
                managed_family_character_count(current.len() + 1, candidate);
            let candidate_characters = base_characters
                .saturating_add(terminology_characters)
                .saturating_add(added_terminology_characters)
                .saturating_add(body_characters)
                .saturating_add(added_body_characters);
            if !current.is_empty() && candidate_characters > target_characters {
                tasks.push(finalize_managed_task(
                    &collection,
                    &instruction,
                    system_prompt,
                    std::mem::take(&mut current),
                ));
                body_characters = 0;
                terminology_characters = 0;
                seen_terms.clear();
                continue;
            }
            for term in new_terms {
                seen_terms.insert((term.term().to_owned(), term.translation().to_owned()));
            }
            terminology_characters =
                terminology_characters.saturating_add(added_terminology_characters);
            body_characters = body_characters.saturating_add(added_body_characters);
            current.push(candidate.clone());
            cursor += 1;
            if current.len() == 1 && candidate_characters > target_characters {
                tasks.push(finalize_managed_task(
                    &collection,
                    &instruction,
                    system_prompt,
                    std::mem::take(&mut current),
                ));
                body_characters = 0;
                terminology_characters = 0;
                seen_terms.clear();
            }
        }
        if !current.is_empty() {
            tasks.push(finalize_managed_task(
                &collection,
                &instruction,
                system_prompt,
                current,
            ));
        }
    }
    tasks
}

fn managed_message_prefix_character_count(instruction: &str) -> usize {
    let mut rendered = String::from("Instruction:\n\n");
    push_markdown_literal(&mut rendered, instruction);
    rendered.push('\n');
    rendered.chars().count()
}

fn managed_term_character_count(term: &ManagedTranslationTerm) -> usize {
    let mut rendered = String::from("- ");
    push_markdown_literal(&mut rendered, term.term());
    rendered.push_str(" → ");
    push_markdown_literal(&mut rendered, term.translation());
    rendered.push('\n');
    rendered.chars().count()
}

fn managed_family_character_count(id: usize, family: &ManagedFamily) -> usize {
    let mut rendered = String::new();
    render_managed_family(&mut rendered, id, family);
    rendered.chars().count()
}

fn finalize_managed_task(
    collection: &str,
    instruction: &str,
    system_prompt: &str,
    families: Vec<ManagedFamily>,
) -> ManagedTaskBlock {
    let user = render_managed_user_message(instruction, &families);
    let expected = families
        .into_iter()
        .enumerate()
        .map(|(index, family)| ManagedTaskExpected {
            id: index + 1,
            representative: family.representative,
            targets: family
                .members
                .into_iter()
                .map(|member| member.identity)
                .collect(),
        })
        .collect();
    ManagedTaskBlock {
        collection: collection.to_owned(),
        messages: vec![
            ChatMessage::new(ChatMessageRole::System, system_prompt),
            ChatMessage::new(ChatMessageRole::User, user),
        ],
        expected,
    }
}

fn render_managed_user_message(instruction: &str, families: &[ManagedFamily]) -> String {
    let mut terms = Vec::<ManagedTranslationTerm>::new();
    let mut seen_terms = HashSet::new();
    for family in families {
        for term in family.representative.terms() {
            if seen_terms.insert((term.term().to_owned(), term.translation().to_owned())) {
                terms.push(term);
            }
        }
    }

    let mut output = String::from("Instruction:\n\n");
    push_markdown_literal(&mut output, instruction);
    output.push('\n');
    if !terms.is_empty() {
        output.push_str("\nTerminology:\n\n");
        for term in terms {
            output.push_str("- ");
            push_markdown_literal(&mut output, term.term());
            output.push_str(" → ");
            push_markdown_literal(&mut output, term.translation());
            output.push('\n');
        }
    }
    for (index, family) in families.iter().enumerate() {
        render_managed_family(&mut output, index + 1, family);
    }
    output
}

fn render_managed_family(output: &mut String, id: usize, family: &ManagedFamily) {
    let unit = &family.representative;
    output.push('\n');
    if !unit.context.is_empty() {
        output.push_str("Context:\n\n> ");
        append_blockquote_text(output, &unit.context);
        output.push_str("\n\n");
    }
    output.push_str("Text [");
    output.push_str(&id.to_string());
    output.push_str("] (");
    output.push_str(&shape_label(unit.shape, &unit.original));
    output.push_str("):");
    match unit.model_content() {
        ManagedTranslationContent::Scalar(value)
            if unit.shape == ManagedTranslationShape::Single =>
        {
            output.push(' ');
            output.push_str(&value);
            output.push('\n');
        }
        ManagedTranslationContent::Scalar(value) => {
            output.push_str("\n\n> ");
            append_blockquote_text(output, &value);
            output.push('\n');
        }
        ManagedTranslationContent::Array(values) => {
            output.push('\n');
            for value in values {
                output.push_str("\n> ");
                output.push_str(&value);
            }
            output.push('\n');
        }
    }
}

fn shape_label(shape: ManagedTranslationShape, original: &ManagedTranslationContent) -> String {
    match shape {
        ManagedTranslationShape::Single => "single line".to_owned(),
        ManagedTranslationShape::Reflow => MANAGED_REFLOW_WIRE_MARKER.to_owned(),
        ManagedTranslationShape::Lines => format!(
            "{} lines, corresponding line by line",
            original.as_array().map_or(0, <[String]>::len)
        ),
        ManagedTranslationShape::Items => format!(
            "{} items, corresponding item by item",
            original.as_array().map_or(0, <[String]>::len)
        ),
    }
}

fn push_markdown_literal(output: &mut String, value: &str) {
    for character in value.chars() {
        if character.is_ascii_punctuation() {
            output.push('\\');
        }
        output.push(character);
    }
}

fn append_blockquote_text(output: &mut String, value: &str) {
    let mut lines = value.split('\n');
    if let Some(first) = lines.next() {
        output.push_str(first);
    }
    for line in lines {
        output.push_str("\n> ");
        output.push_str(line);
    }
}

struct ManagedTaskEvidenceBuilder {
    recording: bool,
    started_at: Option<OffsetDateTime>,
    started: Option<Instant>,
    attempt_count: usize,
    attempts: Vec<LlmRequestAttemptRecord>,
}

impl ManagedTaskEvidenceBuilder {
    fn new(recording: bool) -> Self {
        Self {
            recording,
            started_at: recording.then(OffsetDateTime::now_utc),
            started: recording.then(Instant::now),
            attempt_count: 0,
            attempts: Vec::new(),
        }
    }

    fn absorb_request_evidence(&mut self, evidence: LlmRequestAttemptEvidence) {
        let (attempt_count, attempts) = evidence.into_parts();
        self.attempt_count = self.attempt_count.max(attempt_count);
        self.attempts.extend(attempts);
    }

    fn finish(
        self,
        response: Option<ManagedTranslationResponseRecord>,
    ) -> ManagedTranslationTaskEvidence {
        ManagedTranslationTaskEvidence {
            started_at: self.started_at,
            task_started: self.started,
            attempt_count: self.attempt_count,
            attempts: self.attempts,
            response: self.recording.then_some(response).flatten(),
        }
    }
}

struct ManagedRawTaskExecution {
    outcome: ManagedRawTaskOutcome,
    evidence: ManagedTaskEvidenceBuilder,
}

enum ManagedRawTaskOutcome {
    Response(LlmResponse),
    Unavailable {
        reason: String,
        details: Option<Value>,
    },
}

struct ManagedPreparedTask {
    decisions: Vec<ManagedTaskDecision>,
    protocol_diagnostics: Vec<ManagedTranslationProtocolDiagnostic>,
    evidence: ManagedTranslationTaskEvidence,
}

#[derive(Clone)]
struct ManagedTaskDecision {
    id: usize,
    accepted: Option<ManagedTranslationContent>,
    reason: Option<String>,
    details: Option<Value>,
}

impl ManagedTaskDecision {
    fn accepted(id: usize, content: ManagedTranslationContent) -> Self {
        Self {
            id,
            accepted: Some(content),
            reason: None,
            details: None,
        }
    }

    fn rejected(id: usize, reason: impl Into<String>, details: Option<Value>) -> Self {
        Self {
            id,
            accepted: None,
            reason: Some(reason.into()),
            details,
        }
    }

    fn record(&self) -> ManagedTranslationTaskUnitResult {
        ManagedTranslationTaskUnitResult {
            id: self.id,
            accepted: self.accepted.is_some(),
            reason: self.reason.clone(),
            details: self.details.clone(),
        }
    }
}

struct ManagedTaskStageError {
    source: TrustedLuaHostCallError,
    evidence: ManagedTranslationTaskEvidence,
    cancelled: bool,
}

struct ProcessedManagedResponse {
    decisions: Vec<ManagedTaskDecision>,
    protocol_diagnostics: Vec<ManagedTranslationProtocolDiagnostic>,
    response_record: Option<ManagedTranslationResponseRecord>,
}

#[derive(Debug)]
struct ManagedResponseProcessingFailure {
    source: TrustedLuaHostCallError,
    response_record: Option<ManagedTranslationResponseRecord>,
}

fn process_managed_response(
    task: &ManagedTaskBlock,
    response: LlmResponse,
    envelope: TranslationResponseEnvelope,
    recording: bool,
) -> Result<ProcessedManagedResponse, Box<ManagedResponseProcessingFailure>> {
    let raw = response.content().to_owned();
    let mut protocol_diagnostics = Vec::new();
    if response.finish_reason() != &LlmFinishReason::Stop {
        protocol_diagnostics.push(ManagedTranslationProtocolDiagnostic::NonStopFinish {
            reason: response.finish_reason().to_string(),
        });
    }
    let parsed = match parse_translation_response(&raw, envelope) {
        Ok(parsed) => parsed,
        Err(source) => {
            protocol_diagnostics.push(ManagedTranslationProtocolDiagnostic::InvalidResponse {
                message: source.business_message(),
            });
            return Ok(ProcessedManagedResponse {
                decisions: task
                    .expected
                    .iter()
                    .map(|expected| {
                        ManagedTaskDecision::rejected(
                            expected.id,
                            "model_response_unusable",
                            Some(json!({
                                "kind": source.kind().code(),
                                "line": source.line().get(),
                                "column": source.column().get(),
                            })),
                        )
                    })
                    .collect(),
                protocol_diagnostics,
                response_record: recording.then(|| ManagedTranslationResponseRecord::Invalid {
                    raw_assistant: raw,
                    error: source,
                }),
            });
        }
    };
    let response_record = recording.then(|| {
        let (thinking, entries) = parsed.clone().into_parts();
        ManagedTranslationResponseRecord::Parsed {
            raw_assistant: raw,
            thinking,
            entries: entries
                .into_iter()
                .map(ManagedTranslationAssistantEntry::projected)
                .collect(),
        }
    });

    let mut counts = vec![0usize; task.expected.len()];
    let mut known = vec![None::<&ParsedTranslationAssistantEntry>; task.expected.len()];
    for (item_index, entry) in parsed.entries().iter().enumerate() {
        match entry.canonical_id() {
            None => {
                protocol_diagnostics
                    .push(ManagedTranslationProtocolDiagnostic::InvalidId { item_index });
            }
            Some(id) => match id.checked_sub(1) {
                Some(index) if index < task.expected.len() => {
                    counts[index] = counts[index].saturating_add(1);
                    known[index].get_or_insert(entry);
                }
                _ => {
                    protocol_diagnostics
                        .push(ManagedTranslationProtocolDiagnostic::UnknownId { item_index, id });
                }
            },
        }
    }
    let mut decisions = Vec::with_capacity(task.expected.len());
    for (index, expected) in task.expected.iter().enumerate() {
        let decision = match counts[index] {
            0 => ManagedTaskDecision::rejected(expected.id, "missing_id", None),
            2.. => ManagedTaskDecision::rejected(expected.id, "duplicate_id", None),
            1 => {
                let entry = known[index].expect("计数为一的 ID 必须保留条目");
                match entry.translation() {
                    Ok(values) => accept_managed_candidate(expected, values).map_err(|source| {
                        Box::new(ManagedResponseProcessingFailure {
                            source,
                            response_record: response_record.clone(),
                        })
                    })?,
                    Err(source) => ManagedTaskDecision::rejected(
                        expected.id,
                        "invalid_value",
                        Some(json!({"message": source.business_message()})),
                    ),
                }
            }
        };
        decisions.push(decision);
    }

    Ok(ProcessedManagedResponse {
        decisions,
        protocol_diagnostics,
        response_record,
    })
}

fn accept_managed_candidate(
    expected: &ManagedTaskExpected,
    values: &[String],
) -> Result<ManagedTaskDecision, TrustedLuaHostCallError> {
    let unit = &expected.representative;
    match unit.semantics.accept_wire_values(values)? {
        ManagedPreparedContentAcceptance::Accepted { content, .. } => {
            Ok(ManagedTaskDecision::accepted(expected.id, content))
        }
        ManagedPreparedContentAcceptance::Rejected { rejection } => {
            let details = managed_content_rejection_details(&rejection);
            Ok(ManagedTaskDecision::rejected(
                expected.id,
                rejection.reason(),
                details,
            ))
        }
    }
}

fn managed_content_rejection_details(rejection: &ManagedPreparedContentRejection) -> Option<Value> {
    if let Some((expected, actual)) = rejection.expected_actual() {
        Some(json!({"expected": expected, "actual": actual}))
    } else {
        rejection.item_number().map(|item| json!({"item": item}))
    }
}

/// 一个可由任意引擎适配并生产装配的 Managed 执行内核。
pub(crate) struct ManagedTranslationKernel<L, D, C, S, O>
where
    L: LlmRequestExecutor,
    S: ManagedTranslationStore,
{
    llm: L,
    delay: D,
    cpu: C,
    store: S,
    observer: O,
    configuration: ManagedTranslationKernelConfiguration,
    cancellation: CooperativeCancellation,
    llm_client: Arc<L::Client>,
    semantics: Arc<dyn ManagedTranslationSemantics>,
}

impl<L, D, C, S, O> ManagedTranslationKernel<L, D, C, S, O>
where
    L: LlmRequestExecutor,
    S: ManagedTranslationStore,
{
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        llm: L,
        delay: D,
        cpu: C,
        store: S,
        observer: O,
        configuration: ManagedTranslationKernelConfiguration,
        cancellation: CooperativeCancellation,
        llm_client: Arc<L::Client>,
        semantics: Arc<dyn ManagedTranslationSemantics>,
    ) -> Self {
        Self {
            llm,
            delay,
            cpu,
            store,
            observer,
            configuration,
            cancellation,
            llm_client,
            semantics,
        }
    }
}

impl<L, D, C, S, O> ManagedTranslationKernel<L, D, C, S, O>
where
    L: LlmRequestExecutor + Clone + 'static,
    L::Client: LlmClientConcurrency,
    L::Error: LlmRequestDiagnosticSource,
    D: AsyncDelay + Clone + 'static,
    C: CpuTaskExecutor + Clone + 'static,
    S: ManagedTranslationStore + 'static,
    O: ManagedTranslationObserver + 'static,
{
    pub(crate) async fn run(
        self,
    ) -> Result<Option<ManagedTranslationKernelOutput<S::Snapshot>>, TrustedLuaHostCallError> {
        if self.cancellation.is_requested() {
            return Err(managed_cancelled_error("托管翻译在规划前已取消"));
        }
        let Some(snapshot) = self.store.load().await? else {
            self.observer
                .declare_total_tasks(self.configuration.preceding_task_count);
            return Ok(None);
        };

        let preparation_inputs = managed_preparation_inputs(&snapshot);
        let system_prompt = self.semantics.system_prompt().to_owned();
        let system_prompt_fingerprint = managed_system_prompt_fingerprint(&system_prompt);
        let semantics = Arc::clone(&self.semantics);
        let prepared_units = self
            .cpu
            .execute_ordered_map(preparation_inputs, move |input| {
                prepare_managed_input(input, semantics.as_ref(), system_prompt_fingerprint)
            })
            .await
            .map_err(|source| managed_cpu_error("planning_failed", source, "managed_planning"))?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| {
                normalize_managed_host_error(source, "managed_translation_preparation")
            })?;
        let target_characters = self.configuration.target_user_message_characters;
        let plan = self
            .cpu
            .execute(move || {
                finalize_managed_plan(prepared_units, &system_prompt, target_characters)
            })
            .await
            .map_err(|source| managed_cpu_error("planning_failed", source, "managed_planning"))?
            .map_err(|source| {
                normalize_managed_host_error(source, "managed_translation_planning")
            })?;

        let expected_baseline = match self
            .store
            .checkpoint(
                &snapshot,
                plan.preflight.clone(),
                ManagedTranslationCheckpointMode::CompleteGuard,
            )
            .await
        {
            ManagedTranslationStoreCheckpoint::Applied(expected) => expected,
            ManagedTranslationStoreCheckpoint::PreparationFailed(error)
            | ManagedTranslationStoreCheckpoint::NotApplied(error)
            | ManagedTranslationStoreCheckpoint::OutcomeUnknown(error)
            | ManagedTranslationStoreCheckpoint::Failed(error) => return Err(error),
        };
        let baseline = self.store.load().await?.ok_or_else(|| {
            managed_project_state_error(
                "snapshot_disappeared",
                "托管翻译快照在预检提交后消失",
                DiagnosticFailureKind::NotFound,
                "reload_managed_translation_snapshot",
            )
        })?;
        if baseline != expected_baseline {
            return Err(managed_project_state_error(
                "snapshot_changed",
                "托管翻译快照在规划与请求准入之间发生改变；未发出模型请求",
                DiagnosticFailureKind::StateMismatch,
                "reload_managed_translation_snapshot",
            ));
        }

        let total_tasks = self
            .configuration
            .preceding_task_count
            .saturating_add(plan.tasks.len());
        self.observer.declare_total_tasks(total_tasks);
        let state = ManagedRunState {
            baseline,
            results: plan.initial_results,
        };
        let handler = ManagedOrderedHandler {
            kernel: &self,
            total_tasks,
        };
        let limits = OrderedExecutionLimits::new(
            self.llm_client.max_concurrent_requests(),
            IN_FLIGHT_WINDOW_MULTIPLIER,
        );
        let completion = execute_ordered(plan.tasks, limits, &self.cancellation, &handler, state)
            .await
            .map_err(managed_ordered_error)?;
        let OperationCompletion::Completed(state) = completion else {
            return Err(managed_cancelled_error(
                "托管翻译已取消；已确认的自然序前缀保持提交",
            ));
        };
        let final_snapshot = self.store.load().await?.ok_or_else(|| {
            managed_project_state_error(
                "snapshot_disappeared",
                "托管翻译快照在执行完成后消失",
                DiagnosticFailureKind::NotFound,
                "reload_managed_translation_snapshot",
            )
        })?;
        if final_snapshot != state.baseline {
            return Err(managed_project_state_error(
                "snapshot_changed",
                "托管翻译快照在任务执行期间发生非本轮变更；不会投影混合快照",
                DiagnosticFailureKind::StateMismatch,
                "reload_managed_translation_snapshot",
            ));
        }
        Ok(Some(ManagedTranslationKernelOutput {
            snapshot: final_snapshot,
            results: state.results,
        }))
    }
}

struct ManagedRunState<S> {
    baseline: S,
    results: HashMap<ManagedUnitIdentity, ManagedUnitResult>,
}

struct ManagedOrderedHandler<'a, L, D, C, S, O>
where
    L: LlmRequestExecutor,
    S: ManagedTranslationStore,
{
    kernel: &'a ManagedTranslationKernel<L, D, C, S, O>,
    total_tasks: usize,
}

impl<L, D, C, S, O> OrderedExecutionHandler<ManagedTaskBlock>
    for ManagedOrderedHandler<'_, L, D, C, S, O>
where
    L: LlmRequestExecutor + Clone + 'static,
    L::Client: LlmClientConcurrency,
    L::Error: LlmRequestDiagnosticSource,
    D: AsyncDelay + Clone + 'static,
    C: CpuTaskExecutor + Clone + 'static,
    S: ManagedTranslationStore + 'static,
    O: ManagedTranslationObserver + 'static,
{
    type Executed = ManagedRawTaskExecution;
    type Prepared = ManagedPreparedTask;
    type StageError = ManagedTaskStageError;
    type State = ManagedRunState<S::Snapshot>;
    type Error = TrustedLuaHostCallError;

    async fn execute(
        &self,
        ordinal: usize,
        task: &ManagedTaskBlock,
    ) -> Result<Self::Executed, Self::StageError> {
        self.kernel.observer.task_started(
            self.kernel
                .configuration
                .preceding_task_count
                .saturating_add(ordinal),
            self.total_tasks,
        );
        execute_managed_request(self.kernel, task).await
    }

    async fn prepare(
        &self,
        _ordinal: usize,
        task: &ManagedTaskBlock,
        executed: Self::Executed,
    ) -> Result<Self::Prepared, Self::StageError> {
        let ManagedRawTaskExecution { outcome, evidence } = executed;
        match outcome {
            ManagedRawTaskOutcome::Unavailable { reason, details } => Ok(ManagedPreparedTask {
                decisions: task
                    .expected
                    .iter()
                    .map(|expected| {
                        ManagedTaskDecision::rejected(expected.id, &reason, details.clone())
                    })
                    .collect(),
                protocol_diagnostics: Vec::new(),
                evidence: evidence.finish(None),
            }),
            ManagedRawTaskOutcome::Response(response) => {
                let task = task.clone();
                let response_envelope = self.kernel.configuration.response_envelope;
                let recording = self.kernel.observer.recording_enabled();
                let unprocessed_response =
                    recording.then(|| ManagedTranslationResponseRecord::Unprocessed {
                        raw_assistant: response.content().into(),
                    });
                match self
                    .kernel
                    .cpu
                    .execute(move || {
                        process_managed_response(&task, response, response_envelope, recording)
                    })
                    .await
                {
                    Ok(Ok(processed)) => Ok(ManagedPreparedTask {
                        decisions: processed.decisions,
                        protocol_diagnostics: processed.protocol_diagnostics,
                        evidence: evidence.finish(processed.response_record),
                    }),
                    Ok(Err(failure)) => {
                        let failure = *failure;
                        Err(ManagedTaskStageError {
                            source: normalize_managed_host_error(
                                failure.source,
                                "managed_response_acceptance",
                            ),
                            evidence: evidence
                                .finish(failure.response_record.or(unprocessed_response)),
                            cancelled: false,
                        })
                    }
                    Err(source) => {
                        let cancelled = self.kernel.cancellation.is_requested()
                            && matches!(source, CpuTaskExecutionError::Cancelled);
                        Err(ManagedTaskStageError {
                            source: managed_cpu_error(
                                "response_processing_failed",
                                source,
                                "managed_response_processing",
                            ),
                            evidence: evidence.finish(unprocessed_response),
                            cancelled,
                        })
                    }
                }
            }
        }
    }

    async fn finalize(
        &self,
        ordinal: usize,
        task: ManagedTaskBlock,
        result: OrderedTaskResult<Self::Prepared, Self::StageError>,
        disposition: OrderedFinalizationDisposition,
        state: &mut Self::State,
    ) -> Result<(), Self::Error> {
        finalize_managed_task_result(
            self.kernel,
            self.total_tasks,
            ordinal,
            task,
            result,
            disposition,
            state,
        )
        .await
    }
}

async fn execute_managed_request<L, D, C, S, O>(
    kernel: &ManagedTranslationKernel<L, D, C, S, O>,
    task: &ManagedTaskBlock,
) -> Result<ManagedRawTaskExecution, ManagedTaskStageError>
where
    L: LlmRequestExecutor,
    L::Error: LlmRequestDiagnosticSource,
    D: AsyncDelay,
    S: ManagedTranslationStore,
    O: ManagedTranslationObserver,
{
    let mut evidence = ManagedTaskEvidenceBuilder::new(kernel.observer.recording_enabled());
    let request_execution = execute_llm_request_with_retry(
        &kernel.llm,
        kernel.llm_client.as_ref(),
        &task.messages,
        LlmRequestRetryPolicy::new(
            &kernel.configuration.retry_delays,
            kernel.configuration.max_retry_after,
        ),
        &kernel.delay,
        &kernel.cancellation,
        kernel.observer.recording_enabled(),
    )
    .await;
    let (outcome, request_evidence) = request_execution.into_parts();
    evidence.absorb_request_evidence(request_evidence);

    match outcome {
        LlmRequestExecutionOutcome::Response { response, .. } => Ok(ManagedRawTaskExecution {
            outcome: ManagedRawTaskOutcome::Response(response),
            evidence,
        }),
        LlmRequestExecutionOutcome::RetryAfterExceedsMaximum {
            retry_after,
            maximum,
            ..
        } => Ok(ManagedRawTaskExecution {
            outcome: ManagedRawTaskOutcome::Unavailable {
                reason: "retry_after_exceeds_maximum".to_owned(),
                details: Some(json!({
                    "retry_after_ms": duration_millis(retry_after),
                    "maximum_ms": duration_millis(maximum),
                })),
            },
            evidence,
        }),
        LlmRequestExecutionOutcome::RetryBudgetExhausted { .. } => Ok(ManagedRawTaskExecution {
            outcome: ManagedRawTaskOutcome::Unavailable {
                reason: "request_retry_exhausted".to_owned(),
                details: None,
            },
            evidence,
        }),
        LlmRequestExecutionOutcome::Fatal {
            attempt,
            source,
            diagnostic,
            cancelled,
            classification,
        } => {
            let mut error =
                if classification == LlmRequestTerminalFailureClassification::RetryableCancelled {
                    managed_cancelled_source_error(source)
                } else {
                    let message = format!("托管翻译第 {attempt} 次 LLM 请求不可恢复地失败");
                    TrustedLuaHostCallError::new(
                        "translations",
                        if cancelled {
                            "cancelled"
                        } else {
                            "request_failed"
                        },
                        message,
                        None,
                        Some(Arc::new(source)),
                    )
                    .with_operation("translations.translate")
                };
            if let Some(diagnostic) = diagnostic {
                error = error.with_safe_diagnostic(diagnostic);
            }
            Err(ManagedTaskStageError {
                source: error,
                evidence: evidence.finish(None),
                cancelled,
            })
        }
        LlmRequestExecutionOutcome::Cancelled { point, .. } => {
            let message = match point {
                LlmRequestCancellationPoint::BeforeAttempt => "托管翻译模型请求已取消",
                LlmRequestCancellationPoint::DuringRetryWait => "托管翻译在网络重试等待期间取消",
                LlmRequestCancellationPoint::AfterRetryWait => "托管翻译在网络重试等待完成后取消",
            };
            Err(ManagedTaskStageError {
                source: managed_cancelled_error(message),
                evidence: evidence.finish(None),
                cancelled: true,
            })
        }
    }
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

async fn finalize_managed_task_result<L, D, C, S, O>(
    kernel: &ManagedTranslationKernel<L, D, C, S, O>,
    total_tasks: usize,
    ordinal: usize,
    task: ManagedTaskBlock,
    result: OrderedTaskResult<ManagedPreparedTask, ManagedTaskStageError>,
    disposition: OrderedFinalizationDisposition,
    state: &mut ManagedRunState<S::Snapshot>,
) -> Result<(), TrustedLuaHostCallError>
where
    L: LlmRequestExecutor,
    S: ManagedTranslationStore,
    O: ManagedTranslationObserver,
{
    match result {
        OrderedTaskResult::ExecutionFailed(failure)
        | OrderedTaskResult::PreparationFailed(failure) => {
            let checkpoint = match disposition {
                OrderedFinalizationDisposition::CancelledNoApply => {
                    ManagedTranslationTaskCheckpointState::Cancelled
                }
                OrderedFinalizationDisposition::AfterEarlierFailureNoApply => {
                    ManagedTranslationTaskCheckpointState::EarlierFailure
                }
                OrderedFinalizationDisposition::Apply if failure.cancelled => {
                    ManagedTranslationTaskCheckpointState::Cancelled
                }
                OrderedFinalizationDisposition::Apply => {
                    ManagedTranslationTaskCheckpointState::ExecutionFailed
                }
            };
            let diagnostic = failure.source.safe_diagnostic().cloned();
            submit_managed_observation(
                kernel,
                total_tasks,
                ordinal,
                &task,
                failure.evidence,
                Vec::new(),
                Vec::new(),
                checkpoint,
                Some(0),
                diagnostic,
            );
            if disposition == OrderedFinalizationDisposition::Apply && !failure.cancelled {
                Err(failure.source)
            } else {
                Ok(())
            }
        }
        OrderedTaskResult::Prepared(prepared) => {
            let protocol_diagnostics = prepared.protocol_diagnostics.clone();
            if disposition != OrderedFinalizationDisposition::Apply {
                let checkpoint = match disposition {
                    OrderedFinalizationDisposition::CancelledNoApply => {
                        ManagedTranslationTaskCheckpointState::Cancelled
                    }
                    OrderedFinalizationDisposition::AfterEarlierFailureNoApply => {
                        ManagedTranslationTaskCheckpointState::EarlierFailure
                    }
                    OrderedFinalizationDisposition::Apply => unreachable!(),
                };
                let records = prepared
                    .decisions
                    .iter()
                    .map(ManagedTaskDecision::record)
                    .collect();
                submit_managed_observation(
                    kernel,
                    total_tasks,
                    ordinal,
                    &task,
                    prepared.evidence,
                    records,
                    protocol_diagnostics,
                    checkpoint,
                    Some(0),
                    None,
                );
                return Ok(());
            }

            let replacements = (|| {
                let mut replacements = Vec::new();
                for decision in &prepared.decisions {
                    let expected = &task.expected[decision.id - 1];
                    for target in &expected.targets {
                        let unit = baseline_unit(&state.baseline, target)?;
                        let replacement = decision
                            .accepted
                            .as_ref()
                            .map(|content| {
                                let prepared_target = PreparedManagedUnit {
                                    identity: target.clone(),
                                    kind: unit.kind().to_owned(),
                                    shape: expected.representative.shape,
                                    original: unit.original().clone(),
                                    instruction: expected.representative.instruction.clone(),
                                    context: unit.context().to_owned(),
                                    semantics: expected.representative.semantics.clone(),
                                    state_context: expected.representative.state_context,
                                    stored_translation: unit.translation().cloned(),
                                };
                                unit_store_pair(&prepared_target, content.clone())
                            })
                            .transpose()?;
                        replacements.push(ManagedTranslationReplacement::new(
                            &target.collection,
                            &target.key,
                            replacement,
                        ));
                    }
                }
                Ok::<_, TrustedLuaHostCallError>(replacements)
            })();
            let replacements = match replacements {
                Ok(replacements) => replacements,
                Err(error) => {
                    let diagnostic = error.safe_diagnostic().cloned();
                    let record_units = prepared
                        .decisions
                        .iter()
                        .map(ManagedTaskDecision::record)
                        .collect();
                    submit_managed_observation(
                        kernel,
                        total_tasks,
                        ordinal,
                        &task,
                        prepared.evidence,
                        record_units,
                        protocol_diagnostics,
                        ManagedTranslationTaskCheckpointState::CommitPreparationFailed,
                        Some(0),
                        diagnostic,
                    );
                    return Err(error);
                }
            };

            let accepted_ids = prepared
                .decisions
                .iter()
                .filter(|decision| decision.accepted.is_some())
                .count();
            let accepted_units = prepared
                .decisions
                .iter()
                .filter(|decision| decision.accepted.is_some())
                .map(|decision| task.expected[decision.id - 1].targets.len())
                .sum();
            let checkpoint_state = if accepted_ids == task.expected.len() {
                ManagedTranslationTaskCheckpointState::Complete
            } else if accepted_ids == 0 {
                ManagedTranslationTaskCheckpointState::Unavailable
            } else {
                ManagedTranslationTaskCheckpointState::Partial
            };
            let record_units = prepared
                .decisions
                .iter()
                .map(ManagedTaskDecision::record)
                .collect::<Vec<_>>();
            match kernel
                .store
                .checkpoint(
                    &state.baseline,
                    replacements,
                    ManagedTranslationCheckpointMode::Targeted,
                )
                .await
            {
                ManagedTranslationStoreCheckpoint::Applied(expected_baseline) => {
                    state.baseline = expected_baseline;
                    apply_task_results(&task, &prepared.decisions, &mut state.results);
                    submit_managed_observation(
                        kernel,
                        total_tasks,
                        ordinal,
                        &task,
                        prepared.evidence,
                        record_units,
                        protocol_diagnostics,
                        checkpoint_state,
                        Some(accepted_units),
                        None,
                    );
                    Ok(())
                }
                ManagedTranslationStoreCheckpoint::PreparationFailed(error) => {
                    let diagnostic = error.safe_diagnostic().cloned();
                    submit_managed_observation(
                        kernel,
                        total_tasks,
                        ordinal,
                        &task,
                        prepared.evidence,
                        record_units,
                        protocol_diagnostics,
                        ManagedTranslationTaskCheckpointState::CommitPreparationFailed,
                        Some(0),
                        diagnostic,
                    );
                    Err(error)
                }
                ManagedTranslationStoreCheckpoint::NotApplied(error)
                | ManagedTranslationStoreCheckpoint::Failed(error) => {
                    let diagnostic = error.safe_diagnostic().cloned();
                    submit_managed_observation(
                        kernel,
                        total_tasks,
                        ordinal,
                        &task,
                        prepared.evidence,
                        record_units,
                        protocol_diagnostics,
                        ManagedTranslationTaskCheckpointState::CommitNotApplied,
                        Some(0),
                        diagnostic,
                    );
                    Err(error)
                }
                ManagedTranslationStoreCheckpoint::OutcomeUnknown(error) => {
                    let diagnostic = error.safe_diagnostic().cloned();
                    submit_managed_observation(
                        kernel,
                        total_tasks,
                        ordinal,
                        &task,
                        prepared.evidence,
                        record_units,
                        protocol_diagnostics,
                        ManagedTranslationTaskCheckpointState::OutcomeUnknown,
                        None,
                        diagnostic,
                    );
                    Err(error)
                }
            }
        }
    }
}

fn baseline_unit<'a, S>(
    snapshot: &'a S,
    identity: &ManagedUnitIdentity,
) -> Result<&'a ManagedTranslationUnit, TrustedLuaHostCallError>
where
    S: ManagedTranslationSnapshotView,
{
    snapshot
        .collections()
        .iter()
        .find(|collection| collection.name() == identity.collection)
        .and_then(|collection| collection.unit(&identity.key))
        .ok_or_else(|| {
            managed_project_state_error(
                "snapshot_changed",
                format!(
                    "托管翻译基线缺少 unit：{}/{}",
                    identity.collection, identity.key
                ),
                DiagnosticFailureKind::StateMismatch,
                "reload_managed_translation_snapshot",
            )
        })
}

fn apply_task_results(
    task: &ManagedTaskBlock,
    decisions: &[ManagedTaskDecision],
    results: &mut HashMap<ManagedUnitIdentity, ManagedUnitResult>,
) {
    for decision in decisions {
        let expected = &task.expected[decision.id - 1];
        for target in &expected.targets {
            let result = match &decision.accepted {
                Some(content) => ManagedUnitResult::translated(content.clone()),
                None => ManagedUnitResult::unavailable(
                    decision.reason.as_deref().unwrap_or("unavailable"),
                    decision.details.clone(),
                ),
            };
            results.insert(target.clone(), result);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn submit_managed_observation<L, D, C, S, O>(
    kernel: &ManagedTranslationKernel<L, D, C, S, O>,
    total_tasks: usize,
    ordinal: usize,
    task: &ManagedTaskBlock,
    evidence: ManagedTranslationTaskEvidence,
    unit_results: Vec<ManagedTranslationTaskUnitResult>,
    protocol_diagnostics: Vec<ManagedTranslationProtocolDiagnostic>,
    checkpoint: ManagedTranslationTaskCheckpointState,
    confirmed_committed_units: Option<usize>,
    diagnostic: Option<SafeDiagnostic>,
) where
    L: LlmRequestExecutor,
    S: ManagedTranslationStore,
    O: ManagedTranslationObserver,
{
    kernel
        .observer
        .task_finished(ManagedTranslationTaskObservation {
            total_tasks,
            run_wide_ordinal: kernel
                .configuration
                .preceding_task_count
                .saturating_add(ordinal),
            collection: task.collection.clone(),
            messages: task.messages.clone(),
            identities: task
                .expected
                .iter()
                .map(|expected| ManagedTranslationTaskIdentity {
                    id: expected.id,
                    targets: expected.targets.clone(),
                })
                .collect(),
            evidence,
            unit_results,
            protocol_diagnostics,
            checkpoint,
            confirmed_committed_units,
            diagnostic,
        });
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

fn normalize_managed_host_error(
    source: TrustedLuaHostCallError,
    component: &'static str,
) -> TrustedLuaHostCallError {
    let mut diagnostic = source
        .safe_diagnostic()
        .cloned()
        .unwrap_or_else(|| managed_internal_diagnostic(component));
    diagnostic.stage = DiagnosticStage::Translate;
    diagnostic.impact = DiagnosticImpact::ProgressPreserved;
    source.with_safe_diagnostic(diagnostic)
}

fn managed_cpu_error<E>(
    kind: &'static str,
    source: CpuTaskExecutionError<E>,
    component: &'static str,
) -> TrustedLuaHostCallError
where
    E: Error + Send + Sync + 'static,
{
    let (kind, failure, action, message) = match &source {
        CpuTaskExecutionError::Cancelled => (
            "cancelled",
            DiagnosticFailureKind::LockCancelled,
            DiagnosticAction::Retry,
            "托管翻译 CPU 任务在等待执行许可时取消",
        ),
        CpuTaskExecutionError::Unavailable(_) => (
            kind,
            DiagnosticFailureKind::ExecutorClosed,
            DiagnosticAction::Retry,
            "托管翻译 CPU 执行器不可用",
        ),
        CpuTaskExecutionError::TaskPanicked => (
            kind,
            DiagnosticFailureKind::WorkerPanicked,
            DiagnosticAction::ReportBug,
            "托管翻译 CPU 工作任务发生 panic",
        ),
    };
    let diagnostic = SafeDiagnostic::new(
        DiagnosticCode::InternalOperation,
        DiagnosticStage::Translate,
        DiagnosticSubject::component(component),
        DiagnosticReason::failure(failure),
        DiagnosticImpact::ProgressPreserved,
        action,
    )
    .with_recovery(RecoveryFact::component(
        "committed_managed_translation_prefix_preserved",
    ));
    managed_host_source_error(kind, message, source, diagnostic)
}

fn managed_cancelled_error(message: impl Into<String>) -> TrustedLuaHostCallError {
    managed_host_error("cancelled", message, managed_cancelled_diagnostic())
}

fn managed_cancelled_diagnostic() -> SafeDiagnostic {
    SafeDiagnostic::new(
        DiagnosticCode::InternalOperation,
        DiagnosticStage::Translate,
        DiagnosticSubject::component("managed_translation"),
        DiagnosticReason::failure(DiagnosticFailureKind::LockCancelled),
        DiagnosticImpact::ProgressPreserved,
        DiagnosticAction::Retry,
    )
    .with_recovery(RecoveryFact::component(
        "committed_managed_translation_prefix_preserved",
    ))
}

fn managed_cancelled_source_error<E>(source: E) -> TrustedLuaHostCallError
where
    E: Error + Send + Sync + 'static,
{
    managed_host_source_error(
        "cancelled",
        "托管翻译模型请求在等待期间取消",
        source,
        SafeDiagnostic::new(
            DiagnosticCode::InternalOperation,
            DiagnosticStage::Translate,
            DiagnosticSubject::component("managed_model_request"),
            DiagnosticReason::failure(DiagnosticFailureKind::LockCancelled),
            DiagnosticImpact::ProgressPreserved,
            DiagnosticAction::Retry,
        )
        .with_recovery(RecoveryFact::component(
            "committed_managed_translation_prefix_preserved",
        )),
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

fn managed_ordered_error(
    source: OrderedExecutionError<TrustedLuaHostCallError>,
) -> TrustedLuaHostCallError {
    match source {
        OrderedExecutionError::Finalization { source, .. } => source,
        source @ OrderedExecutionError::IncompleteResultSequence { .. } => {
            managed_host_source_error(
                "ordered_execution_failed",
                "托管翻译有序执行器未能产生完整终结序列",
                source,
                managed_internal_diagnostic("managed_ordered_execution"),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use super::*;

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct TestSnapshot {
        collections: Vec<ManagedTranslationCollection>,
    }

    impl ManagedTranslationSnapshotView for TestSnapshot {
        fn collections(&self) -> &[ManagedTranslationCollection] {
            &self.collections
        }
    }

    #[derive(Clone)]
    struct FakeSemantics {
        source_language: &'static str,
        target_language: &'static str,
        system_prompt: &'static str,
    }

    impl Default for FakeSemantics {
        fn default() -> Self {
            Self {
                source_language: "ja",
                target_language: "zh-Hans",
                system_prompt: "system",
            }
        }
    }

    impl ManagedTranslationSemantics for FakeSemantics {
        fn engine_semantic_identity(&self) -> &str {
            "test_engine"
        }

        fn system_prompt(&self) -> &str {
            self.system_prompt
        }

        fn source_language(&self) -> &str {
            self.source_language
        }

        fn target_language(&self) -> &str {
            self.target_language
        }

        fn prepare_translation(
            &self,
            kind: &str,
            shape: ManagedTranslationShape,
            original: &ManagedTranslationContent,
            semantic_context: &str,
        ) -> Result<Arc<dyn ManagedPreparedTranslation>, TrustedLuaHostCallError> {
            let model_text = match original {
                ManagedTranslationContent::Scalar(value) => value.clone(),
                ManagedTranslationContent::Array(values) => values.join("\n"),
            };
            let mut hasher = Sha256FramedHasher::new(b"att.test.managed.prepared");
            hasher
                .frame(1, kind.as_bytes())
                .frame(2, shape.storage_name().as_bytes())
                .frame(3, original.canonical_json().as_bytes())
                .frame(4, semantic_context.as_bytes());
            Ok(Arc::new(FakePrepared {
                model_text,
                fingerprint: hasher.finish(),
                terms: Vec::new(),
            }))
        }
    }

    struct UnavailableSemantics;

    impl ManagedTranslationSemantics for UnavailableSemantics {
        fn engine_semantic_identity(&self) -> &str {
            "test_engine"
        }

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
            _kind: &str,
            _shape: ManagedTranslationShape,
            _original: &ManagedTranslationContent,
            _semantic_context: &str,
        ) -> Result<Arc<dyn ManagedPreparedTranslation>, TrustedLuaHostCallError> {
            Err(TrustedLuaHostCallError::new(
                "translation",
                "unavailable",
                "测试语义不可用",
                None,
                None,
            ))
        }
    }

    struct FakePrepared {
        model_text: String,
        fingerprint: Sha256Fingerprint,
        terms: Vec<ManagedTranslationTerm>,
    }

    impl ManagedPreparedTranslation for FakePrepared {
        fn status(&self) -> ManagedPreparedTranslationStatus {
            ManagedPreparedTranslationStatus::Active
        }

        fn model_text(&self) -> &str {
            &self.model_text
        }

        fn terms(&self) -> &[ManagedTranslationTerm] {
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
        ) -> Result<ManagedPreparedTranslationAcceptance, TrustedLuaHostCallError> {
            let state = accepted_state(&candidate);
            Ok(ManagedPreparedTranslationAcceptance::accepted(
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
        ManagedTranslationUnit::new(key, "test_kind", shape, original, context, None)
            .expect("测试托管 unit 应合法")
    }

    fn collection(
        name: &str,
        instruction: &str,
        units: Vec<ManagedTranslationUnit>,
    ) -> ManagedTranslationCollection {
        ManagedTranslationCollection::new(name, instruction, units).expect("测试集合应合法")
    }

    fn snapshot(collections: Vec<ManagedTranslationCollection>) -> TestSnapshot {
        TestSnapshot { collections }
    }

    fn prepare_units(
        snapshot: &TestSnapshot,
        semantics: &dyn ManagedTranslationSemantics,
    ) -> Vec<PreparedManagedUnit> {
        let system_prompt_fingerprint =
            managed_system_prompt_fingerprint(semantics.system_prompt());
        managed_preparation_inputs(snapshot)
            .into_iter()
            .map(|input| {
                prepare_managed_input(input, semantics, system_prompt_fingerprint)
                    .expect("测试 unit 应可准备")
            })
            .collect()
    }

    fn plan(
        snapshot: &TestSnapshot,
        semantics: &dyn ManagedTranslationSemantics,
        target_characters: usize,
    ) -> ManagedTranslationPlan {
        finalize_managed_plan(
            prepare_units(snapshot, semantics),
            semantics.system_prompt(),
            target_characters,
        )
        .expect("托管规划应成功")
    }

    fn one_expected(
        shape: ManagedTranslationShape,
        original: ManagedTranslationContent,
    ) -> ManagedTaskExpected {
        let snapshot = snapshot(vec![collection(
            "one",
            "",
            vec![unit("key", shape, original, "")],
        )]);
        let prepared = prepare_units(&snapshot, &FakeSemantics::default())
            .into_iter()
            .next()
            .expect("测试 unit 应存在");
        ManagedTaskExpected {
            id: 1,
            representative: prepared,
            targets: vec![ManagedUnitIdentity::new("one", "key")],
        }
    }

    fn response(content: &str) -> LlmResponse {
        LlmResponse::new(content, LlmFinishReason::Stop, None, None, None)
    }

    fn apply_replacements(
        mut snapshot: TestSnapshot,
        replacements: &[ManagedTranslationReplacement],
    ) -> TestSnapshot {
        for replacement in replacements.iter().cloned() {
            let (collection_name, key, replacement) = replacement.into_parts();
            let collection = snapshot
                .collections
                .iter_mut()
                .find(|collection| collection.name() == collection_name)
                .expect("replacement collection 应存在");
            let unit = collection
                .units
                .iter_mut()
                .find(|unit| unit.key() == key)
                .expect("replacement unit 应存在");
            unit.translation = replacement;
        }
        snapshot
    }

    #[test]
    fn candidate_session_freezes_families_and_produces_guarded_replacements() {
        let baseline = snapshot(vec![
            collection("first", "同一指令", vec![scalar_unit("one", "")]),
            collection("second", "同一指令", vec![scalar_unit("two", "")]),
        ]);
        let session =
            ManagedTranslationCandidateSession::open(baseline.clone(), &FakeSemantics::default())
                .expect("人工候选会话应可打开");
        let units = session.units().expect("冻结单元应可读取");
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].collection(), "first");
        assert_eq!(units[0].key(), "one");
        assert_eq!(units[0].kind(), "test_kind");
        assert_eq!(units[0].shape(), ManagedTranslationShape::Single);
        assert_eq!(
            units[0].original(),
            &ManagedTranslationContent::scalar("原文")
        );
        assert_eq!(units[0].context(), "");
        assert_eq!(units[0].metadata(), None);
        assert_eq!(units[0].translation(), None);
        assert_eq!(
            units[0].model_content(),
            &ManagedTranslationContent::scalar("原文")
        );
        assert_eq!(
            units[0].status(),
            ManagedTranslationCandidateUnitStatus::Missing
        );
        assert_eq!(units[0].family_size(), 2);
        assert_eq!(
            session
                .get("second", "two")
                .expect("get 应可读取")
                .expect("目标单元应存在")
                .handle(),
            units[1].handle()
        );

        let conflicting = session
            .prepare_acceptance(vec![
                ManagedTranslationCandidateRequest::new(
                    units[0].handle(),
                    ManagedTranslationContent::scalar("译文甲"),
                    false,
                ),
                ManagedTranslationCandidateRequest::new(
                    units[1].handle(),
                    ManagedTranslationContent::scalar("译文乙"),
                    false,
                ),
            ])
            .expect("family 候选冲突是普通拒绝");
        assert!(conflicting.results().iter().all(|result| {
            matches!(
                result,
                ManagedTranslationCandidateAcceptance::Rejected { reason, details: None }
                    if reason == "conflicting_candidate"
            )
        }));
        assert!(conflicting.replacements().is_empty());

        let prepared = session
            .prepare_acceptance(vec![ManagedTranslationCandidateRequest::new(
                units[0].handle(),
                ManagedTranslationContent::scalar("译文"),
                false,
            )])
            .expect("合法候选应形成 CAS 计划");
        assert_eq!(prepared.baseline(), &baseline);
        assert_eq!(prepared.replacements().len(), 2);
        assert!(matches!(
            &prepared.results()[0],
            ManagedTranslationCandidateAcceptance::Accepted {
                content,
                changed_units: 2,
            } if content == &ManagedTranslationContent::scalar("译文")
        ));

        let committed = apply_replacements(baseline, prepared.replacements());
        session
            .apply_committed(&prepared, committed)
            .expect("确认提交后应推进会话");
        assert!(session.units().expect("应可重读").iter().all(|unit| {
            unit.status() == ManagedTranslationCandidateUnitStatus::Current
                && unit.translation() == Some(&ManagedTranslationContent::scalar("译文"))
        }));

        let replacement_required = session
            .prepare_acceptance(vec![ManagedTranslationCandidateRequest::new(
                units[1].handle(),
                ManagedTranslationContent::scalar("新译文"),
                false,
            )])
            .expect("Current 覆盖缺少授权是普通拒绝");
        assert!(matches!(
            &replacement_required.results()[0],
            ManagedTranslationCandidateAcceptance::Rejected { reason, details: None }
                if reason == "current_replacement_required"
        ));
        assert!(replacement_required.replacements().is_empty());

        let idempotent = session
            .prepare_acceptance(vec![ManagedTranslationCandidateRequest::new(
                units[0].handle(),
                ManagedTranslationContent::scalar("译文"),
                false,
            )])
            .expect("与 Current 相同的候选应幂等成功");
        assert!(matches!(
            &idempotent.results()[0],
            ManagedTranslationCandidateAcceptance::Accepted {
                content,
                changed_units: 0,
            } if content == &ManagedTranslationContent::scalar("译文")
        ));
        assert!(idempotent.replacements().is_empty());

        let replacement = session
            .prepare_acceptance(vec![ManagedTranslationCandidateRequest::new(
                units[1].handle(),
                ManagedTranslationContent::scalar("新译文"),
                true,
            )])
            .expect("显式 Current 替换应形成整个 family 的 CAS 计划");
        assert!(matches!(
            &replacement.results()[0],
            ManagedTranslationCandidateAcceptance::Accepted {
                content,
                changed_units: 2,
            } if content == &ManagedTranslationContent::scalar("新译文")
        ));
        assert_eq!(replacement.replacements().len(), 2);
    }

    #[test]
    fn candidate_session_projects_semantic_failures_as_unavailable_units() {
        let session = ManagedTranslationCandidateSession::open(
            snapshot(vec![collection(
                "unavailable",
                "",
                vec![scalar_unit("unit", "")],
            )]),
            &UnavailableSemantics,
        )
        .expect("单元语义不可用不应阻止打开其余冻结会话");
        let units = session.units().expect("单元应可读取");
        assert_eq!(
            units[0].status(),
            ManagedTranslationCandidateUnitStatus::Unavailable
        );

        let prepared = session
            .prepare_acceptance(vec![ManagedTranslationCandidateRequest::new(
                units[0].handle(),
                ManagedTranslationContent::scalar("译文"),
                false,
            )])
            .expect("Unavailable 是普通逐项拒绝");
        assert!(matches!(
            &prepared.results()[0],
            ManagedTranslationCandidateAcceptance::Rejected {
                reason,
                details: Some(ManagedTranslationCandidateRejectionDetails::Unavailable { detail }),
            } if reason == "unavailable" && detail.contains("测试语义不可用")
        ));
        assert!(prepared.replacements().is_empty());
    }

    #[test]
    fn candidate_session_uses_current_seed_to_fill_missing_and_stale_family_members() {
        let baseline = snapshot(vec![collection(
            "mixed",
            "同一指令",
            vec![
                scalar_unit("current", ""),
                scalar_unit("missing", ""),
                scalar_unit("stale", ""),
            ],
        )]);
        let seed_session =
            ManagedTranslationCandidateSession::open(baseline.clone(), &FakeSemantics::default())
                .expect("种子会话应可打开");
        let seed_unit = seed_session.units().expect("种子单元应可读取")[0].clone();
        let seeded = seed_session
            .prepare_acceptance(vec![ManagedTranslationCandidateRequest::new(
                seed_unit.handle(),
                ManagedTranslationContent::scalar("译文"),
                false,
            )])
            .expect("种子候选应形成完整 family replacement");

        let mut mixed = apply_replacements(baseline, &seeded.replacements()[..1]);
        let stale = mixed.collections[0]
            .units
            .iter_mut()
            .find(|unit| unit.key() == "stale")
            .expect("stale 测试单元应存在");
        stale.translation = Some(ManagedTranslationPair::new_trusted(
            ManagedTranslationContent::scalar("旧译文"),
            Sha256Fingerprint::from_bytes([0xA5; 32]),
        ));

        let session =
            ManagedTranslationCandidateSession::open(mixed.clone(), &FakeSemantics::default())
                .expect("混合状态会话应可打开");
        let units = session.units().expect("混合状态单元应可读取");
        assert_eq!(
            units
                .iter()
                .map(ManagedTranslationCandidateUnit::status)
                .collect::<Vec<_>>(),
            [
                ManagedTranslationCandidateUnitStatus::Current,
                ManagedTranslationCandidateUnitStatus::Missing,
                ManagedTranslationCandidateUnitStatus::Stale,
            ]
        );

        let prepared = session
            .prepare_acceptance(vec![ManagedTranslationCandidateRequest::new(
                units[0].handle(),
                ManagedTranslationContent::scalar("译文"),
                false,
            )])
            .expect("Current 相同候选应补齐其余 family 成员");
        assert!(matches!(
            &prepared.results()[0],
            ManagedTranslationCandidateAcceptance::Accepted {
                changed_units: 2,
                ..
            }
        ));
        assert_eq!(prepared.replacements().len(), 2);
        let committed = apply_replacements(mixed, prepared.replacements());
        session
            .apply_committed(&prepared, committed)
            .expect("补齐提交后会话应推进");
        assert!(session.units().expect("应可重读").iter().all(|unit| {
            unit.status() == ManagedTranslationCandidateUnitStatus::Current
                && unit.translation() == Some(&ManagedTranslationContent::scalar("译文"))
        }));
    }

    #[test]
    fn globally_deduplicates_across_collections_without_crossing_task_boundary() {
        let snapshot = snapshot(vec![
            collection("first", "同一指令", vec![scalar_unit("first-key", "")]),
            collection(
                "second",
                "同一指令",
                vec![
                    scalar_unit("duplicate-key", ""),
                    scalar_unit("unique-key", "不同上下文"),
                ],
            ),
        ]);

        let plan = plan(&snapshot, &FakeSemantics::default(), usize::MAX);

        assert_eq!(plan.tasks.len(), 2);
        assert_eq!(plan.tasks[0].collection, "first");
        assert_eq!(plan.tasks[0].expected[0].id, 1);
        assert_eq!(
            plan.tasks[0].expected[0].representative.identity,
            ManagedUnitIdentity::new("first", "first-key")
        );
        assert_eq!(
            plan.tasks[0].expected[0].targets,
            [
                ManagedUnitIdentity::new("first", "first-key"),
                ManagedUnitIdentity::new("second", "duplicate-key"),
            ]
        );
        assert_eq!(plan.tasks[1].collection, "second");
        assert_eq!(plan.tasks[1].expected[0].id, 1);
        assert_eq!(
            plan.tasks[1].expected[0].targets,
            [ManagedUnitIdentity::new("second", "unique-key")]
        );
    }

    #[test]
    fn semantic_fields_and_languages_keep_distinct_dedup_and_state_domains() {
        let snapshot = snapshot(vec![
            collection(
                "base",
                "指令甲",
                vec![
                    scalar_unit("base", ""),
                    scalar_unit("different-context", "场景乙"),
                    unit(
                        "different-shape",
                        ManagedTranslationShape::Lines,
                        ManagedTranslationContent::array(vec!["原文".to_owned()]),
                        "",
                    ),
                ],
            ),
            collection(
                "different-instruction",
                "指令乙",
                vec![scalar_unit("different-instruction", "")],
            ),
        ]);
        let base_semantics = FakeSemantics::default();
        let plan = plan(&snapshot, &base_semantics, usize::MAX);
        let expected = plan
            .tasks
            .iter()
            .flat_map(|task| &task.expected)
            .collect::<Vec<_>>();
        assert_eq!(expected.len(), 4);
        assert!(expected.iter().all(|expected| expected.targets.len() == 1));

        let base = prepare_units(&snapshot, &base_semantics);
        let other_source = prepare_units(
            &snapshot,
            &FakeSemantics {
                source_language: "en",
                ..FakeSemantics::default()
            },
        );
        let other_target = prepare_units(
            &snapshot,
            &FakeSemantics {
                target_language: "en",
                ..FakeSemantics::default()
            },
        );
        let other_prompt = prepare_units(
            &snapshot,
            &FakeSemantics {
                system_prompt: "managed system v2",
                ..FakeSemantics::default()
            },
        );
        assert_ne!(base[0].state_context.0, other_source[0].state_context.0);
        assert_ne!(base[0].state_context.0, other_target[0].state_context.0);
        assert_ne!(base[0].state_context.0, other_prompt[0].state_context.0);
    }

    #[test]
    fn task_messages_use_per_block_ids_and_exclude_persistent_identities() {
        let snapshot = snapshot(vec![collection(
            "secret_collection",
            "保持简洁。",
            vec![
                scalar_unit("persistent:key:one", ""),
                scalar_unit("persistent:key:two", "菜单"),
            ],
        )]);

        let plan = plan(&snapshot, &FakeSemantics::default(), usize::MAX);
        let task = &plan.tasks[0];
        assert_eq!(task.expected[0].id, 1);
        assert_eq!(task.expected[1].id, 2);
        assert_eq!(task.messages[0].role(), ChatMessageRole::System);
        assert_eq!(task.messages[0].content(), "system");
        assert_eq!(
            task.messages[1].content(),
            "Instruction:\n\n保持简洁。\n\nText [1] (single line): 原文\n\nContext:\n\n> 菜单\n\nText [2] (single line): 原文\n"
        );
        assert!(!task.messages[1].content().contains("secret_collection"));
        assert!(!task.messages[1].content().contains("persistent:key"));
    }

    #[test]
    fn managed_reflow_uses_its_owned_wire_marker_and_protocol_fragment() {
        let snapshot = snapshot(vec![collection(
            "reflow",
            "",
            vec![unit(
                "body",
                ManagedTranslationShape::Reflow,
                ManagedTranslationContent::scalar("第一行\n第二行"),
                "",
            )],
        )]);

        let plan = plan(&snapshot, &FakeSemantics::default(), usize::MAX);
        assert!(
            plan.tasks[0].messages[1]
                .content()
                .contains("Text [1] (single string, LF allowed):\n\n> 第一行\n> 第二行")
        );
        assert!(
            managed_translation_system_prompt_fragment()
                .contains("array containing exactly one non-blank JSON string")
        );
        assert!(
            managed_translation_system_prompt_fragment()
                .contains("must be encoded as `\\n` in the JSON text")
        );
        assert!(managed_translation_system_prompt_fragment().contains("CR and NUL are forbidden"));
        assert!(
            managed_translation_system_prompt_fragment()
                .contains("only between LF-delimited segments within that same ID")
        );
        assert!(
            managed_translation_system_prompt_fragment()
                .contains("`<why>` analysis must cover this marker's one-element shape")
        );
    }

    #[test]
    fn packing_respects_collection_boundary_resets_ids_and_keeps_oversized_unit_atomic() {
        let semantics = FakeSemantics::default();
        let one_unit = snapshot(vec![collection(
            "first",
            "装箱",
            vec![scalar_unit("one", "上下文一")],
        )]);
        let one_unit_characters = plan(&one_unit, &semantics, usize::MAX).tasks[0].messages[1]
            .content()
            .chars()
            .count();
        let packed = snapshot(vec![
            collection(
                "first",
                "装箱",
                vec![
                    scalar_unit("one", "上下文一"),
                    scalar_unit("two", "上下文二"),
                ],
            ),
            collection("second", "装箱", vec![scalar_unit("three", "上下文三")]),
        ]);

        let packed_plan = plan(&packed, &semantics, one_unit_characters);

        assert_eq!(packed_plan.tasks.len(), 3);
        assert_eq!(
            packed_plan
                .tasks
                .iter()
                .map(|task| task.collection.as_str())
                .collect::<Vec<_>>(),
            ["first", "first", "second"]
        );
        assert!(
            packed_plan
                .tasks
                .iter()
                .all(|task| task.expected.len() == 1 && task.expected[0].id == 1)
        );

        let oversized_text = "完整原文".repeat(128);
        let oversized = snapshot(vec![collection(
            "oversized",
            "",
            vec![unit(
                "large",
                ManagedTranslationShape::Single,
                ManagedTranslationContent::scalar(&oversized_text),
                "",
            )],
        )]);
        let oversized_plan = plan(&oversized, &semantics, 1);
        assert_eq!(oversized_plan.tasks.len(), 1);
        assert_eq!(oversized_plan.tasks[0].expected.len(), 1);
        assert_eq!(oversized_plan.tasks[0].expected[0].id, 1);
        assert!(
            oversized_plan.tasks[0].messages[1]
                .content()
                .contains(&oversized_text)
        );
    }

    #[test]
    fn all_shapes_enforce_atomic_result_contracts() {
        let reflow = one_expected(
            ManagedTranslationShape::Reflow,
            ManagedTranslationContent::scalar("第一行\n第二行"),
        );
        let accepted =
            accept_managed_candidate(&reflow, &["译文一\n译文二".to_owned()]).expect("验收应执行");
        assert_eq!(
            accepted.accepted,
            Some(ManagedTranslationContent::scalar("译文一\n译文二"))
        );
        let rejected =
            accept_managed_candidate(&reflow, &["译文一".to_owned(), "译文二".to_owned()])
                .expect("形状拒绝是普通结果");
        assert_eq!(rejected.reason.as_deref(), Some("item_count_mismatch"));

        let cases = [
            (
                one_expected(
                    ManagedTranslationShape::Single,
                    ManagedTranslationContent::scalar("原文"),
                ),
                vec!["甲".to_owned(), "乙".to_owned()],
                "item_count_mismatch",
            ),
            (
                one_expected(
                    ManagedTranslationShape::Lines,
                    ManagedTranslationContent::array(vec!["第一行".to_owned(), "".to_owned()]),
                ),
                vec!["译文".to_owned()],
                "item_count_mismatch",
            ),
            (
                one_expected(
                    ManagedTranslationShape::Items,
                    ManagedTranslationContent::array(vec![
                        "项目一".to_owned(),
                        "项目二".to_owned(),
                    ]),
                ),
                vec!["译文一".to_owned(), " ".to_owned()],
                "blank_item",
            ),
        ];
        for (expected, values, reason) in cases {
            let decision =
                accept_managed_candidate(&expected, &values).expect("形状拒绝是普通结果");
            assert_eq!(decision.accepted, None);
            assert_eq!(decision.reason.as_deref(), Some(reason));
        }
    }

    #[test]
    fn reflow_json_wire_accepts_one_lf_string_and_rejects_other_shapes() {
        let snapshot = snapshot(vec![collection(
            "reflow",
            "",
            vec![unit(
                "body",
                ManagedTranslationShape::Reflow,
                ManagedTranslationContent::scalar("第一行\n第二行"),
                "",
            )],
        )]);
        let task = plan(&snapshot, &FakeSemantics::default(), usize::MAX)
            .tasks
            .into_iter()
            .next()
            .expect("reflow 应形成一个任务");

        let accepted = process_managed_response(
            &task,
            response(r#"{"1":["译文一\n译文二"]}"#),
            TranslationResponseEnvelope::JsonOnly,
            false,
        )
        .expect("单元素 JSON 字符串中的 LF 应可解码并验收");
        assert_eq!(
            accepted.decisions[0].accepted,
            Some(ManagedTranslationContent::scalar("译文一\n译文二"))
        );

        let multiple = process_managed_response(
            &task,
            response(r#"{"1":["译文一","译文二"]}"#),
            TranslationResponseEnvelope::JsonOnly,
            false,
        )
        .expect("多元素是逐 ID 普通拒绝");
        assert_eq!(
            multiple.decisions[0].reason.as_deref(),
            Some("item_count_mismatch")
        );

        for response_body in [r#"{"1":["译文一\r译文二"]}"#, r#"{"1":["译文\u0000"]}"#] {
            let invalid = process_managed_response(
                &task,
                response(response_body),
                TranslationResponseEnvelope::JsonOnly,
                false,
            )
            .expect("CR/NUL 是逐 ID 普通拒绝");
            assert_eq!(
                invalid.decisions[0].reason.as_deref(),
                Some("invalid_line_text")
            );
        }
    }

    #[test]
    fn response_processing_preserves_valid_ids_and_enforces_envelope() {
        let snapshot = snapshot(vec![collection(
            "many",
            "",
            vec![
                scalar_unit("one", "one"),
                scalar_unit("two", "two"),
                scalar_unit("three", "three"),
            ],
        )]);
        let task = plan(&snapshot, &FakeSemantics::default(), usize::MAX)
            .tasks
            .into_iter()
            .next()
            .expect("应形成任务");
        let processed = process_managed_response(
            &task,
            response(r#"{"1":["甲"],"99":["未知"],"2":["乙"],"2":["重复"]}"#),
            TranslationResponseEnvelope::JsonOnly,
            false,
        )
        .expect("逐 ID 验收应成功");
        assert_eq!(
            processed.decisions[0].accepted,
            Some(ManagedTranslationContent::scalar("甲"))
        );
        assert_eq!(
            processed.decisions[1].reason.as_deref(),
            Some("duplicate_id")
        );
        assert_eq!(processed.decisions[2].reason.as_deref(), Some("missing_id"));

        let thinking = process_managed_response(
            &task,
            response("<why>逐项检查</why>\n{\"1\":[\"乙\"],\"2\":[\"丙\"],\"3\":[\"丁\"]}"),
            TranslationResponseEnvelope::ThinkingThenJson,
            true,
        )
        .expect("thinking 信封应可验收");
        assert_eq!(
            thinking.decisions[0].accepted,
            Some(ManagedTranslationContent::scalar("乙"))
        );
        assert!(matches!(
            thinking.response_record,
            Some(ManagedTranslationResponseRecord::Parsed {
                thinking: Some(ref why),
                ..
            }) if why == "逐项检查"
        ));

        let wrong_envelope = process_managed_response(
            &task,
            response("<why>不允许</why>\n{\"1\":[\"甲\"]}"),
            TranslationResponseEnvelope::JsonOnly,
            false,
        )
        .expect("信封错误应成为普通不可用结果");
        assert!(
            wrong_envelope
                .decisions
                .iter()
                .all(|decision| decision.reason.as_deref() == Some("model_response_unusable"))
        );
    }

    #[test]
    fn conflicting_current_family_fails_before_task_creation() {
        let semantics = FakeSemantics::default();
        let mut snapshot = snapshot(vec![
            collection("first", "同一指令", vec![scalar_unit("one", "")]),
            collection("second", "同一指令", vec![scalar_unit("two", "")]),
        ]);
        let prepared = prepare_units(&snapshot, &semantics);
        assert_eq!(prepared[0].state_context.0, prepared[1].state_context.0);

        for (index, translation) in ["译文甲", "译文乙"].into_iter().enumerate() {
            let content = ManagedTranslationContent::scalar(translation);
            let pair = snapshot.collections[index].units[0]
                .translation_pair(
                    content.clone(),
                    prepared[index].state_context.finish(&content),
                )
                .expect("测试译文应合法");
            snapshot.collections[index].units[0].translation = Some(pair);
        }

        let error = match finalize_managed_plan(
            prepare_units(&snapshot, &semantics),
            semantics.system_prompt(),
            usize::MAX,
        ) {
            Ok(_) => panic!("冲突 Current 必须在请求前失败"),
            Err(error) => error,
        };
        assert_eq!(error.domain(), "translations");
        assert_eq!(error.kind(), "current_conflict");
        assert_eq!(
            error
                .safe_diagnostic()
                .expect("Current 冲突必须有诊断")
                .reason,
            DiagnosticReason::failure(DiagnosticFailureKind::ConflictingValues)
        );
    }

    #[derive(Clone, Copy, Debug)]
    struct TestExecutionError;

    impl fmt::Display for TestExecutionError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("test execution error")
        }
    }

    impl Error for TestExecutionError {}

    #[test]
    fn cpu_terminal_failures_preserve_typed_progress_diagnostics() {
        let cases = [
            (
                managed_cpu_error(
                    "planning_failed",
                    CpuTaskExecutionError::<TestExecutionError>::Cancelled,
                    "managed_planning",
                ),
                "cancelled",
                DiagnosticFailureKind::LockCancelled,
                DiagnosticAction::Retry,
            ),
            (
                managed_cpu_error(
                    "planning_failed",
                    CpuTaskExecutionError::Unavailable(TestExecutionError),
                    "managed_planning",
                ),
                "planning_failed",
                DiagnosticFailureKind::ExecutorClosed,
                DiagnosticAction::Retry,
            ),
            (
                managed_cpu_error(
                    "response_processing_failed",
                    CpuTaskExecutionError::<TestExecutionError>::TaskPanicked,
                    "managed_response_processing",
                ),
                "response_processing_failed",
                DiagnosticFailureKind::WorkerPanicked,
                DiagnosticAction::ReportBug,
            ),
        ];

        for (error, kind, failure, action) in cases {
            assert_eq!(error.kind(), kind);
            let diagnostic = error.safe_diagnostic().expect("CPU 终态必须有诊断");
            assert_eq!(diagnostic.code, DiagnosticCode::InternalOperation);
            assert_eq!(diagnostic.stage, DiagnosticStage::Translate);
            assert_eq!(diagnostic.reason, DiagnosticReason::failure(failure));
            assert_eq!(diagnostic.impact, DiagnosticImpact::ProgressPreserved);
            assert_eq!(diagnostic.action, action);
            assert!(diagnostic.recovery.contains(&RecoveryFact::component(
                "committed_managed_translation_prefix_preserved"
            )));
        }
    }

    #[test]
    fn response_semantic_error_keeps_reason_while_normalizing_stage_and_impact() {
        let original = SafeDiagnostic::new(
            DiagnosticCode::ProjectState,
            DiagnosticStage::Lua,
            DiagnosticSubject::component("prepared_translation"),
            DiagnosticReason::failure(DiagnosticFailureKind::InvalidValue),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixInput,
        )
        .with_recovery(RecoveryFact::component("semantic_contract"));
        let error = TrustedLuaHostCallError::new(
            "translation",
            "acceptance_failed",
            "验收失败",
            None,
            None,
        )
        .with_safe_diagnostic(original.clone());

        let normalized = normalize_managed_host_error(error, "managed_response_acceptance");
        let diagnostic = normalized
            .safe_diagnostic()
            .expect("响应语义错误必须保留安全诊断");
        assert_eq!(diagnostic.code, original.code);
        assert_eq!(diagnostic.subject, original.subject);
        assert_eq!(diagnostic.reason, original.reason);
        assert_eq!(diagnostic.action, original.action);
        assert_eq!(diagnostic.recovery, original.recovery);
        assert_eq!(diagnostic.stage, DiagnosticStage::Translate);
        assert_eq!(diagnostic.impact, DiagnosticImpact::ProgressPreserved);
    }
}
