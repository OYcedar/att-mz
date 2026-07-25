//! WriteBack 全部非根能力使用根替身的纵向贯通测试。

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::asset_reader::RpgMakerStandardWriteBackAssetReadingService;
use super::lua::LuaWriteBackService;
use super::publisher::StandardWriteBackPublishingService;
use super::rewriter::RpgMakerWriteBackDocumentRewritingService;
use super::standard::{ConservativeRpgMakerWriteBackTextLayouter, StandardWriteBackService};
use super::{
    WriteBackInput, WriteBackLog, WriteBackLogEvent, WriteBackLogPublicationOutcome,
    WriteBackOutput, WriteBackProgressPhase, WriteBackService,
};
use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, SafeDiagnostic, SafeDiagnosticSource,
};
use crate::execution::OperationCompletion;
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::fingerprint::Sha256Fingerprint;
use crate::llm::{
    ChatMessage, LlmFinishReason, LlmRequestDiagnosticSource, LlmRequestError, LlmRequestExecutor,
    LlmResponse,
};
use crate::progress::{ProgressAmount, ProgressObserver, ProgressSnapshot};
use crate::rpg_maker::RpgMakerLayout;
use crate::rpg_maker::extract::document::{
    RpgMakerDocumentReadingConfig, RpgMakerProjectDocumentReadingService,
};
use crate::rpg_maker::location_codec::RpgMakerLocationCodec;
use crate::rpg_maker::lua::hosting::TrustedLuaExecutionHostingService;
use crate::rpg_maker::lua::runtime::{
    OwnedLuaProgram, TrustedLuaExecutionHandle, TrustedLuaPhaseBindings, TrustedLuaRuntimeBindings,
    TrustedLuaRuntimeExecutionError, TrustedLuaRuntimeExecutionReport, TrustedLuaRuntimeExecutor,
};
use crate::rpg_maker::lua::{LuaPhase, LuaProjectContext};
use crate::rpg_maker::project::ExistingProjectOpeningService;
use crate::rpg_maker::project_database::ProjectDatabaseRecordReadingService;
use crate::rpg_maker::project_lease::{
    ProjectCommandLease, ProjectCommandLeaseError, ProjectCommandLeaseProvider,
};
use crate::rpg_maker::text::{
    RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, StandardDataFile, TextGroupKind,
};
use crate::rpg_maker::{ProjectName, SelectedLua};
use crate::storage::file_system::{
    BoundScopedDirectory, DirectoryDiscardError, DirectoryEntry, DirectoryEntryKind,
    DirectoryLister, DirectoryPrepareError, DirectoryPublishError, DirectoryPublishIntent,
    DirectoryStageRequest, DirectoryTreeFingerprintError, DirectoryTreeFingerprintRequest,
    DirectoryTreeFingerprinter, ExistingDirectoryResolver, FileReader, ListDirectoryError,
    ReadFile, ReadFileError, RecoverableDirectoryPublisher, ResolveDirectoryError,
    ScopedDirectoryBindError, ScopedDirectoryEditError, ScopedDirectoryEditor,
    ScopedDirectoryEntry, ScopedDirectoryEntryKind, ScopedDirectoryPath, ScopedDirectoryScope,
    StagedDirectory,
};
use crate::storage::sqlite::{
    QueryExistingDatabaseError, SqliteCommand, SqliteQuery, SqliteQueryExecutor, SqliteRow,
    SqliteValue,
};
use crate::storage::sqlite_session::{
    OpenSqliteInteractiveSessionError, OpenedSqliteInteractiveSession,
    SqliteInteractiveSessionError, SqliteInteractiveSessionFactory,
    SqliteInteractiveSessionFinalization, SqliteInteractiveSessionFinalizationError,
    SqliteInteractiveSessionFinalizer, SqliteInteractiveSessionOperations,
};

const PROJECTS_ROOT: &str = "C:/att/projects";
const PROJECT_NAME: &str = "demo";
const LUA_SCRIPT: &str = "C:/att/scripts/write_back.lua";
const DIALOGUE_DEFINITION_JSON: &str = r#"{"rules":[]}"#;

#[derive(Clone, Default)]
struct RecordingProgress(Arc<Mutex<Vec<ProgressSnapshot<WriteBackProgressPhase>>>>);

