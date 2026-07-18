use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::asset_reader::MzStandardTranslationAssetReadingService;
use super::executor::{
    AsyncDelay, MzStandardTranslationTaskExecutionService, TranslationTaskResponseProcessingService,
};
use super::lua::LuaTranslationService;
use super::placeholder::Pcre2PlaceholderService;
use super::planner::MzStandardTranslationTaskPlanningService;
use super::planning_resource::JsonTranslationPlanningResourceReadingService;
use super::profile::{
    InMemoryTranslationExecutionProfileResolver, MzTranslationExecutionConfiguration,
    MzTranslationExecutionPayload, MzTranslationPlanningConfiguration, TranslationExecutionProfile,
    TranslationProfileCatalog, TranslationProfileLanguagePair,
};
use super::result_store::{
    MzStandardTranslationResultStorageConfig, MzStandardTranslationResultStorageService,
};
use super::service::TranslateService;
use super::standard::{StandardTranslationService, TranslationLogEvent};
use super::{TranslateInput, TranslateUseCase};
use crate::att_mz::location_codec::MzLocationCodec;
use crate::att_mz::lua::LuaPhase;
use crate::att_mz::lua::hosting::TrustedLuaExecutionHostingService;
use crate::att_mz::lua::runtime::{
    OwnedLuaProgram, TrustedLuaExecutionHandle, TrustedLuaPhaseBindings, TrustedLuaRuntimeBindings,
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
use crate::att_mz::project::ExistingProjectOpeningService;
use crate::att_mz::standard_asset::MzStandardAssetReadingConfig;
use crate::att_mz::text::{MzLocation, MzLocationStep, MzSource, StandardDataFile};
use crate::fingerprint::Sha256Fingerprint;
use crate::language::{
    JapaneseLanguageModule, JapaneseResidualPolicy, LanguageModule, LanguageModuleCatalog,
};
use crate::llm::{
    ChatMessage, ChatMessageRole, LlmClientSemanticIdentity, LlmFinishReason, LlmRequestError,
    LlmRequestExecutor, LlmResponse,
};
use crate::observability::PersistentEventLog;
use crate::project_database::ProjectDatabaseRecordReadingService;
use crate::storage::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::storage::file_system::{
    DirectoryEntry, DirectoryLister, DirectoryTreeFingerprintError,
    DirectoryTreeFingerprintRequest, DirectoryTreeFingerprinter, ExistingDirectoryResolver,
    FileReader, ListDirectoryError, ReadFile, ReadFileError, ResolveDirectoryError,
};
use crate::storage::sqlite::{
    ExecuteTransactionError, QueryExistingDatabaseError, SqliteCommand, SqliteQuery,
    SqliteQueryExecutor, SqliteRow, SqliteTransactionExecutor, SqliteTransactionPlan,
    SqliteTransactionStep, SqliteValue,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FakeRootError(&'static str);

impl fmt::Display for FakeRootError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for FakeRootError {}

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
    LogTask,
    LogCommitFailure,
    LogRun,
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
struct FakePersistentEventLog {
    events: EventLog,
    calls: Arc<AtomicUsize>,
}

impl PersistentEventLog<TranslationLogEvent> for FakePersistentEventLog {
    type Error = FakeRootError;

    async fn append(&self, event: TranslationLogEvent) -> Result<(), Self::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        record(
            &self.events,
            match event {
                TranslationLogEvent::TaskProcessed(_) => Event::LogTask,
                TranslationLogEvent::TaskCommitFailed(_) => Event::LogCommitFailure,
                TranslationLogEvent::RunCompleted(_) => Event::LogRun,
            },
        );
        Ok(())
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
            PathBuf::from("C:/projects").join("demo").join("project.db")
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
            ])]);
        }

        assert!(query.statement().contains("UNION ALL"));
        assert!(query.statement().contains("standard_translation_resource"));
        record(&self.events, Event::QueryAssets);
        Ok(vec![
            metadata_snapshot_row(),
            owner_snapshot_row(),
            resource_snapshot_row("placeholder_rules"),
            resource_snapshot_row("terminology"),
            standard_asset_row(1),
            standard_asset_row(2),
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
    SqliteRow::new(vec![
        SqliteValue::Text("0_metadata".to_owned()),
        SqliteValue::Null,
        SqliteValue::Blob(vec![0xa5; 32]),
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Null,
    ])
}

fn owner_snapshot_row() -> SqliteRow {
    SqliteRow::new(vec![
        SqliteValue::Text("1_owner".to_owned()),
        SqliteValue::Text("builtin".to_owned()),
        SqliteValue::Blob(vec![0xa5; 32]),
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Null,
    ])
}

fn resource_snapshot_row(kind: &str) -> SqliteRow {
    SqliteRow::new(vec![
        SqliteValue::Text("2_resource".to_owned()),
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Text(kind.to_owned()),
        SqliteValue::Text("[]".to_owned()),
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Null,
    ])
}

fn standard_asset_row(index: usize) -> SqliteRow {
    let source = MzSource::data(StandardDataFile::Items);
    let group_location = MzLocation::value(source.clone(), vec![MzLocationStep::index(index)]);
    let exact_location = MzLocation::value(
        source,
        vec![MzLocationStep::index(index), MzLocationStep::key("name")],
    );

    SqliteRow::new(vec![
        SqliteValue::Text("3_asset".to_owned()),
        SqliteValue::Text("builtin".to_owned()),
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Null,
        SqliteValue::Text("entry".to_owned()),
        SqliteValue::Text(MzLocationCodec::encode(&exact_location).expect("测试位置应可编码")),
        SqliteValue::Text(MzLocationCodec::encode(&group_location).expect("测试位置应可编码")),
        SqliteValue::Text("name".to_owned()),
        SqliteValue::Null,
        SqliteValue::Text("魔法剣".to_owned()),
        SqliteValue::Null,
        SqliteValue::Null,
    ])
}

#[derive(Clone)]
struct FakeSqliteTransactionExecutor {
    events: EventLog,
    calls: Arc<AtomicUsize>,
}

impl SqliteTransactionExecutor for FakeSqliteTransactionExecutor {
    type Error = FakeRootError;

    async fn execute_transaction(
        &self,
        path: PathBuf,
        plan: SqliteTransactionPlan,
    ) -> Result<(), ExecuteTransactionError<Self::Error>> {
        assert_eq!(
            path,
            PathBuf::from("C:/projects").join("demo").join("project.db")
        );
        let translated_leaf_count = plan
            .steps()
            .iter()
            .filter(|step| match step {
                SqliteTransactionStep::Execute(command) => command
                    .parameters()
                    .contains(&SqliteValue::Text("魔法剑".to_owned())),
                SqliteTransactionStep::ExecuteMany(_)
                | SqliteTransactionStep::RequireNoRows { .. } => false,
            })
            .count();
        let event = if translated_leaf_count == 0 {
            assert!(plan.steps().iter().any(|step| matches!(
                step,
                SqliteTransactionStep::Execute(command)
                    if command.statement().contains("standard_translation_resource")
            )));
            Event::PreparationTransaction
        } else {
            assert_eq!(
                translated_leaf_count, 2,
                "一次代表译文必须在同一事务中扩散到两个物理位置"
            );
            Event::CommitTransaction
        };
        self.calls.fetch_add(1, Ordering::SeqCst);
        record(&self.events, event);
        Ok(())
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
                "lua-response",
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
            r#"[{"id":"0","translation":"魔法剑"}]"#,
            LlmFinishReason::Stop,
            Some("standard-request".to_owned()),
            "standard-response",
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
    type Error = FakeRootError;

    async fn wait(&self, duration: Duration) -> Result<(), Self::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        record(&self.events, Event::Delay(duration));
        Ok(())
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
}

struct FakeSqliteInteractiveSessionFinalizer {
    events: EventLog,
    state: Arc<Mutex<FakeSessionState>>,
}

impl SqliteInteractiveSessionFinalizer for FakeSqliteInteractiveSessionFinalizer {
    type Error = FakeRootError;

    async fn finalize(self) -> SqliteInteractiveSessionFinalizationReport<Self::Error> {
        record(&self.events, Event::LuaInspect);
        let mut state = self.state.lock().expect("会话状态锁不应中毒");
        let transaction = if state.transaction_active {
            state.transaction_active = false;
            SqliteInteractiveTransactionObservation::Active
        } else {
            SqliteInteractiveTransactionObservation::Idle
        };
        state.closed = true;
        drop(state);
        record(&self.events, Event::LuaClose);
        let rollback = if matches!(transaction, SqliteInteractiveTransactionObservation::Active) {
            SqliteInteractiveRollbackOutcome::RolledBack
        } else {
            SqliteInteractiveRollbackOutcome::NotRequired
        };
        SqliteInteractiveSessionFinalizationReport::new(
            transaction,
            rollback,
            SqliteInteractiveConnectionCloseOutcome::Closed,
        )
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
            PathBuf::from("C:/projects").join("demo").join("project.db")
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
    type Reservation = FakeTrustedLuaRuntimeReservation;

    async fn reserve(&self) -> Result<Self::Reservation, Self::Error> {
        Ok(FakeTrustedLuaRuntimeReservation {
            events: Arc::clone(&self.events),
            calls: Arc::clone(&self.calls),
        })
    }
}

struct FakeTrustedLuaRuntimeReservation {
    events: EventLog,
    calls: Arc<AtomicUsize>,
}

impl TrustedLuaRuntimeReservation for FakeTrustedLuaRuntimeReservation {
    type Error = FakeRootError;

    fn start(
        self,
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
        assert_eq!(calls.project().name().as_str(), "demo");
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

fn non_zero(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("测试配置必须非零")
}

fn language_catalog() -> LanguageModuleCatalog {
    let japanese: Arc<dyn LanguageModule> = Arc::new(JapaneseLanguageModule::new(
        JapaneseResidualPolicy::new(non_zero(1), Vec::new()).expect("测试日文残留策略应合法"),
        None,
    ));
    LanguageModuleCatalog::new([("ja".to_owned(), japanese)]).expect("测试语言目录应合法")
}

fn event_position(events: &[Event], predicate: impl Fn(&Event) -> bool) -> usize {
    events.iter().position(predicate).expect("预期事件应该发生")
}

#[tokio::test]
async fn all_non_root_translation_services_reach_the_nine_root_fakes() {
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

    let pair = TranslationProfileLanguagePair::new("ja", "zh-Hans").expect("测试语言对应合法");
    let planning = MzTranslationPlanningConfiguration::new(
        non_zero(1),
        non_zero(10_000),
        [(pair, "# 完整系统提示词\n\n只返回约定 JSON。".to_owned())],
    )
    .expect("测试规划配置应合法");
    let payload = MzTranslationExecutionPayload::new(
        planning,
        MzTranslationExecutionConfiguration::new(
            vec![Duration::from_millis(7)],
            Duration::from_secs(1),
        ),
        Arc::new(FakeLlmClient {
            name: "shared-llm-config",
        }),
    );
    let resolver = InMemoryTranslationExecutionProfileResolver::new(
        TranslationProfileCatalog::new([TranslationExecutionProfile::new(
            "quality",
            non_zero(1),
            payload,
        )])
        .expect("测试 Profile 目录应合法"),
    );

    let project_reader = ExistingProjectOpeningService::new(
        ProjectDatabaseRecordReadingService::new(
            PathBuf::from("C:/projects"),
            sqlite_query.clone(),
        ),
        FakeDirectoryResolver,
        FakeDirectoryTreeFingerprinter,
    );
    let asset_reader = MzStandardTranslationAssetReadingService::new(
        sqlite_query,
        cpu.clone(),
        MzStandardAssetReadingConfig::new(non_zero(1), non_zero(1)),
    );
    let languages = language_catalog();
    let resources =
        JsonTranslationPlanningResourceReadingService::new(file_reader.clone(), cpu.clone());
    let planner = MzStandardTranslationTaskPlanningService::<_, _, FakeLlmClient>::new(
        resources,
        languages.clone(),
        Pcre2PlaceholderService::new().expect("内置占位符规格应可编译"),
        cpu.clone(),
    );
    let response_processor = TranslationTaskResponseProcessingService::new(cpu.clone(), languages);
    type SelectedProfile =
        Arc<TranslationExecutionProfile<MzTranslationExecutionPayload<FakeLlmClient>>>;
    let executor = MzStandardTranslationTaskExecutionService::<_, _, _, SelectedProfile>::new(
        llm.clone(),
        delay,
        response_processor,
    );
    let result_store = MzStandardTranslationResultStorageService::new(
        sqlite_transaction,
        cpu,
        MzStandardTranslationResultStorageConfig::new(non_zero(1), non_zero(1)),
    );
    let standard = StandardTranslationService::new(
        asset_reader,
        planner,
        executor,
        result_store,
        FakePersistentEventLog {
            events: Arc::clone(&events),
            calls: Arc::clone(&persistent_log_calls),
        },
        crate::execution::CooperativeCancellation::default(),
    );

    let lua_host =
        TrustedLuaExecutionHostingService::with_llm(file_reader, llm, lua_runtime, session_factory);
    let use_case = TranslateService::new(
        resolver,
        project_reader,
        standard,
        Some(LuaTranslationService::new(lua_host)),
        crate::execution::CooperativeCancellation::default(),
    );

    let output = use_case
        .execute(TranslateInput {
            name: "demo".parse().expect("测试项目名称应合法"),
            profile_id: "quality".to_owned(),
            terminology_path: None,
            placeholder_rules_path: None,
            lua_script: Some(PathBuf::from("scripts/translate.lua")),
        })
        .await
        .expect("完整非根 Translate 树应该成功");

    assert_eq!(output.name.as_str(), "demo");
    assert_eq!(output.profile_id, "quality");
    assert!(cpu_calls.load(Ordering::SeqCst) > 0);
    assert_eq!(transaction_calls.load(Ordering::SeqCst), 1);
    assert_eq!(standard_attempts.load(Ordering::SeqCst), 2);
    assert_eq!(delay_calls.load(Ordering::SeqCst), 1);
    assert_eq!(file_calls.load(Ordering::SeqCst), 1);
    assert_eq!(lua_runtime_calls.load(Ordering::SeqCst), 1);
    assert_eq!(session_factory_calls.load(Ordering::SeqCst), 1);
    assert_eq!(persistent_log_calls.load(Ordering::SeqCst), 2);
    assert!(session_state.lock().expect("会话状态锁不应中毒").closed);

    let events = events.lock().expect("事件锁不应中毒").clone();
    let metadata = event_position(&events, |event| matches!(event, Event::QueryMetadata));
    let assets = event_position(&events, |event| matches!(event, Event::QueryAssets));
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, Event::PreparationTransaction)),
        "资源和待清状态均未变化时不应提交 Preparation 事务"
    );
    let first_llm = event_position(&events, |event| {
        matches!(event, Event::LlmStandard { attempt: 1, .. })
    });
    let delay = event_position(&events, |event| matches!(event, Event::Delay(_)));
    let second_llm = event_position(&events, |event| {
        matches!(event, Event::LlmStandard { attempt: 2, .. })
    });
    let commit_transaction =
        event_position(&events, |event| matches!(event, Event::CommitTransaction));
    let log_task = event_position(&events, |event| matches!(event, Event::LogTask));
    let log_run = event_position(&events, |event| matches!(event, Event::LogRun));
    let read_lua = event_position(&events, |event| matches!(event, Event::ReadLua(_)));
    let open_lua = event_position(&events, |event| matches!(event, Event::OpenLuaDatabase(_)));
    let lua_runtime = event_position(&events, |event| matches!(event, Event::LuaRuntime));
    let lua_begin = event_position(&events, |event| matches!(event, Event::LuaBegin));
    let lua_execute = event_position(&events, |event| matches!(event, Event::LuaExecute));
    let lua_llm = event_position(&events, |event| matches!(event, Event::LlmLua { .. }));
    let lua_commit = event_position(&events, |event| matches!(event, Event::LuaCommit));
    let lua_inspect = event_position(&events, |event| matches!(event, Event::LuaInspect));
    let lua_close = event_position(&events, |event| matches!(event, Event::LuaClose));

    assert!(metadata < assets);
    assert!(assets < first_llm);
    assert!(first_llm < delay && delay < second_llm);
    assert!(second_llm < commit_transaction);
    assert!(commit_transaction < log_task && log_task < log_run);
    assert!(log_run < read_lua);
    assert!(read_lua < open_lua && open_lua < lua_runtime);
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
