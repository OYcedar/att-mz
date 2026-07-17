//! WriteBack 全部非根能力使用根替身的纵向贯通测试。

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::asset_reader::MzStandardWriteBackAssetReadingService;
use super::lua::LuaWriteBackService;
use super::publisher::StandardWriteBackPublishingService;
use super::rewriter::MzWriteBackDocumentRewritingService;
use super::standard::{
    ConservativeMzWriteBackTextLayouter, StandardWriteBackRunLog, StandardWriteBackService,
};
use super::{WriteBackInput, WriteBackService, WriteBackUseCase};
use crate::att_mz::ProjectName;
use crate::att_mz::extract::document::{MzDocumentReadingConfig, MzProjectDocumentReadingService};
use crate::att_mz::location_codec::MzLocationCodec;
use crate::att_mz::lua::hosting::TrustedLuaExecutionHostingService;
use crate::att_mz::lua::runtime::{
    OwnedLuaProgram, TrustedLuaExecutionHandle, TrustedLuaRuntimeBindings,
    TrustedLuaRuntimeExecutionError, TrustedLuaRuntimeExecutionReport, TrustedLuaRuntimeExecutor,
    TrustedLuaRuntimeReservation, TrustedLuaRuntimeTermination,
};
use crate::att_mz::lua::session::{
    OpenSqliteInteractiveSessionError, OpenedSqliteInteractiveSession,
    SqliteInteractiveConnectionCloseOutcome, SqliteInteractiveRollbackOutcome,
    SqliteInteractiveSessionError, SqliteInteractiveSessionFactory,
    SqliteInteractiveSessionFinalizationReport, SqliteInteractiveSessionFinalizer,
    SqliteInteractiveSessionOperations, SqliteInteractiveTransactionObservation,
};
use crate::att_mz::lua::{LuaPhase, LuaProjectContext};
use crate::att_mz::project::ExistingProjectOpeningService;
use crate::att_mz::standard_asset::MzStandardAssetReadingConfig;
use crate::att_mz::text::{MzLocation, MzLocationStep, MzSource, StandardDataFile};
use crate::att_mz::translate::executor::{
    LlmFinishReason, LlmRequestError, LlmRequestExecutor, LlmResponse,
};
use crate::att_mz::translate::standard::ChatMessage;
use crate::observability::PersistentEventLog;
use crate::project_database::ProjectDatabaseRecordReadingService;
use crate::storage::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::storage::file_system::{
    DirectoryDiscardError, DirectoryLister, DirectoryPrepareError, DirectoryPublishError,
    DirectoryPublishIntent, DirectoryStageRequest, ExistingDirectoryResolver, FileReader,
    ListDirectoryError, ReadFile, ReadFileError, RecoverableDirectoryPublisher,
    ResolveDirectoryError, StagedDirectory,
};
use crate::storage::sqlite::{
    QueryExistingDatabaseError, SqliteCommand, SqliteQuery, SqliteQueryExecutor, SqliteRow,
    SqliteValue,
};

const PROJECTS_ROOT: &str = "C:/att/projects";
const PROJECT_NAME: &str = "demo";
const LUA_SCRIPT: &str = "C:/att/scripts/write_back.lua";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TestError(&'static str);

impl fmt::Display for TestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for TestError {}

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
        } else if query.statement().contains("'entry' AS asset_table") {
            Ok(write_back_asset_rows())
        } else {
            Err(QueryExistingDatabaseError::QueryFailed(TestError(
                "意外的全树测试查询",
            )))
        }
    }
}

#[derive(Clone, Default)]
struct RecordingDirectoryResolver {
    calls: Arc<Mutex<Vec<PathBuf>>>,
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

#[derive(Clone, Copy)]
struct RejectingDirectoryLister;

impl DirectoryLister for RejectingDirectoryLister {
    type Error = TestError;

    async fn list_directory(
        &self,
        path: PathBuf,
    ) -> Result<Vec<PathBuf>, ListDirectoryError<Self::Error>> {
        Err(ListDirectoryError::Io {
            path,
            source: TestError("精确 Map 选择不应列举 data"),
        })
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
        _staged: StagedDirectory<Self::StagingState>,
    ) -> Result<(), DirectoryDiscardError<Self::Error>> {
        panic!("Standard WriteBack 全树不应丢弃已暂存候选")
    }
}

#[derive(Clone, Default)]
struct RecordingRunLog {
    events: Arc<Mutex<Vec<StandardWriteBackRunLog>>>,
}

impl PersistentEventLog<StandardWriteBackRunLog> for RecordingRunLog {
    type Error = TestError;