impl ProgressObserver<WriteBackProgressPhase> for RecordingProgress {
    fn observe(&self, snapshot: ProgressSnapshot<WriteBackProgressPhase>) {
        self.0.lock().expect("进度记录锁不应中毒").push(snapshot);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TestError(&'static str);

impl fmt::Display for TestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for TestError {}

impl SafeDiagnosticSource for TestError {
    fn safe_diagnostic_source(
        &self,
        stage: DiagnosticStage,
        impact: DiagnosticImpact,
        action: DiagnosticAction,
    ) -> SafeDiagnostic {
        SafeDiagnostic::new(
            DiagnosticCode::InternalOperation,
            stage,
            DiagnosticSubject::component("write-back full-tree test root"),
            DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
            impact,
            action,
        )
    }
}

impl LlmRequestDiagnosticSource for TestError {
    fn request_diagnostic(
        &self,
        retry_after: Option<std::time::Duration>,
        impact: DiagnosticImpact,
    ) -> SafeDiagnostic {
        SafeDiagnostic::new(
            DiagnosticCode::ModelRequest,
            DiagnosticStage::ModelRequest,
            DiagnosticSubject::component("write-back full-tree test LLM"),
            DiagnosticReason::Http {
                status: Some(503),
                retry_after_seconds: retry_after.map(|value| value.as_secs()),
                provider_code: Some("unavailable".to_owned()),
                provider_type: Some("test".to_owned()),
            },
            impact,
            DiagnosticAction::CheckModelService,
        )
    }
}

#[derive(Clone, Copy)]
struct FakeProjectLease;

impl ProjectCommandLeaseProvider for FakeProjectLease {
    type Error = TestError;
    type LeaseState = ();

    async fn acquire(
        &self,
        _: &ProjectName,
    ) -> Result<ProjectCommandLease<Self::LeaseState>, ProjectCommandLeaseError<Self::Error>> {
        Ok(ProjectCommandLease::for_test(()))
    }
}

#[derive(Clone, Default)]
struct RecordingSqliteQuery {
    calls: Arc<Mutex<Vec<(PathBuf, SqliteQuery)>>>,
}

impl SqliteQueryExecutor for RecordingSqliteQuery {
    type Error = TestError;

    async fn query_existing_database(
        &self,
        path: PathBuf,
        query: SqliteQuery,
    ) -> Result<Vec<SqliteRow>, QueryExistingDatabaseError<Self::Error>> {
        self.calls
            .lock()
            .expect("SQLite 记录锁不应中毒")
            .push((path, query.clone()));
        if query.statement().contains("FROM metadata") {
            Ok(vec![metadata_row()])
        } else {
            let rows = write_back_snapshot_rows();
            if query
                .statement()
                .contains("FROM standard_asset_owner_state")
            {
                Ok(rows_for_requested_owner(rows.owners, &query))
            } else if query.statement().contains("standard_text_unit AS unit") {
                Ok(rows_for_requested_owner(rows.units, &query))
            } else if query.statement().contains("FROM standard_text_group") {
                Ok(rows_for_requested_owner(rows.groups, &query))
            } else if query.statement().contains("FROM standard_mutation_claim") {
                Ok(rows_for_requested_owner(rows.claims, &query))
            } else {
                Err(QueryExistingDatabaseError::QueryFailed(TestError(
                    "意外的全树测试查询",
                )))
            }
        }
    }

    async fn query_existing_database_snapshot(
        &self,
        path: PathBuf,
        queries: Vec<SqliteQuery>,
    ) -> Result<Vec<Vec<SqliteRow>>, QueryExistingDatabaseError<Self::Error>> {
        let mut results = Vec::with_capacity(queries.len());
        for query in queries {
            results.push(self.query_existing_database(path.clone(), query).await?);
        }
        Ok(results)
    }
}

#[derive(Clone, Default)]
struct RecordingDirectoryResolver {
    calls: Arc<Mutex<Vec<PathBuf>>>,
}

#[derive(Clone, Copy)]
struct MatchingDirectoryTreeFingerprinter;

impl DirectoryTreeFingerprinter for MatchingDirectoryTreeFingerprinter {
    type Error = TestError;

    async fn fingerprint_directory_tree(
        &self,
        _: DirectoryTreeFingerprintRequest,
    ) -> Result<Sha256Fingerprint, DirectoryTreeFingerprintError<Self::Error>> {
        Ok(Sha256Fingerprint::from_slice(&[7; 32]).expect("固定测试指纹应合法"))
    }
}

impl ExistingDirectoryResolver for RecordingDirectoryResolver {
    type Error = TestError;

    async fn resolve_existing_directory(
        &self,
        path: PathBuf,
    ) -> Result<PathBuf, ResolveDirectoryError<Self::Error>> {
        self.calls
            .lock()
            .expect("目录记录锁不应中毒")
            .push(path.clone());
        Ok(path)
    }
}

#[derive(Clone)]
struct RecordingFileReader {
    files: Arc<BTreeMap<PathBuf, Vec<u8>>>,
    calls: Arc<Mutex<Vec<PathBuf>>>,
}

impl FileReader for RecordingFileReader {
    type Error = TestError;

    async fn read_file(&self, path: PathBuf) -> Result<ReadFile, ReadFileError<Self::Error>> {
        self.calls
            .lock()
            .expect("文件读取记录锁不应中毒")
            .push(path.clone());
        let Some(bytes) = self.files.get(&path) else {
            return Err(ReadFileError::NotFound { path });
        };
        Ok(ReadFile::new(path, bytes.clone()))
    }
}

impl DirectoryLister for RecordingFileReader {
    type Error = TestError;

    async fn list_directory(
        &self,
        path: PathBuf,
    ) -> Result<Vec<DirectoryEntry>, ListDirectoryError<Self::Error>> {
        let mut entries = self
            .files
            .keys()
            .filter(|entry| entry.parent() == Some(path.as_path()))
            .cloned()
            .map(|entry| DirectoryEntry::new(entry, DirectoryEntryKind::RegularFile))
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.resolved_path().cmp(right.resolved_path()));
        Ok(entries)
    }
}

#[derive(Clone, Default)]
struct InlineCpuExecutor {
    calls: Arc<AtomicUsize>,
}

impl CpuTaskExecutor for InlineCpuExecutor {
    type Error = TestError;

    async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(task())
    }
}

#[derive(Clone, Default)]
struct RecordingRecoverablePublisher {
    requests: Arc<Mutex<Vec<DirectoryStageRequest>>>,
    publish_calls: Arc<Mutex<Vec<(PathBuf, PathBuf, DirectoryPublishIntent)>>>,
    discard_calls: Arc<Mutex<Vec<PathBuf>>>,
    publish_error: Arc<Mutex<Option<DirectoryPublishError<TestError>>>>,
}

impl RecoverableDirectoryPublisher for RecordingRecoverablePublisher {
    type Error = TestError;
    type StagingState = ();

    async fn prepare(
        &self,
        request: DirectoryStageRequest,
    ) -> Result<StagedDirectory<Self::StagingState>, DirectoryPrepareError<Self::Error>> {
        let target_root = request.target_root().to_path_buf();
        let publish_intent = request.publish_intent();
        self.requests
            .lock()
            .expect("发布记录锁不应中毒")
            .push(request);
        let staging_root = target_root.with_extension("att-stage");
        Ok(StagedDirectory::new(
            target_root,
            staging_root,
            publish_intent,
            (),
        ))
    }

    async fn publish(
        &self,
        staged: StagedDirectory<Self::StagingState>,
    ) -> Result<(), DirectoryPublishError<Self::Error>> {
        let mode = staged.publish_intent();
        self.publish_calls
            .lock()
            .expect("发布记录锁不应中毒")
            .push((
                staged.target_root().to_path_buf(),
                staged.staging_root().to_path_buf(),
                mode,
            ));
        if let Some(error) = self
            .publish_error
            .lock()
            .expect("发布结果锁不应中毒")
            .take()
        {
            return Err(error);
        }
        Ok(())
    }

    async fn discard(
        &self,
        staged: StagedDirectory<Self::StagingState>,
    ) -> Result<(), DirectoryDiscardError<Self::Error>> {
        self.discard_calls
            .lock()
            .expect("丢弃记录锁不应中毒")
            .push(staged.staging_root().to_path_buf());
        Ok(())
    }
}

impl ScopedDirectoryEditor for RecordingRecoverablePublisher {
    type CandidateState = ();
    type ScopeState = ();
    type Error = TestError;

    fn bind_scoped_directory(
        &self,
        candidate: &StagedDirectory<Self::CandidateState>,
        scope: ScopedDirectoryScope,
    ) -> impl Future<
        Output = Result<
            BoundScopedDirectory<Self::ScopeState>,
            ScopedDirectoryBindError<Self::Error>,
        >,
    > + Send
    + use<> {
        let root = candidate.staging_root().to_path_buf();
        std::future::ready(Ok(BoundScopedDirectory::new(root, scope, ())))
    }

    fn read_scoped_file(
        &self,
        _scope: &BoundScopedDirectory<Self::ScopeState>,
        _path: ScopedDirectoryPath,
    ) -> impl Future<Output = Result<Vec<u8>, ScopedDirectoryEditError<Self::Error>>> + Send {
        std::future::ready(Ok(Vec::new()))
    }

    fn list_scoped_directory(
        &self,
        _scope: &BoundScopedDirectory<Self::ScopeState>,
        _path: ScopedDirectoryPath,
    ) -> impl Future<
        Output = Result<Vec<ScopedDirectoryEntry>, ScopedDirectoryEditError<Self::Error>>,
    > + Send {
        std::future::ready(Ok(Vec::new()))
    }

    fn list_scoped_root(
        &self,
        _scope: &BoundScopedDirectory<Self::ScopeState>,
    ) -> impl Future<
        Output = Result<Vec<ScopedDirectoryEntry>, ScopedDirectoryEditError<Self::Error>>,
    > + Send {
        std::future::ready(Ok(vec![
            ScopedDirectoryEntry::new("data".into(), ScopedDirectoryEntryKind::Directory),
            ScopedDirectoryEntry::new("js".into(), ScopedDirectoryEntryKind::Directory),
        ]))
    }

