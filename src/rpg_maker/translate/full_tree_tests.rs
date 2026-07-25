use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::TranslateInput;
use super::asset_reader::RpgMakerStandardTranslationAssetReadingService;
use super::executor::{
    AsyncDelay, RpgMakerStandardTranslationTaskExecutionService,
    TranslationTaskResponseProcessingService,
};
use super::lua::{LuaTranslation, LuaTranslationService};
use super::placeholder::Pcre2PlaceholderService;
use super::planner::RpgMakerStandardTranslationTaskPlanningService;
use super::planning_resource::TranslationPlanningResourceReadingService;
use super::profile::{
    ResolvedRpgMakerTranslationResources, RpgMakerSystemPrompt,
    RpgMakerTranslationPlanningConfiguration, RpgMakerTranslationProfile,
    RpgMakerTranslationRequestConfiguration, TranslationResponseEnvelope,
};
use super::result_store::RpgMakerStandardTranslationResultStorageService;
use super::service::{
    SelectedTranslationExecution, SelectedTranslationExecutionBuilder, TranslateService,
};
use super::standard::{
    StandardTranslation, StandardTranslationLog, StandardTranslationLogEvent,
    StandardTranslationLogTaskOutcome, StandardTranslationService,
};
use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, SafeDiagnostic, SafeDiagnosticSource,
};
use crate::execution::OperationCompletion;
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::fingerprint::Sha256Fingerprint;
use crate::language::{
    JapaneseLanguageModule, JapaneseResidualPolicy, LanguageId, LanguageModule, LanguagePair,
};
use crate::llm::{
    ChatMessage, ChatMessageRole, LlmClientConcurrency, LlmClientSemanticIdentity, LlmFinishReason,
    LlmRequestDiagnosticSource, LlmRequestError, LlmRequestExecutor, LlmResponse,
};
use crate::rpg_maker::RpgMakerLayout;
use crate::rpg_maker::location_codec::{RpgMakerLocationCodec, RpgMakerProjectionCodec};
use crate::rpg_maker::lua::LuaPhase;
use crate::rpg_maker::lua::hosting::TrustedLuaExecutionHostingService;
use crate::rpg_maker::lua::runtime::{
    OwnedLuaProgram, TrustedLuaExecutionHandle, TrustedLuaPhaseBindings, TrustedLuaRuntimeBindings,
    TrustedLuaRuntimeExecutionError, TrustedLuaRuntimeExecutionReport, TrustedLuaRuntimeExecutor,
};
use crate::rpg_maker::model::{ScalarFieldKey, TextUnitRole};
use crate::rpg_maker::project::ExistingProjectOpeningService;
use crate::rpg_maker::project_database::ProjectDatabaseRecordReadingService;
use crate::rpg_maker::project_lease::{
    ProjectCommandLease, ProjectCommandLeaseError, ProjectCommandLeaseProvider,
};
use crate::rpg_maker::text::{
    RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, StandardDataFile,
};
use crate::storage::file_system::{
    DirectoryEntry, DirectoryLister, DirectoryTreeFingerprintError,
    DirectoryTreeFingerprintRequest, DirectoryTreeFingerprinter, ExistingDirectoryResolver,
    FileReader, ListDirectoryError, ReadFile, ReadFileError, ResolveDirectoryError,
};
use crate::storage::sqlite::{
    ExecuteTransactionError, QueryExistingDatabaseError, SqliteCommand, SqliteQuery,
    SqliteQueryExecutor, SqliteRow, SqliteTransactionPlan, SqliteTransactionStep, SqliteValue,
};
use crate::storage::sqlite_session::{
    OpenSqliteInteractiveSessionError, OpenedSqliteInteractiveSession,
    SqliteInteractiveSessionError, SqliteInteractiveSessionFactory,
    SqliteInteractiveSessionFinalization, SqliteInteractiveSessionFinalizationError,
    SqliteInteractiveSessionFinalizer, SqliteInteractiveSessionOperations,
};
use crate::storage::sqlite_transaction_session::{
    OpenSqliteTransactionSessionError, OpenedSqliteTransactionSession,
    SqliteTransactionSessionFactory, SqliteTransactionSessionOperations,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FakeRootError(&'static str);

impl fmt::Display for FakeRootError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for FakeRootError {}

impl SafeDiagnosticSource for FakeRootError {
    fn safe_diagnostic_source(
        &self,
        stage: DiagnosticStage,
        impact: DiagnosticImpact,
        action: DiagnosticAction,
    ) -> SafeDiagnostic {
        SafeDiagnostic::new(
            DiagnosticCode::InternalOperation,
            stage,
            DiagnosticSubject::component("translate full-tree test root"),
            DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
            impact,
            action,
        )
    }
}

impl LlmRequestDiagnosticSource for FakeRootError {
    fn request_diagnostic(
        &self,
        retry_after: Option<Duration>,
        impact: crate::diagnostic::DiagnosticImpact,
    ) -> crate::diagnostic::SafeDiagnostic {
        crate::diagnostic::SafeDiagnostic::new(
            crate::diagnostic::DiagnosticCode::ModelRequest,
            crate::diagnostic::DiagnosticStage::ModelRequest,
            crate::diagnostic::DiagnosticSubject::component("fake LLM provider"),
            crate::diagnostic::DiagnosticReason::Http {
                status: Some(503),
                retry_after_seconds: retry_after.map(|value| value.as_secs()),
                provider_code: Some("temporarily_unavailable".to_owned()),
                provider_type: Some("service_error".to_owned()),
            },
            impact,
            crate::diagnostic::DiagnosticAction::CheckModelService,
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Event {
    QueryMetadata,
    QueryAssets,
    Cpu,
    LlmStandard {
        attempt: usize,
        client_address: usize,
    },
    Delay(Duration),
    PreparationTransaction,
    CommitTransaction,
    LogTaskStarted,
    LogTask,
    LogCommitFailure,
    ReadLua(PathBuf),
    OpenLuaDatabase(PathBuf),
    LuaRuntime,
    LuaBegin,
    LuaExecute,
    LlmLua {
        client_address: usize,
    },
    LuaCommit,
    LuaInspect,
    LuaClose,
}

#[derive(Clone)]
struct FakeTranslationLog {
    events: EventLog,
    calls: Arc<AtomicUsize>,
}

impl StandardTranslationLog for FakeTranslationLog {
    fn emit(&self, event: StandardTranslationLogEvent) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let recorded = match event {
            StandardTranslationLogEvent::TaskStarted { .. } => Event::LogTaskStarted,
            StandardTranslationLogEvent::TaskFinished {
                outcome:
                    StandardTranslationLogTaskOutcome::Complete
                    | StandardTranslationLogTaskOutcome::Partial
                    | StandardTranslationLogTaskOutcome::Unavailable,
                ..
            } => Event::LogTask,
            StandardTranslationLogEvent::TaskFinished { .. } => Event::LogCommitFailure,
            StandardTranslationLogEvent::PlanningUnresolved { .. } => return,
        };
        record(&self.events, recorded);
    }
}

type EventLog = Arc<Mutex<Vec<Event>>>;

fn record(events: &EventLog, event: Event) {
    events.lock().expect("事件锁不应中毒").push(event);
}

#[derive(Clone)]
struct FakeCpuTaskExecutor {
    events: EventLog,
    calls: Arc<AtomicUsize>,
}

impl CpuTaskExecutor for FakeCpuTaskExecutor {
    type Error = FakeRootError;

    async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        self.calls.fetch_add(1, Ordering::SeqCst);
        record(&self.events, Event::Cpu);
        Ok(task())
    }
}

#[derive(Clone)]
struct FakeSqliteQueryExecutor {
    events: EventLog,
}

impl SqliteQueryExecutor for FakeSqliteQueryExecutor {
    type Error = FakeRootError;

    async fn query_existing_database(
        &self,
        path: PathBuf,
        query: SqliteQuery,
    ) -> Result<Vec<SqliteRow>, QueryExistingDatabaseError<Self::Error>> {
        assert_eq!(
            path,
            PathBuf::from("C:/projects")
                .join("mz")
                .join("demo")
                .join("project.db")
        );
        if query.statement().contains("FROM metadata") && !query.statement().contains("UNION ALL") {
            record(&self.events, Event::QueryMetadata);
            assert!(query.parameters().is_empty());
            return Ok(vec![SqliteRow::new(vec![
                SqliteValue::Text("demo".to_owned()),
                SqliteValue::Text("ja".to_owned()),
                SqliteValue::Text("zh-Hans".to_owned()),
                SqliteValue::Blob(vec![0xa5; 32]),
                SqliteValue::Integer(24),
                SqliteValue::Integer(30),
                SqliteValue::Integer(18),
                SqliteValue::Text("{\"rules\":[]}".to_owned()),
            ])]);
        }

        panic!("标准翻译资产必须使用同一快照中的窄查询")
    }

    async fn query_existing_database_snapshot(
        &self,
        path: PathBuf,
        queries: Vec<SqliteQuery>,
    ) -> Result<Vec<Vec<SqliteRow>>, QueryExistingDatabaseError<Self::Error>> {
        assert_eq!(
            path,
            PathBuf::from("C:/projects")
                .join("mz")
                .join("demo")
                .join("project.db")
        );
        assert_eq!(queries.len(), 9);
        assert_eq!(
            queries.iter().map(SqliteQuery::id).collect::<Vec<_>>(),
            [
                "translation.metadata",
                "translation.owners",
                "translation.resources",
                "translation.builtin.groups",
                "translation.rules.groups",
                "translation.lua.groups",
                "translation.builtin.units",
                "translation.rules.units",
                "translation.lua.units",
            ]
        );
        assert!(
            queries[..3]
                .iter()
                .all(|query| query.parameters().is_empty())
        );
        assert_eq!(
            queries[3..]
                .iter()
                .map(|query| match query.parameters() {
                    [SqliteValue::Text(owner)] => owner.as_str(),
                    parameters => panic!("owner 分区查询参数无效：{parameters:?}"),
                })
                .collect::<Vec<_>>(),
            ["builtin", "rules", "lua", "builtin", "rules", "lua"]
        );
        assert!(
            queries
                .iter()
                .all(|query| !query.statement().contains("UNION ALL"))
        );
        assert!(queries[0].statement().contains("FROM metadata"));
        assert!(
            queries[1]
                .statement()
                .contains("standard_asset_owner_state")
        );
        assert!(
            queries[2]
                .statement()
                .contains("standard_translation_resource")
        );
        assert!(queries[3..6].iter().all(|query| {
            query.statement().contains("FROM standard_text_group")
                && !query.statement().contains("standard_text_unit")
        }));
        assert!(queries[6..].iter().all(|query| {
            query.statement().contains("standard_text_unit")
                && query.statement().contains("WHERE text_group.owner = ?")
        }));
        record(&self.events, Event::QueryAssets);
        Ok(vec![
            vec![metadata_snapshot_row()],
            vec![owner_snapshot_row()],
            vec![
                resource_snapshot_row("placeholder_rules"),
                resource_snapshot_row("terminology"),
            ],
            vec![standard_group_row(1, 0), standard_group_row(2, 1)],
            Vec::new(),
            Vec::new(),
            vec![standard_asset_row(1, 0), standard_asset_row(2, 1)],
            Vec::new(),
            Vec::new(),
        ])
    }
}

#[derive(Clone, Copy)]
struct FakeDirectoryResolver;

impl ExistingDirectoryResolver for FakeDirectoryResolver {
    type Error = FakeRootError;

    async fn resolve_existing_directory(
        &self,
        path: PathBuf,
    ) -> Result<PathBuf, ResolveDirectoryError<Self::Error>> {
        Ok(path)
    }
}

#[derive(Clone, Copy)]
struct FakeDirectoryTreeFingerprinter;

impl DirectoryTreeFingerprinter for FakeDirectoryTreeFingerprinter {
    type Error = FakeRootError;

    async fn fingerprint_directory_tree(
        &self,
        request: DirectoryTreeFingerprintRequest,
    ) -> Result<Sha256Fingerprint, DirectoryTreeFingerprintError<Self::Error>> {
        assert_eq!(request.roots().len(), 2);
        Ok(Sha256Fingerprint::from_bytes([0xa5; 32]))
    }
}

fn metadata_snapshot_row() -> SqliteRow {
    SqliteRow::new(vec![SqliteValue::Blob(vec![0xa5; 32])])
}

fn owner_snapshot_row() -> SqliteRow {
    SqliteRow::new(vec![
        SqliteValue::Text("builtin".to_owned()),
        SqliteValue::Blob(vec![0xa5; 32]),
        SqliteValue::Blob(vec![0xb4; 32]),
    ])
}

fn resource_snapshot_row(kind: &str) -> SqliteRow {
    SqliteRow::new(vec![
        SqliteValue::Text(kind.to_owned()),
        SqliteValue::Text("[]".to_owned()),
    ])
}

fn standard_group_row(index: usize, group_order: i64) -> SqliteRow {
    let group_location = RpgMakerLocation::value(
        RpgMakerSource::data(StandardDataFile::Items),
        vec![RpgMakerLocationStep::index(index)],
    );
    SqliteRow::new(vec![
        SqliteValue::Text(
            RpgMakerLocationCodec::encode(&group_location).expect("测试位置应可编码"),
        ),
        SqliteValue::Text("database_entry".to_owned()),
        SqliteValue::Integer(group_order),
    ])
}

fn standard_asset_row(index: usize, group_order: i64) -> SqliteRow {
    let group_location = RpgMakerLocation::value(
        RpgMakerSource::data(StandardDataFile::Items),
        vec![RpgMakerLocationStep::index(index)],
    );
    let unit_role = RpgMakerProjectionCodec::encode_role(&TextUnitRole::Scalar(
        ScalarFieldKey::new("name").expect("字段键应合法"),
    ))
    .expect("字段角色应可编码");

    SqliteRow::new(vec![
        SqliteValue::Text(
            RpgMakerLocationCodec::encode(&group_location).expect("测试位置应可编码"),
        ),
        SqliteValue::Text("database_entry".to_owned()),
        SqliteValue::Integer(group_order),
        SqliteValue::Text(unit_role),
        SqliteValue::Integer(0),
        SqliteValue::Text(r#""魔法剣""#.to_owned()),
        SqliteValue::Text("{}".to_owned()),
        SqliteValue::Null,
        SqliteValue::Null,
    ])
}

#[derive(Clone)]
struct FakeSqliteTransactionExecutor {
    events: EventLog,
    calls: Arc<AtomicUsize>,
}

impl SqliteTransactionSessionOperations for FakeSqliteTransactionExecutor {
    type Error = FakeRootError;

    async fn execute_transaction(
        &self,
        plan: SqliteTransactionPlan,
    ) -> Result<(), ExecuteTransactionError<Self::Error>> {
        let translated_unit_count = plan
            .steps()
            .iter()
            .map(|step| match step {
                SqliteTransactionStep::Execute(command) => usize::from(
                    command
                        .parameters()
                        .contains(&SqliteValue::Text(r#""魔法剑""#.to_owned())),
                ),
                SqliteTransactionStep::ExecuteMany(batch)
                | SqliteTransactionStep::ExecuteManyExactlyOne(batch) => {
                    let translation = SqliteValue::Text(r#""魔法剑""#.to_owned());
                    if batch.shared_parameters().contains(&translation) {
                        batch.parameter_set_count()
                    } else {
                        batch
                            .parameter_rows()
                            .filter(|parameters| parameters.contains(&translation))
                            .count()
                    }
                }
                SqliteTransactionStep::RequireNoRows(_)
                | SqliteTransactionStep::RequireNoRowsReturningFirstRow(_)
                | SqliteTransactionStep::RequireNoRowsMany(_) => 0,
            })
            .sum::<usize>();
        let event = if translated_unit_count == 0 {
            assert!(
                !plan.steps().is_empty(),
                "准备事务至少必须携带 baseline CAS 或必要的状态更新"
            );
            Event::PreparationTransaction
        } else {
            assert_eq!(
                translated_unit_count, 2,
                "一次代表译文必须在同一事务中扩散到两个物理位置"
            );
            Event::CommitTransaction
        };
        self.calls.fetch_add(1, Ordering::SeqCst);
        record(&self.events, event);
        Ok(())
    }
}

struct FakeSqliteTransactionFinalizer;

impl SqliteInteractiveSessionFinalizer for FakeSqliteTransactionFinalizer {
    type Error = FakeRootError;

    async fn finalize(
        self,
    ) -> Result<
        SqliteInteractiveSessionFinalization,
        SqliteInteractiveSessionFinalizationError<Self::Error>,
    > {
        Ok(SqliteInteractiveSessionFinalization::new(false))
    }
}

impl SqliteTransactionSessionFactory for FakeSqliteTransactionExecutor {
    type Operations = FakeSqliteTransactionExecutor;
    type Finalizer = FakeSqliteTransactionFinalizer;
    type Error = FakeRootError;

    async fn open_existing_transaction_session(
        &self,
        path: PathBuf,
    ) -> Result<
        OpenedSqliteTransactionSession<Self::Operations, Self::Finalizer>,
        OpenSqliteTransactionSessionError<Self::Error>,
    > {
        assert_eq!(
            path,
            PathBuf::from("C:/projects")
                .join("mz")
                .join("demo")
                .join("project.db")
        );
        Ok(OpenedSqliteTransactionSession::new(
            Arc::new(self.clone()),
            FakeSqliteTransactionFinalizer,
        ))
    }
}

#[derive(Clone, Debug)]
struct FakeLlmClient {
    name: &'static str,
}

impl LlmClientSemanticIdentity for FakeLlmClient {
    fn semantic_fingerprint(&self) -> Sha256Fingerprint {
        Sha256Fingerprint::from_bytes([0x4c; 32])
    }
}

impl LlmClientConcurrency for FakeLlmClient {
    fn max_concurrent_requests(&self) -> NonZeroUsize {
        NonZeroUsize::MIN
    }
}

#[derive(Clone)]
struct FakeLlmRequestExecutor {
    events: EventLog,
    standard_attempts: Arc<AtomicUsize>,
}

impl LlmRequestExecutor for FakeLlmRequestExecutor {
    type Client = FakeLlmClient;
    type Error = FakeRootError;

    async fn request<'a>(
        &'a self,
        client: &'a Self::Client,
        messages: &'a [ChatMessage],
    ) -> Result<LlmResponse, LlmRequestError<Self::Error>> {
        assert_eq!(client.name, "shared-llm-config");
        let client_address = std::ptr::from_ref(client).addr();
        let is_lua = messages
            .iter()
            .any(|message| message.content() == "# Lua full messages");

        if is_lua {
            record(&self.events, Event::LlmLua { client_address });
            return Ok(LlmResponse::new(
                "lua raw response",
                LlmFinishReason::Stop,
                Some("lua-request".to_owned()),
                Some("lua-response".to_owned()),
                None,
            ));
        }

        assert_eq!(
            messages.first().map(ChatMessage::role),
            Some(ChatMessageRole::System)
        );
        let attempt = self.standard_attempts.fetch_add(1, Ordering::SeqCst) + 1;
        record(
            &self.events,
            Event::LlmStandard {
                attempt,
                client_address,
            },
        );
        if attempt == 1 {
            return Err(LlmRequestError::Retryable {
                source: FakeRootError("temporary"),
                retry_after: None,
            });
        }

        Ok(LlmResponse::new(
            r#"{"1":["魔法剑"]}"#,
            LlmFinishReason::Stop,
            Some("standard-request".to_owned()),
            Some("standard-response".to_owned()),
            None,
        ))
    }
}

#[derive(Clone)]
struct FakeAsyncDelay {
    events: EventLog,
    calls: Arc<AtomicUsize>,
}

impl AsyncDelay for FakeAsyncDelay {
    async fn wait(&self, duration: Duration) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        record(&self.events, Event::Delay(duration));
    }
}

#[derive(Clone)]
struct FakeFileReader {
    events: EventLog,
    calls: Arc<AtomicUsize>,
}

impl FileReader for FakeFileReader {
    type Error = FakeRootError;

    async fn read_file(&self, path: PathBuf) -> Result<ReadFile, ReadFileError<Self::Error>> {
        assert_eq!(path, PathBuf::from("scripts/translate.lua"));
        self.calls.fetch_add(1, Ordering::SeqCst);
        record(&self.events, Event::ReadLua(path));
        Ok(ReadFile::new(
            PathBuf::from("C:/resolved/scripts/translate.lua"),
            b"return true".to_vec(),
        ))
    }
}

impl DirectoryLister for FakeFileReader {
    type Error = FakeRootError;

    async fn list_directory(
        &self,
        _path: PathBuf,
    ) -> Result<Vec<DirectoryEntry>, ListDirectoryError<Self::Error>> {
        Ok(Vec::new())
    }
}

#[derive(Debug)]
struct FakeSessionState {
    transaction_active: bool,
    closed: bool,
}

#[derive(Clone)]
struct FakeSqliteInteractiveSession {
    events: EventLog,
    state: Arc<Mutex<FakeSessionState>>,
}

impl SqliteInteractiveSessionOperations for FakeSqliteInteractiveSession {
    type Error = FakeRootError;

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
        let state = self.state.lock().expect("会话状态锁不应中毒");
        if state.closed {
            return Err(SqliteInteractiveSessionError::Closed);
        }
        assert!(state.transaction_active);
        drop(state);
        record(&self.events, Event::LuaExecute);
        Ok(1)
    }

    async fn begin(&self) -> Result<(), SqliteInteractiveSessionError<Self::Error>> {
        let mut state = self.state.lock().expect("会话状态锁不应中毒");
        if state.transaction_active {
            return Err(SqliteInteractiveSessionError::TransactionAlreadyActive);
        }
        state.transaction_active = true;
        drop(state);
        record(&self.events, Event::LuaBegin);
        Ok(())
    }

    async fn commit(&self) -> Result<(), SqliteInteractiveSessionError<Self::Error>> {
        let mut state = self.state.lock().expect("会话状态锁不应中毒");
        if !state.transaction_active {
            return Err(SqliteInteractiveSessionError::NoActiveTransaction);
        }
        state.transaction_active = false;
        drop(state);
        record(&self.events, Event::LuaCommit);
        Ok(())
    }

    async fn rollback(&self) -> Result<(), SqliteInteractiveSessionError<Self::Error>> {
        let mut state = self.state.lock().expect("会话状态锁不应中毒");
        if !state.transaction_active {
            return Err(SqliteInteractiveSessionError::NoActiveTransaction);
        }
        state.transaction_active = false;
        Ok(())
    }

    async fn transaction_active(&self) -> Result<bool, SqliteInteractiveSessionError<Self::Error>> {
        let state = self.state.lock().expect("会话状态锁不应中毒");
        if state.closed {
            return Err(SqliteInteractiveSessionError::Closed);
        }
        Ok(state.transaction_active)
    }
}

struct FakeSqliteInteractiveSessionFinalizer {
    events: EventLog,
    state: Arc<Mutex<FakeSessionState>>,
}

impl SqliteInteractiveSessionFinalizer for FakeSqliteInteractiveSessionFinalizer {
    type Error = FakeRootError;

    async fn finalize(
        self,
    ) -> Result<
        SqliteInteractiveSessionFinalization,
        SqliteInteractiveSessionFinalizationError<Self::Error>,
    > {
        record(&self.events, Event::LuaInspect);
        let mut state = self.state.lock().expect("会话状态锁不应中毒");
        let had_unclosed_transaction = if state.transaction_active {
            state.transaction_active = false;
            true
        } else {
            false
        };
        state.closed = true;
        drop(state);
        record(&self.events, Event::LuaClose);
        Ok(SqliteInteractiveSessionFinalization::new(
            had_unclosed_transaction,
        ))
    }
}

#[derive(Clone)]
struct FakeSqliteInteractiveSessionFactory {
    events: EventLog,
    session: FakeSqliteInteractiveSession,
    calls: Arc<AtomicUsize>,
}

impl SqliteInteractiveSessionFactory for FakeSqliteInteractiveSessionFactory {
    type Operations = FakeSqliteInteractiveSession;
    type Finalizer = FakeSqliteInteractiveSessionFinalizer;
    type Error = FakeRootError;

    async fn open_existing(
        &self,
        path: PathBuf,
    ) -> Result<
        OpenedSqliteInteractiveSession<Self::Operations, Self::Finalizer>,
        OpenSqliteInteractiveSessionError<Self::Error>,
    > {
        assert_eq!(
            path,
            PathBuf::from("C:/projects")
                .join("mz")
                .join("demo")
                .join("project.db")
        );
        self.calls.fetch_add(1, Ordering::SeqCst);
        record(&self.events, Event::OpenLuaDatabase(path));
        Ok(OpenedSqliteInteractiveSession::new(
            Arc::new(self.session.clone()),
            FakeSqliteInteractiveSessionFinalizer {
                events: Arc::clone(&self.events),
                state: Arc::clone(&self.session.state),
            },
        ))
    }
}

#[derive(Clone)]
struct FakeTrustedLuaRuntimeExecutor {
    events: EventLog,
    calls: Arc<AtomicUsize>,
}

impl TrustedLuaRuntimeExecutor for FakeTrustedLuaRuntimeExecutor {
    type Error = FakeRootError;

    fn start(
        &self,
        program: OwnedLuaProgram,
        bindings: TrustedLuaRuntimeBindings,
    ) -> TrustedLuaExecutionHandle<Self::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        record(&self.events, Event::LuaRuntime);
        assert_eq!(
            program.main_script_path(),
            std::path::Path::new("C:/resolved/scripts/translate.lua")
        );
        assert_eq!(program.source(), b"return true");
        let (common, phase, finalizer) = bindings.into_parts();
        assert_eq!(phase.phase(), LuaPhase::Translate);
        let calls = Arc::clone(common.calls());
        assert_eq!(calls.project().name(), "demo");
        let TrustedLuaPhaseBindings::Translate(translate) = phase else {
            panic!("翻译 Runtime 必须获得 Translate 阶段能力");
        };

        let (sender, receiver) = tokio::sync::oneshot::channel();
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        tokio::spawn(async move {
            let runtime = async {
                calls
                    .begin()
                    .await
                    .map_err(TrustedLuaRuntimeExecutionError::Binding)?;
                calls
                    .execute(SqliteCommand::new(
                        "INSERT INTO lua_owned(value) VALUES (?)",
                        vec![SqliteValue::Text("自由译文".to_owned())],
                    ))
                    .await
                    .map_err(TrustedLuaRuntimeExecutionError::Binding)?;
                let response = translate
                    .request_llm(vec![ChatMessage::new(
                        ChatMessageRole::User,
                        "# Lua full messages",
                    )])
                    .await
                    .map_err(TrustedLuaRuntimeExecutionError::Binding)?;
                assert_eq!(response.content(), "lua raw response");
                calls
                    .commit()
                    .await
                    .map_err(TrustedLuaRuntimeExecutionError::Binding)?;
                Ok(())
            }
            .await;
            let finalization = finalizer.finalize().await;
            let _ = sender.send(TrustedLuaRuntimeExecutionReport::new(runtime, finalization));
        });
        TrustedLuaExecutionHandle::new(receiver, cancelled)
    }
}

fn non_zero(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("测试配置必须非零")
}

fn translation_resources() -> Arc<ResolvedRpgMakerTranslationResources> {
    let japanese: Arc<dyn LanguageModule> = Arc::new(JapaneseLanguageModule::new(
        JapaneseResidualPolicy::new(non_zero(1), Vec::new()).expect("测试日文残留策略应合法"),
        None,
    ));
    let pair = LanguagePair::new(
        LanguageId::parse("ja").expect("测试源语言合法"),
        LanguageId::parse("zh-Hans").expect("测试目标语言合法"),
    );
    let prompt = RpgMakerSystemPrompt::new(
        pair,
        "# 完整系统提示词\n\n只返回约定 JSON。".to_owned(),
        TranslationResponseEnvelope::JsonOnly,
    )
    .expect("测试 Prompt 应合法");
    Arc::new(ResolvedRpgMakerTranslationResources::new(prompt, japanese))
}

fn event_position(events: &[Event], predicate: impl Fn(&Event) -> bool) -> usize {
    events.iter().position(predicate).expect("预期事件应该发生")
}

struct FixedExecutionBuilder<P, S, L> {
    profile: Arc<RpgMakerTranslationProfile<P>>,
    standard: Mutex<Option<S>>,
    lua: Mutex<Option<crate::rpg_maker::SelectedLua<L>>>,
}

impl<P, S, L> SelectedTranslationExecutionBuilder for FixedExecutionBuilder<P, S, L>
where
    P: Send + Sync + 'static,
    S: StandardTranslation<Profile = Arc<RpgMakerTranslationProfile<P>>>,
    L: LuaTranslation<Client = P>,
{
    type Client = P;
    type Standard = S;
    type Lua = L;
    type Error = FakeRootError;

    async fn build(
        &self,
        _project: &crate::rpg_maker::project::OpenedProject,
    ) -> Result<SelectedTranslationExecution<P, S, L>, Self::Error> {
        let standard = self
            .standard
            .lock()
            .expect("标准翻译构造锁不应中毒")
            .take()
            .ok_or(FakeRootError("翻译执行切片已经构造"))?;
        let lua = self.lua.lock().expect("Lua 构造锁不应中毒").take();
        Ok(SelectedTranslationExecution::new(
            Arc::clone(&self.profile),
            standard,
            lua,
        ))
    }
}

#[derive(Clone, Copy)]
struct FakeProjectLeaseProvider;

impl ProjectCommandLeaseProvider for FakeProjectLeaseProvider {
    type Error = FakeRootError;
    type LeaseState = ();

    async fn acquire(
        &self,
        _project: &crate::rpg_maker::ProjectName,
    ) -> Result<ProjectCommandLease<Self::LeaseState>, ProjectCommandLeaseError<Self::Error>> {
        Ok(ProjectCommandLease::for_test(()))
    }
}

#[tokio::test]
async fn all_non_root_translation_services_reach_the_selected_root_fakes() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let cpu_calls = Arc::new(AtomicUsize::new(0));
    let transaction_calls = Arc::new(AtomicUsize::new(0));
    let standard_attempts = Arc::new(AtomicUsize::new(0));
    let delay_calls = Arc::new(AtomicUsize::new(0));
    let file_calls = Arc::new(AtomicUsize::new(0));
    let lua_runtime_calls = Arc::new(AtomicUsize::new(0));
    let session_factory_calls = Arc::new(AtomicUsize::new(0));
    let persistent_log_calls = Arc::new(AtomicUsize::new(0));

    let cpu = FakeCpuTaskExecutor {
        events: Arc::clone(&events),
        calls: Arc::clone(&cpu_calls),
    };
    let sqlite_query = FakeSqliteQueryExecutor {
        events: Arc::clone(&events),
    };
    let sqlite_transaction = FakeSqliteTransactionExecutor {
        events: Arc::clone(&events),
        calls: Arc::clone(&transaction_calls),
    };
    let llm = FakeLlmRequestExecutor {
        events: Arc::clone(&events),
        standard_attempts: Arc::clone(&standard_attempts),
    };
    let delay = FakeAsyncDelay {
        events: Arc::clone(&events),
        calls: Arc::clone(&delay_calls),
    };
    let file_reader = FakeFileReader {
        events: Arc::clone(&events),
        calls: Arc::clone(&file_calls),
    };
    let session_state = Arc::new(Mutex::new(FakeSessionState {
        transaction_active: false,
        closed: false,
    }));
    let session = FakeSqliteInteractiveSession {
        events: Arc::clone(&events),
        state: Arc::clone(&session_state),
    };
    let session_factory = FakeSqliteInteractiveSessionFactory {
        events: Arc::clone(&events),
        session,
        calls: Arc::clone(&session_factory_calls),
    };
    let lua_runtime = FakeTrustedLuaRuntimeExecutor {
        events: Arc::clone(&events),
        calls: Arc::clone(&lua_runtime_calls),
    };

    let planning = RpgMakerTranslationPlanningConfiguration::new(non_zero(10_000));
    let profile = Arc::new(RpgMakerTranslationProfile::new(
        "quality",
        planning,
        RpgMakerTranslationRequestConfiguration::new(
            vec![Duration::from_millis(7)],
            Duration::from_secs(1),
        ),
        Arc::new(FakeLlmClient {
            name: "shared-llm-config",
        }),
    ));

    let project_reader = ExistingProjectOpeningService::new(
        ProjectDatabaseRecordReadingService::new(
            PathBuf::from("C:/projects"),
            RpgMakerLayout::MZ,
            sqlite_query.clone(),
        ),
        FakeDirectoryResolver,
        FakeDirectoryTreeFingerprinter,
    );
    let asset_reader =
        RpgMakerStandardTranslationAssetReadingService::new(sqlite_query, cpu.clone());
    let languages = translation_resources();
    let resources =
        TranslationPlanningResourceReadingService::new(file_reader.clone(), cpu.clone());
    let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, FakeLlmClient>::new(
        resources,
        languages.clone(),
        Pcre2PlaceholderService::new().expect("内置占位符规格应可编译"),
        cpu.clone(),
    );
    let response_processor = TranslationTaskResponseProcessingService::new(cpu.clone(), languages);
    type SelectedProfile = Arc<RpgMakerTranslationProfile<FakeLlmClient>>;
    let cancellation = crate::execution::CooperativeCancellation::default();
    let executor = RpgMakerStandardTranslationTaskExecutionService::<_, _, _, SelectedProfile>::new(
        llm.clone(),
        delay,
        response_processor,
        cancellation.clone(),
    );
    let result_store =
        RpgMakerStandardTranslationResultStorageService::new(sqlite_transaction, cpu);
    let standard = StandardTranslationService::new(
        asset_reader,
        planner,
        executor,
        result_store,
        FakeTranslationLog {
            events: Arc::clone(&events),
            calls: Arc::clone(&persistent_log_calls),
        },
        cancellation,
    );

    let lua_host =
        TrustedLuaExecutionHostingService::with_llm(file_reader, llm, lua_runtime, session_factory);
    let execution_builder = FixedExecutionBuilder {
        profile,
        standard: Mutex::new(Some(standard)),
        lua: Mutex::new(Some(crate::rpg_maker::SelectedLua::new(
            crate::rpg_maker::lua::runtime::OwnedLuaProgram::new(
                PathBuf::from("C:/resolved/scripts/translate.lua"),
                b"return true".to_vec(),
            ),
            LuaTranslationService::new(lua_host),
        ))),
    };
    let use_case = TranslateService::new(
        project_reader,
        execution_builder,
        FakeProjectLeaseProvider,
        crate::execution::CooperativeCancellation::default(),
    );

    let completion = use_case
        .execute(TranslateInput {
            name: "demo".parse().expect("测试项目名称应合法"),
            terminology_path: None,
            placeholder_rules_path: None,
        })
        .await
        .expect("完整非根 Translate 树应该成功");
    let OperationCompletion::Completed(output) = completion else {
        panic!("测试未请求取消");
    };

    assert_eq!(output.name.as_str(), "demo");
    assert_eq!(output.profile_id, "quality");
    assert!(cpu_calls.load(Ordering::SeqCst) > 0);
    assert_eq!(transaction_calls.load(Ordering::SeqCst), 2);
    assert_eq!(standard_attempts.load(Ordering::SeqCst), 2);
    assert_eq!(delay_calls.load(Ordering::SeqCst), 1);
    assert_eq!(file_calls.load(Ordering::SeqCst), 0);
    assert_eq!(lua_runtime_calls.load(Ordering::SeqCst), 1);
    assert_eq!(session_factory_calls.load(Ordering::SeqCst), 1);
    assert_eq!(persistent_log_calls.load(Ordering::SeqCst), 2);
    assert!(session_state.lock().expect("会话状态锁不应中毒").closed);

    let events = events.lock().expect("事件锁不应中毒").clone();
    let metadata = event_position(&events, |event| matches!(event, Event::QueryMetadata));
    let assets = event_position(&events, |event| matches!(event, Event::QueryAssets));
    let preparation_transaction = event_position(&events, |event| {
        matches!(event, Event::PreparationTransaction)
    });
    let first_llm = event_position(&events, |event| {
        matches!(event, Event::LlmStandard { attempt: 1, .. })
    });
    let delay = event_position(&events, |event| matches!(event, Event::Delay(_)));
    let second_llm = event_position(&events, |event| {
        matches!(event, Event::LlmStandard { attempt: 2, .. })
    });
    let commit_transaction =
        event_position(&events, |event| matches!(event, Event::CommitTransaction));
    let log_start = event_position(&events, |event| matches!(event, Event::LogTaskStarted));
    let log_task = event_position(&events, |event| matches!(event, Event::LogTask));
    let open_lua = event_position(&events, |event| matches!(event, Event::OpenLuaDatabase(_)));
    let lua_runtime = event_position(&events, |event| matches!(event, Event::LuaRuntime));
    let lua_begin = event_position(&events, |event| matches!(event, Event::LuaBegin));
    let lua_execute = event_position(&events, |event| matches!(event, Event::LuaExecute));
    let lua_llm = event_position(&events, |event| matches!(event, Event::LlmLua { .. }));
    let lua_commit = event_position(&events, |event| matches!(event, Event::LuaCommit));
    let lua_inspect = event_position(&events, |event| matches!(event, Event::LuaInspect));
    let lua_close = event_position(&events, |event| matches!(event, Event::LuaClose));

    assert!(metadata < assets);
    assert!(assets < preparation_transaction && preparation_transaction < log_start);
    assert!(log_start < first_llm);
    assert!(first_llm < delay && delay < second_llm);
    assert!(second_llm < commit_transaction);
    assert!(commit_transaction < log_task && log_task < open_lua);
    assert!(open_lua < lua_runtime);
    assert!(lua_runtime < lua_begin && lua_begin < lua_execute);
    assert!(lua_execute < lua_llm && lua_llm < lua_commit);
    assert!(lua_commit < lua_inspect && lua_inspect < lua_close);

    let client_addresses = events
        .iter()
        .filter_map(|event| match event {
            Event::LlmStandard { client_address, .. } | Event::LlmLua { client_address } => {
                Some(*client_address)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(client_addresses.len(), 3);
    assert!(
        client_addresses
            .iter()
            .all(|address| *address == client_addresses[0]),
        "Standard 和 Lua 必须使用同一份 LLM Client 快照"
    );
}