    async fn append(&self, event: StandardWriteBackRunLog) -> Result<(), Self::Error> {
        self.events.lock().expect("日志记录锁不应中毒").push(event);
        Ok(())
    }
}

#[derive(Clone, Default)]
struct RecordingLlm {
    calls: Arc<AtomicUsize>,
}

impl LlmRequestExecutor for RecordingLlm {
    type Profile = ();
    type Error = TestError;

    fn request<'a>(
        &'a self,
        _profile: &'a Self::Profile,
        _messages: &'a [ChatMessage],
    ) -> impl Future<Output = Result<LlmResponse, LlmRequestError<Self::Error>>> + Send + 'a {
        self.calls.fetch_add(1, Ordering::SeqCst);
        async {
            Ok(LlmResponse::new(
                "不应到达 LLM 根",
                LlmFinishReason::Stop,
                None,
                "unused-response",
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
    type Reservation = ExercisingLuaReservation;

    async fn reserve(&self) -> Result<Self::Reservation, Self::Error> {
        Ok(ExercisingLuaReservation {
            mode: self.mode,
            facts: Arc::clone(&self.facts),
        })
    }
}

struct ExercisingLuaReservation {
    mode: RuntimeTransactionMode,
    facts: Arc<Mutex<RuntimeFacts>>,
}

impl TrustedLuaRuntimeReservation for ExercisingLuaReservation {
    type Error = TestError;

    fn start(
        self,
        program: OwnedLuaProgram,
        bindings: TrustedLuaRuntimeBindings,
    ) -> TrustedLuaExecutionHandle<Self::Error> {
        let (calls, finalizer) = bindings.into_parts();
        {
            let mut facts = self.facts.lock().expect("Runtime 记录锁不应中毒");
            facts.program_path = Some(program.main_script_path().to_path_buf());
            facts.program_source = program.source().to_vec();
            facts.phase = Some(calls.phase());
            facts.project = Some(calls.project().clone());
        }

        let (sender, receiver) = tokio::sync::oneshot::channel();
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let facts = Arc::clone(&self.facts);
        let mode = self.mode;
        tokio::spawn(async move {
            let llm_unavailable = calls.request_llm(Vec::new()).await.is_err();
            facts
                .lock()
                .expect("Runtime 记录锁不应中毒")
                .llm_unavailable = llm_unavailable;

            let runtime = match calls.begin().await {
                Err(source) => Err(TrustedLuaRuntimeExecutionError::Binding(source)),
                Ok(()) if mode == RuntimeTransactionMode::Commit => calls
                    .commit()
                    .await
                    .map_err(TrustedLuaRuntimeExecutionError::Binding),
                Ok(()) => Ok(()),
            };
            let termination = if runtime.is_ok() {
                TrustedLuaRuntimeTermination::Completed
            } else {
                TrustedLuaRuntimeTermination::Failed
            };
            let finalization = finalizer.finalize(termination).await;
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
}

struct RecordingSessionFinalizer {
    facts: Arc<Mutex<SessionFacts>>,
}

impl SqliteInteractiveSessionFinalizer for RecordingSessionFinalizer {
    type Error = TestError;

    async fn finalize(self) -> SqliteInteractiveSessionFinalizationReport<Self::Error> {
        let mut facts = self.facts.lock().expect("Session 记录锁不应中毒");
        let transaction = if facts.state == SessionTransactionState::Active {
            facts.rollback_calls += 1;
            facts.state = SessionTransactionState::Idle;
            SqliteInteractiveTransactionObservation::Active
        } else {
            SqliteInteractiveTransactionObservation::Idle
        };
        let rollback = if matches!(transaction, SqliteInteractiveTransactionObservation::Active) {
            SqliteInteractiveRollbackOutcome::RolledBack
        } else {
            SqliteInteractiveRollbackOutcome::NotRequired
        };
        facts.close_calls += 1;
        facts.closed = true;
        SqliteInteractiveSessionFinalizationReport::new(
            transaction,
            rollback,
            SqliteInteractiveConnectionCloseOutcome::Closed,
        )
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
    run_logs: Arc<Mutex<Vec<StandardWriteBackRunLog>>>,
    llm_calls: Arc<AtomicUsize>,
    runtime_facts: Arc<Mutex<RuntimeFacts>>,
    session_facts: Arc<Mutex<SessionFacts>>,
    opened_database_paths: Arc<Mutex<Vec<PathBuf>>>,
}

fn build_full_tree(mode: RuntimeTransactionMode) -> (impl WriteBackUseCase, FullTreeObservations) {
    build_full_tree_with_publish_error(mode, None)
}

fn build_full_tree_with_publish_error(
    mode: RuntimeTransactionMode,
    publish_error: Option<DirectoryPublishError<TestError>>,
) -> (impl WriteBackUseCase, FullTreeObservations) {
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
    let run_log = RecordingRunLog::default();
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

    let record_reader =
        ProjectDatabaseRecordReadingService::new(PathBuf::from(PROJECTS_ROOT), sqlite.clone());
    let opener = ExistingProjectOpeningService::new(record_reader, resolver.clone());
    let asset_reader = MzStandardWriteBackAssetReadingService::new(
        sqlite.clone(),
        cpu.clone(),
        MzStandardAssetReadingConfig::new(non_zero(2), non_zero(1)),
    );
    let document_reader = MzProjectDocumentReadingService::new(
        file_reader.clone(),
        RejectingDirectoryLister,
        cpu.clone(),
        MzDocumentReadingConfig::new(non_zero(2), non_zero(2)),
    );
    let rewriter = MzWriteBackDocumentRewritingService::new(document_reader, cpu.clone());
    let cancellation = crate::execution::CooperativeCancellation::default();
    let publisher =
        StandardWriteBackPublishingService::new(directory_publisher.clone(), cancellation.clone());
    let standard = StandardWriteBackService::new(
        asset_reader,
        ConservativeMzWriteBackTextLayouter,
        rewriter,
        publisher,
        run_log.clone(),
        cancellation.clone(),
    );
    let host = TrustedLuaExecutionHostingService::<_, RecordingLlm, _, _>::without_llm(
        file_reader.clone(),
        runtime.clone(),
        session_factory.clone(),
    );
    let lua = LuaWriteBackService::new(host);
    let service = WriteBackService::new(opener, standard, Some(lua), cancellation);
    let observations = FullTreeObservations {
        sqlite_calls: sqlite.calls,
        resolved_directories: resolver.calls,
        file_calls: file_reader.calls,
        cpu_calls: cpu.calls,
        publish_requests: directory_publisher.requests,
        publish_calls: directory_publisher.publish_calls,
        run_logs: run_log.events,
        llm_calls: llm.calls,
        runtime_facts: runtime.facts,
        session_facts: session.facts,
        opened_database_paths: session_factory.opened_paths,
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

    assert_eq!(output.name.as_str(), PROJECT_NAME);
    assert_eq!(output.output_root, workspace_root().join("write_back"));
    assert!(output.lua_executed);
    assert_eq!(output.standard.translated_locations, 2);
    assert_eq!(output.standard.original_locations, 0);
    assert_eq!(output.standard.auto_wrapped_units, 2);
    assert_eq!(output.standard.inserted_line_breaks, 2);
    assert_eq!(output.standard.inserted_fullwidth_indents, 1);
    assert_eq!(output.standard.manual_layout_units, 0);

    assert_project_open_and_asset_queries(&observations);
    assert_document_reads_and_cpu_work(&observations);
    assert_published_documents(&observations, output.standard);
    assert_successful_lua_execution(&observations);
}

#[tokio::test]
async fn unclosed_lua_transaction_is_rolled_back_after_standard_remains_published() {
    let (service, observations) = build_full_tree(RuntimeTransactionMode::LeaveActive);

    let error = service
        .execute(write_back_input())
        .await
        .expect_err("Lua 遗留活动事务必须令命令失败");

    let message = error.to_string();
    assert!(message.contains("Lua 写回失败"));
    assert!(message.contains("write_back.lua"));
    assert!(message.contains("write_back"));
    assert_eq!(
        observations
            .publish_requests
            .lock()
            .expect("发布记录锁不应中毒")
            .len(),
        1,
        "Lua 失败不得撤销已经确认的 Standard 发布"
    );
    assert_eq!(
        observations
            .run_logs
            .lock()
            .expect("日志记录锁不应中毒")
            .len(),
        1
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
    assert!(
        observations
            .run_logs
            .lock()
            .expect("日志记录锁不应中毒")
            .is_empty()
    );
    assert_eq!(observations.llm_calls.load(Ordering::SeqCst), 0);
}

fn assert_project_open_and_asset_queries(observations: &FullTreeObservations) {
    let calls = observations
        .sqlite_calls
        .lock()
        .expect("SQLite 记录锁不应中毒");
    assert_eq!(calls.len(), 2);
    assert!(calls[0].1.statement().contains("FROM metadata"));
    assert!(calls[1].1.statement().contains("UNION ALL"));
    assert!(!calls[1].1.statement().contains("terminology"));
    assert!(calls.iter().all(|(_, query)| query.parameters().is_empty()));
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
    assert!(calls.contains(&PathBuf::from(LUA_SCRIPT)));
    assert_eq!(calls.len(), 3);
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

    let items: Value = serde_json::from_slice(overlay(request, "data/Items.json"))
        .expect("Items overlay 应为 JSON");
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

    let logs = observations.run_logs.lock().expect("日志记录锁不应中毒");
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].name().as_str(), PROJECT_NAME);
    assert_eq!(logs[0].output_root(), workspace_root().join("write_back"));
    assert_eq!(logs[0].summary(), summary);
    assert!(logs[0].manual_layout_diagnostics().is_empty());
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
    assert_eq!(project.name().as_str(), PROJECT_NAME);
    assert_eq!(project.source_root(), workspace_root().join("source"));
    assert_eq!(
        project.output_root(),
        Some(workspace_root().join("write_back").as_path())
    );
    assert_eq!(project.database_path(), database_path());
    assert_eq!(project.source_language(), "ja");
    assert_eq!(project.target_language(), "zh-Hans");

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
        lua_script: Some(PathBuf::from(LUA_SCRIPT)),
    }
}

fn metadata_row() -> SqliteRow {
    SqliteRow::new(vec![
        SqliteValue::Text(PROJECT_NAME.to_owned()),
        SqliteValue::Text("ja".to_owned()),
        SqliteValue::Text("zh-Hans".to_owned()),
        SqliteValue::Integer(4),
        SqliteValue::Integer(8),
        SqliteValue::Integer(3),
    ])
}

fn write_back_asset_rows() -> Vec<SqliteRow> {
    let item_source = MzSource::data(StandardDataFile::Items);
    let item_group = MzLocation::value(item_source.clone(), vec![MzLocationStep::index(1)]);
    let item_description = MzLocation::value(
        item_source,
        vec![MzLocationStep::index(1), MzLocationStep::key("description")],
    );

    let map_source = MzSource::map(1);
    let list_steps = vec![
        MzLocationStep::key("events"),
        MzLocationStep::index(1),
        MzLocationStep::key("pages"),
        MzLocationStep::index(0),
        MzLocationStep::key("list"),
    ];
    let dialogue_group = MzLocation::value(
        map_source.clone(),
        [list_steps.clone(), vec![MzLocationStep::index(0)]].concat(),
    );
    let dialogue_body = MzLocation::value(
        map_source,
        [
            list_steps,
            vec![
                MzLocationStep::index(1),
                MzLocationStep::key("parameters"),
                MzLocationStep::index(0),
            ],
        ]
        .concat(),
    );

    vec![
        write_back_row(
            "entry",
            &item_description,
            &item_group,
            "description",
            None,
            "旧说明\n第二行",
            "甲乙，丙丁。",
        ),
        write_back_row(
            "text_body",
            &dialogue_body,
            &dialogue_group,
            "body[0]",
            Some("dialogue"),
            "原始对话",
            "「甲乙，丙丁」",
        ),
    ]
}

fn write_back_row(
    table: &str,
    exact_location: &MzLocation,
    group_location: &MzLocation,
    field_name: &str,
    unit_type: Option<&str>,
    original_text: &str,
    translation: &str,
) -> SqliteRow {
    SqliteRow::new(vec![
        SqliteValue::Text(table.to_owned()),
        SqliteValue::Text(MzLocationCodec::encode(exact_location).expect("精确位置应该可编码")),
        SqliteValue::Text("builtin".to_owned()),
        SqliteValue::Text(MzLocationCodec::encode(group_location).expect("组位置应该可编码")),
        SqliteValue::Text(field_name.to_owned()),
        unit_type.map_or(SqliteValue::Null, |value| {
            SqliteValue::Text(value.to_owned())
        }),
        SqliteValue::Text(original_text.to_owned()),
        SqliteValue::Text(translation.to_owned()),
    ])
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
    PathBuf::from(PROJECTS_ROOT).join(PROJECT_NAME)
}

fn database_path() -> PathBuf {
    workspace_root().join("project.db")
}