    fn create_scoped_directory(
        &self,
        _scope: &BoundScopedDirectory<Self::ScopeState>,
        _path: ScopedDirectoryPath,
    ) -> impl Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send {
        std::future::ready(Ok(()))
    }

    fn write_scoped_file(
        &self,
        _scope: &BoundScopedDirectory<Self::ScopeState>,
        _path: ScopedDirectoryPath,
        _bytes: Vec<u8>,
    ) -> impl Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send {
        std::future::ready(Ok(()))
    }

    fn remove_scoped_path(
        &self,
        _scope: &BoundScopedDirectory<Self::ScopeState>,
        _path: ScopedDirectoryPath,
    ) -> impl Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send {
        std::future::ready(Ok(()))
    }
}

#[derive(Clone, Default)]
struct RecordingProjectLog {
    events: Arc<Mutex<Vec<WriteBackLogEvent>>>,
}

impl WriteBackLog for RecordingProjectLog {
    fn emit(&self, event: WriteBackLogEvent) {
        self.events.lock().expect("日志记录锁不应中毒").push(event);
    }
}

#[derive(Clone, Default)]
struct RecordingLlm {
    calls: Arc<AtomicUsize>,
}

impl LlmRequestExecutor for RecordingLlm {
    type Client = ();
    type Error = TestError;

    fn request<'a>(
        &'a self,
        _client: &'a Self::Client,
        _messages: &'a [ChatMessage],
    ) -> impl Future<Output = Result<LlmResponse, LlmRequestError<Self::Error>>> + Send + 'a {
        self.calls.fetch_add(1, Ordering::SeqCst);
        async {
            Ok(LlmResponse::new(
                "不应到达 LLM 根",
                LlmFinishReason::Stop,
                None,
                Some("unused-response".to_owned()),
                None,
            ))
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeTransactionMode {
    Commit,
    LeaveActive,
}

#[derive(Clone, Debug, Default)]
struct RuntimeFacts {
    program_path: Option<PathBuf>,
    program_source: Vec<u8>,
    phase: Option<LuaPhase>,
    project: Option<LuaProjectContext>,
    llm_unavailable: bool,
}

#[derive(Clone)]
struct ExercisingLuaRuntime {
    mode: RuntimeTransactionMode,
    facts: Arc<Mutex<RuntimeFacts>>,
}

impl TrustedLuaRuntimeExecutor for ExercisingLuaRuntime {
    type Error = TestError;

    fn start(
        &self,
        program: OwnedLuaProgram,
        bindings: TrustedLuaRuntimeBindings,
    ) -> TrustedLuaExecutionHandle<Self::Error> {
        let (common, phase, finalizer) = bindings.into_parts();
        let is_write_back = matches!(phase, TrustedLuaPhaseBindings::WriteBack(_));
        assert!(is_write_back, "WriteBack 测试树只应收到 WriteBack 阶段能力");
        let calls = Arc::clone(common.calls());
        {
            let mut facts = self.facts.lock().expect("Runtime 记录锁不应中毒");
            facts.program_path = Some(program.main_script_path().to_path_buf());
            facts.program_source = program.source().to_vec();
            facts.phase = Some(LuaPhase::WriteBack);
            facts.project = Some(calls.project().clone());
            facts.llm_unavailable = true;
        }

        let (sender, receiver) = tokio::sync::oneshot::channel();
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mode = self.mode;
        tokio::spawn(async move {
            let runtime = match calls.begin().await {
                Err(source) => Err(TrustedLuaRuntimeExecutionError::Binding(source)),
                Ok(()) if mode == RuntimeTransactionMode::Commit => calls
                    .commit()
                    .await
                    .map_err(TrustedLuaRuntimeExecutionError::Binding),
                Ok(()) => Ok(()),
            };
            let finalization = finalizer.finalize().await;
            let _ = sender.send(TrustedLuaRuntimeExecutionReport::new(runtime, finalization));
        });
        TrustedLuaExecutionHandle::new(receiver, cancelled)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionTransactionState {
    Idle,
    Active,
}

#[derive(Clone, Debug)]
struct SessionFacts {
    state: SessionTransactionState,
    begin_calls: usize,
    commit_calls: usize,
    rollback_calls: usize,
    close_calls: usize,
    closed: bool,
}

impl Default for SessionFacts {
    fn default() -> Self {
        Self {
            state: SessionTransactionState::Idle,
            begin_calls: 0,
            commit_calls: 0,
            rollback_calls: 0,
            close_calls: 0,
            closed: false,
        }
    }
}

#[derive(Clone, Default)]
struct RecordingSession {
    facts: Arc<Mutex<SessionFacts>>,
}

impl SqliteInteractiveSessionOperations for RecordingSession {
    type Error = TestError;

    async fn query(
        &self,
        _query: SqliteQuery,
    ) -> Result<Vec<SqliteRow>, SqliteInteractiveSessionError<Self::Error>> {
        Ok(Vec::new())
    }

    async fn execute(
        &self,
        _command: SqliteCommand,
    ) -> Result<u64, SqliteInteractiveSessionError<Self::Error>> {
        Ok(0)
    }

    async fn begin(&self) -> Result<(), SqliteInteractiveSessionError<Self::Error>> {
        let mut facts = self.facts.lock().expect("Session 记录锁不应中毒");
        if facts.closed {
            return Err(SqliteInteractiveSessionError::Closed);
        }
        if facts.state == SessionTransactionState::Active {
            return Err(SqliteInteractiveSessionError::TransactionAlreadyActive);
        }
        facts.begin_calls += 1;
        facts.state = SessionTransactionState::Active;
        Ok(())
    }

    async fn commit(&self) -> Result<(), SqliteInteractiveSessionError<Self::Error>> {
        let mut facts = self.facts.lock().expect("Session 记录锁不应中毒");
        if facts.state != SessionTransactionState::Active {
            return Err(SqliteInteractiveSessionError::NoActiveTransaction);
        }
        facts.commit_calls += 1;
        facts.state = SessionTransactionState::Idle;
        Ok(())
    }

    async fn rollback(&self) -> Result<(), SqliteInteractiveSessionError<Self::Error>> {
        let mut facts = self.facts.lock().expect("Session 记录锁不应中毒");
        if facts.state != SessionTransactionState::Active {
            return Err(SqliteInteractiveSessionError::NoActiveTransaction);
        }
        facts.rollback_calls += 1;
        facts.state = SessionTransactionState::Idle;
        Ok(())
    }

    async fn transaction_active(&self) -> Result<bool, SqliteInteractiveSessionError<Self::Error>> {
        let facts = self.facts.lock().expect("Session 记录锁不应中毒");
        if facts.closed {
            return Err(SqliteInteractiveSessionError::Closed);
        }
        Ok(facts.state == SessionTransactionState::Active)
    }
}

struct RecordingSessionFinalizer {
    facts: Arc<Mutex<SessionFacts>>,
}

impl SqliteInteractiveSessionFinalizer for RecordingSessionFinalizer {
    type Error = TestError;

    async fn finalize(
        self,
    ) -> Result<
        SqliteInteractiveSessionFinalization,
        SqliteInteractiveSessionFinalizationError<Self::Error>,
    > {
        let mut facts = self.facts.lock().expect("Session 记录锁不应中毒");
        let had_unclosed_transaction = if facts.state == SessionTransactionState::Active {
            facts.rollback_calls += 1;
            facts.state = SessionTransactionState::Idle;
            true
        } else {
            false
        };
        facts.close_calls += 1;
        facts.closed = true;
        Ok(SqliteInteractiveSessionFinalization::new(
            had_unclosed_transaction,
        ))
    }
}

#[derive(Clone)]
struct RecordingSessionFactory {
    session: RecordingSession,
    opened_paths: Arc<Mutex<Vec<PathBuf>>>,
}

impl SqliteInteractiveSessionFactory for RecordingSessionFactory {
    type Operations = RecordingSession;
    type Finalizer = RecordingSessionFinalizer;
    type Error = TestError;

    async fn open_existing(
        &self,
        path: PathBuf,
    ) -> Result<
        OpenedSqliteInteractiveSession<Self::Operations, Self::Finalizer>,
        OpenSqliteInteractiveSessionError<Self::Error>,
    > {
        self.opened_paths
            .lock()
            .expect("Session factory 记录锁不应中毒")
            .push(path);
        Ok(OpenedSqliteInteractiveSession::new(
            Arc::new(self.session.clone()),
            RecordingSessionFinalizer {
                facts: Arc::clone(&self.session.facts),
            },
        ))
    }
}

#[derive(Clone)]
struct FullTreeObservations {
    sqlite_calls: Arc<Mutex<Vec<(PathBuf, SqliteQuery)>>>,
    resolved_directories: Arc<Mutex<Vec<PathBuf>>>,
    file_calls: Arc<Mutex<Vec<PathBuf>>>,
    cpu_calls: Arc<AtomicUsize>,
    publish_requests: Arc<Mutex<Vec<DirectoryStageRequest>>>,
    publish_calls: Arc<Mutex<Vec<(PathBuf, PathBuf, DirectoryPublishIntent)>>>,
    discard_calls: Arc<Mutex<Vec<PathBuf>>>,
    log_events: Arc<Mutex<Vec<WriteBackLogEvent>>>,
    llm_calls: Arc<AtomicUsize>,
    runtime_facts: Arc<Mutex<RuntimeFacts>>,
    session_facts: Arc<Mutex<SessionFacts>>,
    opened_database_paths: Arc<Mutex<Vec<PathBuf>>>,
    progress: Arc<Mutex<Vec<ProgressSnapshot<WriteBackProgressPhase>>>>,
}

type FullTreeRunFuture = Pin<
    Box<
        dyn Future<
                Output = Result<OperationCompletion<WriteBackOutput>, Box<dyn Error + Send + Sync>>,
            > + Send,
    >,
>;

struct FullTreeService {
    execute: Box<dyn Fn(WriteBackInput) -> FullTreeRunFuture + Send + Sync>,
}

impl FullTreeService {
    async fn execute(
        &self,
        input: WriteBackInput,
    ) -> Result<OperationCompletion<WriteBackOutput>, Box<dyn Error + Send + Sync>> {
        (self.execute)(input).await
    }
}

fn build_full_tree(mode: RuntimeTransactionMode) -> (FullTreeService, FullTreeObservations) {
    build_full_tree_with_publish_error(mode, None)
}

fn build_full_tree_with_publish_error(
    mode: RuntimeTransactionMode,
    publish_error: Option<DirectoryPublishError<TestError>>,
) -> (FullTreeService, FullTreeObservations) {
    let sqlite = RecordingSqliteQuery::default();
    let resolver = RecordingDirectoryResolver::default();
    let file_reader = RecordingFileReader {
        files: Arc::new(source_files()),
        calls: Arc::new(Mutex::new(Vec::new())),
    };
    let cpu = InlineCpuExecutor::default();
    let directory_publisher = RecordingRecoverablePublisher {
        publish_error: Arc::new(Mutex::new(publish_error)),
        ..RecordingRecoverablePublisher::default()
    };
    let project_log = RecordingProjectLog::default();
    let llm = RecordingLlm::default();
    let runtime = ExercisingLuaRuntime {
        mode,
        facts: Arc::new(Mutex::new(RuntimeFacts::default())),
    };
    let session = RecordingSession::default();
    let session_factory = RecordingSessionFactory {
        session: session.clone(),
        opened_paths: Arc::new(Mutex::new(Vec::new())),
    };
    let progress = RecordingProgress::default();

    let record_reader = ProjectDatabaseRecordReadingService::new(
        PathBuf::from(PROJECTS_ROOT),
        RpgMakerLayout::MZ,
        sqlite.clone(),
    );
    let opener = ExistingProjectOpeningService::new(
        record_reader,
        resolver.clone(),
        MatchingDirectoryTreeFingerprinter,
    );
    let asset_reader =
        RpgMakerStandardWriteBackAssetReadingService::new(sqlite.clone(), cpu.clone());
    let document_reader = RpgMakerProjectDocumentReadingService::new(
        file_reader.clone(),
        file_reader.clone(),
        cpu.clone(),
        RpgMakerDocumentReadingConfig::new(non_zero(2)),
    );
    let rewriter = RpgMakerWriteBackDocumentRewritingService::new(document_reader, cpu.clone())
        .with_progress(progress.clone());
    let cancellation = crate::execution::CooperativeCancellation::default();
    let publisher = StandardWriteBackPublishingService::new(directory_publisher.clone());
    let standard = StandardWriteBackService::new(
        asset_reader,
        ConservativeRpgMakerWriteBackTextLayouter,
        rewriter,
        cpu.clone(),
        cancellation.clone(),
    )
    .with_progress(progress.clone());
    let host = TrustedLuaExecutionHostingService::<_, RecordingLlm, _, _>::without_llm(
        file_reader.clone(),
        runtime.clone(),
        session_factory.clone(),
    );
    let lua = LuaWriteBackService::new(host, directory_publisher.clone());
    let service = WriteBackService::new(
        opener,
        standard,
        publisher,
        Some(SelectedLua::new(
            crate::rpg_maker::lua::runtime::OwnedLuaProgram::new(
                PathBuf::from(LUA_SCRIPT),
                b"-- trusted write-back".to_vec(),
            ),
            lua,
        )),
        project_log.clone(),
        FakeProjectLease,
        cancellation,
    )
    .with_progress(progress.clone());
    let service = Arc::new(service);
    let run_service = Arc::clone(&service);
    let service = FullTreeService {
        execute: Box::new(move |input| {
            let service = Arc::clone(&run_service);
            Box::pin(async move {
                service
                    .execute(input)
                    .await
                    .map_err(|source| Box::new(source) as Box<dyn Error + Send + Sync>)
            })
        }),
    };
    let observations = FullTreeObservations {
        sqlite_calls: sqlite.calls,
        resolved_directories: resolver.calls,
        file_calls: file_reader.calls,
        cpu_calls: cpu.calls,
        publish_requests: directory_publisher.requests,
        publish_calls: directory_publisher.publish_calls,
        discard_calls: directory_publisher.discard_calls,
        log_events: project_log.events,
        llm_calls: llm.calls,
        runtime_facts: runtime.facts,
        session_facts: session.facts,
        opened_database_paths: session_factory.opened_paths,
        progress: progress.0,
    };
    (service, observations)
}

#[tokio::test]
async fn real_write_back_non_root_tree_rewrites_publishes_logs_and_runs_lua() {
    let (service, observations) = build_full_tree(RuntimeTransactionMode::Commit);

    let output = service
        .execute(write_back_input())
        .await
        .expect("完整非根 WriteBack 树应该成功");
    let OperationCompletion::Completed(output) = output else {
        panic!("完整非根 WriteBack 树应正常完成")
    };

    assert_eq!(output.name.as_str(), PROJECT_NAME);
    assert_eq!(output.output_root, workspace_root().join("write_back"));
    assert!(output.lua_executed);
    assert_eq!(output.standard.translated_units, 3);
    assert_eq!(output.standard.original_units, 0);
    assert_eq!(output.standard.auto_wrapped_units, 2);
    assert_eq!(output.standard.inserted_line_breaks, 2);
    assert_eq!(output.standard.inserted_fullwidth_indents, 1);
    assert_eq!(output.standard.manual_layout_units, 0);

    assert_project_open_and_asset_queries(&observations);
    assert_document_reads_and_cpu_work(&observations);
    assert_published_documents(&observations, output.standard);
    assert_successful_lua_execution(&observations);
    assert_real_progress(&observations);
}

fn assert_real_progress(observations: &FullTreeObservations) {
    let snapshots = observations
        .progress
        .lock()
        .expect("进度记录锁不应中毒")
        .clone();

    let reading = determinate_counts(&snapshots, WriteBackProgressPhase::ReadingAssets);
    assert_eq!(reading, vec![(0, 1), (1, 1)]);

    assert_complete_monotonic_counts(&snapshots, WriteBackProgressPhase::PlanningStandard);
    assert!(snapshots.iter().any(|snapshot| {
        snapshot.phase == WriteBackProgressPhase::RewritingDocuments
            && snapshot.amount == ProgressAmount::Indeterminate
    }));
    assert_complete_monotonic_counts(&snapshots, WriteBackProgressPhase::RewritingDocuments);

    let expected_order = [
        WriteBackProgressPhase::ReadingAssets,
        WriteBackProgressPhase::PlanningStandard,
        WriteBackProgressPhase::RewritingDocuments,
        WriteBackProgressPhase::PreparingCandidate,
        WriteBackProgressPhase::RunningLua,
        WriteBackProgressPhase::ValidatingCandidate,
        WriteBackProgressPhase::Publishing,
    ];
    let mut cursor = 0;
    for phase in expected_order {
        let offset = snapshots[cursor..]
            .iter()
            .position(|snapshot| snapshot.phase == phase)
            .unwrap_or_else(|| panic!("真实写回未观测到阶段 {phase:?}：{snapshots:?}"));
        cursor += offset + 1;
    }
}

fn assert_complete_monotonic_counts(
    snapshots: &[ProgressSnapshot<WriteBackProgressPhase>],
    phase: WriteBackProgressPhase,
) {
    let counts = determinate_counts(snapshots, phase);
    assert!(!counts.is_empty(), "阶段 {phase:?} 必须报告真实数量");
    let total = counts[0].1;
    assert!(total > 0, "真实样本的阶段 {phase:?} 必须包含工作项");
    assert_eq!(counts[0], (0, total));
    assert_eq!(counts.last(), Some(&(total, total)));
    assert!(
        counts
            .windows(2)
            .all(|pair| pair[0].1 == total && pair[1] == (pair[0].0 + 1, total)),
        "阶段 {phase:?} 的绝对快照必须单调且共享同一分母：{counts:?}"
    );
}

fn determinate_counts(
    snapshots: &[ProgressSnapshot<WriteBackProgressPhase>],
    phase: WriteBackProgressPhase,
) -> Vec<(u64, u64)> {
    snapshots
        .iter()
        .filter_map(|snapshot| match snapshot.amount {
            ProgressAmount::Determinate { completed, total } if snapshot.phase == phase => {
                Some((completed, total))
            }
            ProgressAmount::Indeterminate | ProgressAmount::Determinate { .. } => None,
        })
        .collect()
}

#[tokio::test]
async fn unclosed_lua_transaction_discards_the_unpublished_candidate() {
    let (service, observations) = build_full_tree(RuntimeTransactionMode::LeaveActive);

    let error = service
        .execute(write_back_input())
        .await
        .expect_err("Lua 遗留活动事务必须令命令失败");

    let message = error.to_string();
    assert!(message.contains("Lua 写回候选失败"));
    assert!(message.contains("write_back.lua"));
    assert!(message.contains("write_back"));
    assert_eq!(
        observations
            .publish_requests
            .lock()
            .expect("发布记录锁不应中毒")
            .len(),
        1,
        "Lua 执行前必须已准备完整候选"
    );
    assert!(
        observations
            .publish_calls
            .lock()
            .expect("发布记录锁不应中毒")
            .is_empty(),
        "Lua 失败不得让候选成为最终输出"
    );
    assert_eq!(
        observations
            .discard_calls
            .lock()
            .expect("丢弃记录锁不应中毒")
            .as_slice(),
        [workspace_root().join("write_back.att-stage")]
    );
    assert!(
        observations
            .log_events
            .lock()
            .expect("日志记录锁不应中毒")
            .is_empty(),
        "未发布候选不得记录完成日志"
    );
    let session = observations
        .session_facts
        .lock()
        .expect("Session 记录锁不应中毒")
        .clone();
    assert_eq!(session.begin_calls, 1);
    assert_eq!(session.commit_calls, 0);
    assert_eq!(session.rollback_calls, 1);
    assert_eq!(session.close_calls, 1);
    assert!(session.closed);
    assert_eq!(session.state, SessionTransactionState::Idle);
    assert_eq!(observations.llm_calls.load(Ordering::SeqCst), 0);
    assert!(
        observations
            .runtime_facts
            .lock()
            .expect("Runtime 记录锁不应中毒")
            .llm_unavailable
    );
}

#[tokio::test]
async fn published_cleanup_failure_is_not_logged_as_complete_standard_success() {
    let target_root = workspace_root().join("write_back");
    let residual_path = workspace_root().join(".write_back-old");
    let (service, observations) = build_full_tree_with_publish_error(
        RuntimeTransactionMode::Commit,
        Some(DirectoryPublishError::PublishedWithResiduals {
            target_root: target_root.clone(),
            residual_path: residual_path.clone(),
            source: TestError("cleanup"),
        }),
    );

    let error = service
        .execute(write_back_input())
        .await
        .expect_err("已发布但清理失败不是完全成功");

    let message = error.to_string();
    assert!(message.contains("已发布"));
    assert!(message.contains(&target_root.display().to_string()));
    assert!(message.contains(&residual_path.display().to_string()));
    assert_eq!(
        observations
            .publish_calls
            .lock()
            .expect("发布记录锁不应中毒")
            .len(),
        1
    );
    assert_eq!(
        *observations.log_events.lock().expect("日志记录锁不应中毒"),
        vec![
            WriteBackLogEvent::PublicationStarted {
                output_root: target_root.clone(),
            },
            WriteBackLogEvent::PublicationFinished {
                output_root: target_root.clone(),
                outcome: WriteBackLogPublicationOutcome::PublishedWithResiduals,
            },
        ]
    );
    assert!(
        observations
            .discard_calls
            .lock()
            .expect("丢弃记录锁不应中毒")
            .is_empty(),
        "发布根接管 token 后不得由顶层再次丢弃"
    );
    assert_eq!(observations.llm_calls.load(Ordering::SeqCst), 0);
}

fn assert_project_open_and_asset_queries(observations: &FullTreeObservations) {
    let calls = observations
        .sqlite_calls
        .lock()
        .expect("SQLite 记录锁不应中毒");
    assert_eq!(calls.len(), 11);
    assert!(calls[0].1.statement().contains("FROM metadata"));
    assert!(
        calls[1]
            .1
            .statement()
            .contains("FROM standard_asset_owner_state")
    );
    for (offset, statement_fragment) in [
        (2, "FROM standard_text_group"),
        (5, "standard_text_unit AS unit"),
        (8, "FROM standard_mutation_claim"),
    ] {
        for (owner_offset, owner) in ["builtin", "rules", "lua"].into_iter().enumerate() {
            let query = &calls[offset + owner_offset].1;
            assert!(query.statement().contains(statement_fragment));
            assert_eq!(query.parameters(), &[SqliteValue::Text(owner.to_owned())]);
        }
    }
    assert!(
        calls
            .iter()
            .all(|(_, query)| !query.statement().contains("UNION ALL"))
    );
    assert!(
        calls[..2]
            .iter()
            .all(|(_, query)| query.parameters().is_empty())
    );
    assert!(calls.iter().all(|(path, _)| path == &database_path()));
    drop(calls);

    assert_eq!(
        *observations
            .resolved_directories
            .lock()
            .expect("目录记录锁不应中毒"),
        vec![
            workspace_root().join("source/data"),
            workspace_root().join("source/js"),
        ]
    );
}

fn assert_document_reads_and_cpu_work(observations: &FullTreeObservations) {
    let calls = observations
        .file_calls
        .lock()
        .expect("文件读取记录锁不应中毒");
    assert!(calls.contains(&workspace_root().join("source/data/Items.json")));
    assert!(calls.contains(&workspace_root().join("source/data/Map001.json")));
    assert_eq!(calls.len(), 2);
    assert!(observations.cpu_calls.load(Ordering::SeqCst) >= 6);
}

fn assert_published_documents(
    observations: &FullTreeObservations,
    summary: super::StandardWriteBackSummary,
) {
    let requests = observations
        .publish_requests
        .lock()
        .expect("发布记录锁不应中毒");
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.target_root(), workspace_root().join("write_back"));
    assert_eq!(request.source_mappings().len(), 2);
    assert_eq!(
        request.source_mappings()[0].source_directory(),
        workspace_root().join("source/data")
    );
    assert_eq!(
        request.source_mappings()[0].relative_target(),
        Path::new("data")
    );
    assert_eq!(
        request.source_mappings()[1].source_directory(),
        workspace_root().join("source/js")
    );
    assert_eq!(
        request.source_mappings()[1].relative_target(),
        Path::new("js")
    );
    assert_eq!(request.overlays().len(), 2);
    assert!(request.empty_directories().is_empty());

    let publish_calls = observations
        .publish_calls
        .lock()
        .expect("发布记录锁不应中毒");
    assert_eq!(publish_calls.len(), 1);
    assert_eq!(publish_calls[0].0, workspace_root().join("write_back"));
    assert_eq!(publish_calls[0].2, DirectoryPublishIntent::ReplaceExisting);
    drop(publish_calls);
    assert!(
        observations
            .discard_calls
            .lock()
            .expect("丢弃记录锁不应中毒")
            .is_empty()
    );

    let items: Value = serde_json::from_slice(overlay(request, "data/Items.json"))
        .expect("Items overlay 应为 JSON");
    assert_eq!(items[1]["name"], "Potion");
    assert_eq!(items[1]["description"], "甲乙，\n丙丁。");
    assert_eq!(items[1]["unknown"], true);

    let map: Value = serde_json::from_slice(overlay(request, "data/Map001.json"))
        .expect("Map overlay 应为 JSON");
    let list = map["events"][1]["pages"][0]["list"]
        .as_array()
        .expect("Map 事件 list 应为数组");
    assert_eq!(
        list.iter()
            .map(|command| command["code"].as_i64().expect("code 应为整数"))
            .collect::<Vec<_>>(),
        vec![101, 401, 401, 0]
    );
    assert_eq!(list[1]["parameters"][0], "「甲乙，");
    assert_eq!(list[2]["parameters"][0], "　丙丁」");
    assert_eq!(list[1]["lineUnknown"], "保留");
    assert_eq!(list[2]["lineUnknown"], "保留");
    assert_eq!(map["unknown"], 7);
    drop(requests);

    let log_events = observations.log_events.lock().expect("日志记录锁不应中毒");
    assert_eq!(
        log_events.as_slice(),
        [
            WriteBackLogEvent::PublicationStarted {
                output_root: workspace_root().join("write_back"),
            },
            WriteBackLogEvent::PublicationFinished {
                output_root: workspace_root().join("write_back"),
                outcome: WriteBackLogPublicationOutcome::Published {
                    standard: summary,
                    lua_executed: true,
                },
            },
        ]
    );
}

fn assert_successful_lua_execution(observations: &FullTreeObservations) {
    assert_eq!(observations.llm_calls.load(Ordering::SeqCst), 0);
    let runtime = observations
        .runtime_facts
        .lock()
        .expect("Runtime 记录锁不应中毒")
        .clone();
    assert_eq!(runtime.program_path, Some(PathBuf::from(LUA_SCRIPT)));
    assert_eq!(runtime.program_source, b"-- trusted write-back".to_vec());
    assert_eq!(runtime.phase, Some(LuaPhase::WriteBack));
    assert!(runtime.llm_unavailable);
    let project = runtime.project.expect("Runtime 应收到项目上下文");
    assert_eq!(project.name(), PROJECT_NAME);
    assert_eq!(project.source_root(), workspace_root().join("source"));
    assert_eq!(
        project.output_root(),
        Some(workspace_root().join("write_back.att-stage").as_path())
    );
    assert_eq!(project.database_path(), database_path());
    assert_eq!(project.source_language().as_str(), "ja");
    assert_eq!(project.target_language().as_str(), "zh-Hans");

    assert_eq!(
        *observations
            .opened_database_paths
            .lock()
            .expect("数据库打开记录锁不应中毒"),
        vec![database_path()]
    );
    let session = observations
        .session_facts
        .lock()
        .expect("Session 记录锁不应中毒")
        .clone();
    assert_eq!(session.begin_calls, 1);
    assert_eq!(session.commit_calls, 1);
    assert_eq!(session.rollback_calls, 0);
    assert_eq!(session.close_calls, 1);
    assert!(session.closed);
    assert_eq!(session.state, SessionTransactionState::Idle);
}

fn overlay<'a>(request: &'a DirectoryStageRequest, path: &str) -> &'a [u8] {
    request
        .overlays()
        .iter()
        .find(|overlay| overlay.relative_file() == Path::new(path))
        .unwrap_or_else(|| panic!("缺少 overlay：{path}"))
        .bytes()
}

fn write_back_input() -> WriteBackInput {
    WriteBackInput {
        name: project_name(),
    }
}

fn metadata_row() -> SqliteRow {
    SqliteRow::new(vec![
        SqliteValue::Text(PROJECT_NAME.to_owned()),
        SqliteValue::Text("ja".to_owned()),
        SqliteValue::Text("zh-Hans".to_owned()),
        SqliteValue::Blob(vec![7; 32]),
        SqliteValue::Integer(4),
        SqliteValue::Integer(8),
        SqliteValue::Integer(3),
        SqliteValue::Text(DIALOGUE_DEFINITION_JSON.to_owned()),
    ])
}

struct WriteBackSnapshotRows {
    owners: Vec<SqliteRow>,
    groups: Vec<SqliteRow>,
    units: Vec<SqliteRow>,
    claims: Vec<SqliteRow>,
}

fn write_back_snapshot_rows() -> WriteBackSnapshotRows {
    use crate::rpg_maker::model::{
        DialogueLinePart, DialogueLineRecipe, DialogueWriteRecipe, DirectTextPart,
        DirectTextRecipe, ScalarFieldKey, TextProjectionRecipe, TextUnitContent, TextUnitRole,
    };

    let item_source = RpgMakerSource::data(StandardDataFile::Items);
    let item_group =
        RpgMakerLocation::value(item_source.clone(), vec![RpgMakerLocationStep::index(1)]);
    let item_description = RpgMakerLocation::value(
        item_source.clone(),
        vec![
            RpgMakerLocationStep::index(1),
            RpgMakerLocationStep::key("description"),
        ],
    );
    let item_name = RpgMakerLocation::value(
        item_source,
        vec![
            RpgMakerLocationStep::index(1),
            RpgMakerLocationStep::key("name"),
        ],
    );

    let map_source = RpgMakerSource::map(1);
    let list_steps = vec![
        RpgMakerLocationStep::key("events"),
        RpgMakerLocationStep::index(1),
        RpgMakerLocationStep::key("pages"),
        RpgMakerLocationStep::index(0),
        RpgMakerLocationStep::key("list"),
    ];
    let dialogue_group = RpgMakerLocation::value(
        map_source.clone(),
        [list_steps.clone(), vec![RpgMakerLocationStep::index(0)]].concat(),
    );
    let dialogue_body = RpgMakerLocation::value(
        map_source,
        [
            list_steps,
            vec![
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("parameters"),
                RpgMakerLocationStep::index(0),
            ],
        ]
        .concat(),
    );

    let description_role =
        TextUnitRole::Scalar(ScalarFieldKey::new("description").expect("字段键应合法"));
    let description_recipe = TextProjectionRecipe::Direct(
        DirectTextRecipe::new(
            item_description,
            "旧说明\n第二行",
            vec![DirectTextPart::TextSlot {
                role: description_role.clone(),
            }],
        )
        .expect("说明配方应合法"),
    );
    let name_role = TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法"));
    let name_recipe = TextProjectionRecipe::Direct(
        DirectTextRecipe::new(
            item_name,
            "药水",
            vec![DirectTextPart::TextSlot {
                role: name_role.clone(),
            }],
        )
        .expect("名称配方应合法"),
    );
    let dialogue_role = TextUnitRole::DialogueBody;
    let dialogue_recipe = TextProjectionRecipe::Dialogue(
        DialogueWriteRecipe::new(
            dialogue_group.clone(),
            None,
            vec![
                DialogueLineRecipe::new(
                    dialogue_body,
                    "原始对话",
                    vec![DialogueLinePart::BodyLine {
                        source_line_index: 0,
                    }],
                )
                .expect("对话行配方应合法"),
            ],
        )
        .expect("对话配方应合法"),
    );

    let groups = vec![
        fixture_group(
            "builtin",
            0,
            &item_group,
            TextGroupKind::DatabaseEntry,
            description_recipe,
            (
                description_role,
                TextUnitContent::Value("旧说明\n第二行".to_owned()),
                TextUnitContent::Value("甲乙，丙丁。".to_owned()),
            ),
        ),
        fixture_group(
            "builtin",
            1,
            &dialogue_group,
            TextGroupKind::EventDialogue,
            dialogue_recipe,
            (
                dialogue_role,
                TextUnitContent::Lines(vec!["原始对话".to_owned()]),
                TextUnitContent::Lines(vec!["「甲乙，丙丁」".to_owned()]),
            ),
        ),
        fixture_group(
            "rules",
            0,
            &item_group,
            TextGroupKind::DatabaseEntry,
            name_recipe,
            (
                name_role,
                TextUnitContent::Value("药水".to_owned()),
                TextUnitContent::Value("Potion".to_owned()),
            ),
        ),
    ];

    let owners = ["builtin", "rules"]
        .into_iter()
        .map(|owner| {
            SqliteRow::new(vec![
                SqliteValue::Text(owner.to_owned()),
                SqliteValue::Blob(vec![7; 32]),
                SqliteValue::Blob(fixture_asset_fingerprint(owner, &groups).to_vec()),
            ])
        })
        .collect::<Vec<_>>();
    let mut group_rows = Vec::new();
    let mut unit_rows = Vec::new();
    let mut claim_rows = Vec::new();
    for group in groups {
        group_rows.push(SqliteRow::new(vec![
            SqliteValue::Text(group.owner.clone()),
            SqliteValue::Text(group.group_location.clone()),
            SqliteValue::Integer(i64::try_from(group.group_order).expect("测试顺序应可编码")),
            SqliteValue::Text(group.kind.to_owned()),
            SqliteValue::Text(group.recipes.clone()),
        ]));
        unit_rows.push(SqliteRow::new(vec![
            SqliteValue::Text(group.owner.clone()),
            SqliteValue::Text(group.group_location.clone()),
            SqliteValue::Text(group.role.clone()),
            SqliteValue::Integer(0),
            SqliteValue::Text(group.source_content_json.clone()),
            SqliteValue::Text("{}".to_owned()),
            SqliteValue::Text(group.translation_content_json),
        ]));
        for (resource_key, access) in group.claims {
            claim_rows.push(SqliteRow::new(vec![
                SqliteValue::Text(group.owner.clone()),
                SqliteValue::Text(group.group_location.clone()),
                SqliteValue::Text(resource_key),
                SqliteValue::Text(access),
            ]));
        }
    }
    group_rows.sort_by(|left, right| {
        snapshot_row_text(left, 0)
            .cmp(snapshot_row_text(right, 0))
            .then_with(|| snapshot_row_text(left, 1).cmp(snapshot_row_text(right, 1)))
    });
    unit_rows.sort_by(|left, right| {
        snapshot_row_text(left, 0)
            .cmp(snapshot_row_text(right, 0))
            .then_with(|| snapshot_row_text(left, 1).cmp(snapshot_row_text(right, 1)))
            .then_with(|| snapshot_row_text(left, 2).cmp(snapshot_row_text(right, 2)))
    });
    claim_rows.sort_by(|left, right| {
        snapshot_row_text(left, 2)
            .cmp(snapshot_row_text(right, 2))
            .then_with(|| snapshot_row_text(left, 3).cmp(snapshot_row_text(right, 3)))
            .then_with(|| snapshot_row_text(left, 0).cmp(snapshot_row_text(right, 0)))
            .then_with(|| snapshot_row_text(left, 1).cmp(snapshot_row_text(right, 1)))
    });
    WriteBackSnapshotRows {
        owners,
        groups: group_rows,
        units: unit_rows,
        claims: claim_rows,
    }
}

fn snapshot_row_text(row: &SqliteRow, index: usize) -> &str {
    match &row.values()[index] {
        SqliteValue::Text(value) => value,
        SqliteValue::Null => "",
        value => panic!(
            "测试快照排序列应为 TEXT 或 NULL，实际为 {}",
            value.kind_name()
        ),
    }
}

fn rows_for_requested_owner(rows: Vec<SqliteRow>, query: &SqliteQuery) -> Vec<SqliteRow> {
    let requested_owner = query.parameters().first().and_then(|value| match value {
        SqliteValue::Text(owner) => Some(owner.as_str()),
        _ => None,
    });
    rows.into_iter()
        .filter(|row| requested_owner.is_none_or(|owner| snapshot_row_text(row, 0) == owner))
        .collect()
}

struct FixtureGroup {
    owner: String,
    group_order: usize,
    group_location: String,
    kind: &'static str,
    recipes: String,
    role: String,
    source_content_json: String,
    translation_content_json: String,
    claims: Vec<(String, String)>,
}

fn fixture_group(
    owner: &str,
    group_order: usize,
    group_location: &RpgMakerLocation,
    kind: TextGroupKind,
    recipe: crate::rpg_maker::model::TextProjectionRecipe,
    unit: (
        crate::rpg_maker::model::TextUnitRole,
        crate::rpg_maker::model::TextUnitContent,
        crate::rpg_maker::model::TextUnitContent,
    ),
) -> FixtureGroup {
    use crate::rpg_maker::location_codec::RpgMakerProjectionCodec;

    let (role, source_content, translation_content) = unit;
    let recipes = [recipe];
    let claims = crate::rpg_maker::model::mutation_claims_for_group(kind, group_location, &recipes)
        .expect("测试配方 Claim 应合法")
        .locks()
        .iter()
        .map(|lock| {
            (
                RpgMakerProjectionCodec::encode_mutation_resource(lock.resource())
                    .expect("资源应可编码"),
                lock.access().storage_name().to_owned(),
            )
        })
        .collect();
    FixtureGroup {
        owner: owner.to_owned(),
        group_order,
        group_location: RpgMakerLocationCodec::encode(group_location).expect("组位置应可编码"),
        kind: fixture_group_kind(kind),
        recipes: RpgMakerProjectionCodec::encode_recipes(&recipes).expect("配方应可编码"),
        role: RpgMakerProjectionCodec::encode_role(&role).expect("角色应可编码"),
        source_content_json: serde_json::to_string(&source_content).expect("单元原文应可编码"),
        translation_content_json: serde_json::to_string(&translation_content)
            .expect("单元译文应可编码"),
        claims,
    }
}

fn fixture_asset_fingerprint(owner: &str, groups: &[FixtureGroup]) -> [u8; 32] {
    use crate::fingerprint::Sha256FramedHasher;

    let mut owner_groups = groups
        .iter()
        .filter(|group| group.owner == owner)
        .collect::<Vec<_>>();
    owner_groups.sort_by_key(|group| group.group_order);
    let mut units = owner_groups.clone();
    units.sort_by_key(|group| group.group_order);
    let mut claims = owner_groups
        .iter()
        .flat_map(|group| {
            group
                .claims
                .iter()
                .map(|(resource, access)| (resource, access, &group.group_location))
        })
        .collect::<Vec<_>>();
    claims.sort();

    let mut hasher = Sha256FramedHasher::new(b"att.rpg_maker.standard_text_snapshot");
    hasher.frame(1, owner.as_bytes());
    if owner == "builtin" {
        hasher
            .frame(14, b"project_definition")
            .frame(15, DIALOGUE_DEFINITION_JSON.as_bytes());
    }
    for group in owner_groups {
        let group_order = u64::try_from(group.group_order).expect("测试顺序应可编码");
        hasher
            .frame(2, b"group")
            .frame(3, group.group_location.as_bytes())
            .frame(16, &group_order.to_le_bytes())
            .frame(4, group.kind.as_bytes())
            .frame(5, group.recipes.as_bytes());
    }
    for unit in units {
        let unit_order = 0_u64;
        hasher
            .frame(6, b"unit")
            .frame(7, unit.group_location.as_bytes())
            .frame(8, unit.role.as_bytes())
            .frame(17, &unit_order.to_le_bytes())
            .frame(9, unit.source_content_json.as_bytes())
            .frame(10, b"{}");
    }
    for (resource, access, group_location) in claims {
        hasher
            .frame(11, b"claim")
            .frame(12, resource.as_bytes())
            .frame(18, access.as_bytes())
            .frame(13, group_location.as_bytes());
    }
    hasher.finish().into_bytes()
}

const fn fixture_group_kind(kind: TextGroupKind) -> &'static str {
    match kind {
        TextGroupKind::DatabaseEntry => "database_entry",
        TextGroupKind::System => "system",
        TextGroupKind::Map => "map",
        TextGroupKind::EventDialogue => "event_dialogue",
        TextGroupKind::EventChoices => "event_choices",
        TextGroupKind::EventScrollingText => "event_scrolling_text",
        TextGroupKind::EventCommand => "event_command",
        TextGroupKind::PluginParameter => "plugin_parameter",
    }
}

fn source_files() -> BTreeMap<PathBuf, Vec<u8>> {
    let items = json!([
        null,
        {
            "id": 1,
            "name": "药水",
            "description": "旧说明\n第二行",
            "unknown": true
        }
    ]);
    let map = json!({
        "events": [
            null,
            {
                "id": 1,
                "pages": [{
                    "list": [
                        {
                            "code": 101,
                            "indent": 0,
                            "parameters": ["", 0, 0, 2, ""]
                        },
                        {
                            "code": 401,
                            "indent": 1,
                            "parameters": ["原始对话"],
                            "lineUnknown": "保留"
                        },
                        {"code": 0, "indent": 0, "parameters": []}
                    ]
                }]
            }
        ],
        "unknown": 7
    });
    BTreeMap::from([
        (
            workspace_root().join("source/data/Items.json"),
            serde_json::to_vec(&items).expect("Items fixture 应可序列化"),
        ),
        (
            workspace_root().join("source/data/Map001.json"),
            serde_json::to_vec(&map).expect("Map fixture 应可序列化"),
        ),
        (PathBuf::from(LUA_SCRIPT), b"-- trusted write-back".to_vec()),
    ])
}

fn non_zero(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("测试资源配置必须非零")
}

fn project_name() -> ProjectName {
    PROJECT_NAME.parse().expect("测试项目名应该合法")
}

fn workspace_root() -> PathBuf {
    PathBuf::from(PROJECTS_ROOT).join("mz").join(PROJECT_NAME)
}

fn database_path() -> PathBuf {
    workspace_root().join("project.db")
}
