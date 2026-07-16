use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::asset_reader::{
    MzStandardTranslationAssetReadingConfig, MzStandardTranslationAssetReadingService,
};
use super::executor::{
    AsyncDelay, LlmFinishReason, LlmRequestError, LlmRequestExecutor, LlmResponse,
    MzStandardTranslationTaskExecutionService, TranslationTaskResponseProcessingService,
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
use super::standard::{
    ChatMessage, ChatMessageRole, StandardTranslationService, TranslationLogEvent,
};
use super::{TranslateInput, TranslateUseCase};
use crate::att_mz::location_codec::MzLocationCodec;
use crate::att_mz::lua::LuaPhase;
use crate::att_mz::lua::hosting::TrustedLuaExecutionHostingService;
use crate::att_mz::lua::runtime::{
    OwnedLuaProgram, TrustedLuaHostBindings, TrustedLuaRuntimeExecutionError,
    TrustedLuaRuntimeExecutionReport, TrustedLuaRuntimeExecutor, TrustedLuaRuntimeTermination,
};
use crate::att_mz::lua::session::{
    OpenSqliteInteractiveSessionError, SqliteInteractiveSession, SqliteInteractiveSessionError,
    SqliteInteractiveSessionFactory, SqliteInteractiveTransactionState,
};
use crate::att_mz::text::{MzLocation, MzLocationStep, MzSource, StandardDataFile};
use crate::language::{
    JapaneseLanguageModule, JapaneseResidualPolicy, LanguageModule, LanguageModuleCatalog,
};
use crate::observability::PersistentEventLog;
use crate::project_database::ProjectDatabaseRecordReadingService;
use crate::storage::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::storage::file_system::{FileReader, ReadFile, ReadFileError};
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
        profile_address: usize,
    },
    Delay(Duration),
    StandardTransaction,
    LogTask,
    LogCommitFailure,
    LogRun,
    ReadLua(PathBuf),
    OpenLuaDatabase(PathBuf),
    LuaRuntime,
    LuaBegin,
    LuaExecute,
    LlmLua {
        profile_address: usize,
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
        if query.statement().contains("FROM metadata") {
            record(&self.events, Event::QueryMetadata);
            assert!(query.parameters().is_empty());
            return Ok(vec![SqliteRow::new(vec![
                SqliteValue::Text("demo".to_owned()),
                SqliteValue::Text("ja".to_owned()),
                SqliteValue::Text("zh-Hans".to_owned()),
                SqliteValue::Integer(24),
                SqliteValue::Integer(30),
                SqliteValue::Integer(18),
            ])]);
        }

        assert!(query.statement().contains("UNION ALL"));
        assert!(
            query
                .statement()
                .contains("translation_terminology_dependency")
        );
        record(&self.events, Event::QueryAssets);
        Ok(vec![standard_asset_row(1), standard_asset_row(2)])
    }
}

fn standard_asset_row(index: usize) -> SqliteRow {
    let source = MzSource::data(StandardDataFile::Items);
    let group_location = MzLocation::value(source.clone(), vec![MzLocationStep::index(index)]);
    let exact_location = MzLocation::value(
        source,
        vec![MzLocationStep::index(index), MzLocationStep::key("name")],
    );

    SqliteRow::new(vec![
        SqliteValue::Text("entry".to_owned()),
        SqliteValue::Text(MzLocationCodec::encode(&exact_location).expect("测试位置应可编码")),
        SqliteValue::Text("builtin".to_owned()),
        SqliteValue::Text(MzLocationCodec::encode(&group_location).expect("测试位置应可编码")),
        SqliteValue::Text("name".to_owned()),
        SqliteValue::Null,
        SqliteValue::Text("魔法剣".to_owned()),
        SqliteValue::Null,
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
        assert_eq!(
            translated_leaf_count, 2,
            "一次代表译文必须在同一事务中扩散到两个物理位置"
        );
        self.calls.fetch_add(1, Ordering::SeqCst);
        record(&self.events, Event::StandardTransaction);
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct FakeLlmProfile {
    name: &'static str,
}

#[derive(Clone)]
struct FakeLlmRequestExecutor {
    events: EventLog,
    standard_attempts: Arc<AtomicUsize>,
}

impl LlmRequestExecutor for FakeLlmRequestExecutor {
    type Profile = FakeLlmProfile;
    type Error = FakeRootError;

    async fn request<'a>(
        &'a self,
        profile: &'a Self::Profile,
        messages: &'a [ChatMessage],
    ) -> Result<LlmResponse, LlmRequestError<Self::Error>> {
        assert_eq!(profile.name, "shared-llm-config");
        let profile_address = std::ptr::from_ref(profile).addr();
        let is_lua = messages
            .iter()
            .any(|message| message.content() == "# Lua full messages");

        if is_lua {
            record(&self.events, Event::LlmLua { profile_address });
            return Ok(LlmResponse::new(
                "lua raw response",
                LlmFinishReason::Stop,
                Some("lua-request".to_owned()),
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
                profile_address,
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

#[derive(Debug)]
struct FakeSessionState {
    transaction: SqliteInteractiveTransactionState,
    closed: bool,
}

#[derive(Clone)]
struct FakeSqliteInteractiveSession {
    events: EventLog,
    state: Arc<Mutex<FakeSessionState>>,
}

impl SqliteInteractiveSession for FakeSqliteInteractiveSession {
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
        assert_eq!(state.transaction, SqliteInteractiveTransactionState::Active);
        drop(state);
        record(&self.events, Event::LuaExecute);
        Ok(1)
    }

    async fn begin(&self) -> Result<(), SqliteInteractiveSessionError<Self::Error>> {
        let mut state = self.state.lock().expect("会话状态锁不应中毒");
        if state.transaction == SqliteInteractiveTransactionState::Active {
            return Err(SqliteInteractiveSessionError::TransactionAlreadyActive);
        }
        state.transaction = SqliteInteractiveTransactionState::Active;
        drop(state);
        record(&self.events, Event::LuaBegin);
        Ok(())
    }

    async fn commit(&self) -> Result<(), SqliteInteractiveSessionError<Self::Error>> {
        let mut state = self.state.lock().expect("会话状态锁不应中毒");
        if state.transaction == SqliteInteractiveTransactionState::Idle {
            return Err(SqliteInteractiveSessionError::NoActiveTransaction);
        }
        state.transaction = SqliteInteractiveTransactionState::Idle;
        drop(state);
        record(&self.events, Event::LuaCommit);
        Ok(())
    }

    async fn rollback(&self) -> Result<(), SqliteInteractiveSessionError<Self::Error>> {
        let mut state = self.state.lock().expect("会话状态锁不应中毒");
        if state.transaction == SqliteInteractiveTransactionState::Idle {
            return Err(SqliteInteractiveSessionError::NoActiveTransaction);
        }
        state.transaction = SqliteInteractiveTransactionState::Idle;
        Ok(())
    }

    async fn transaction_state(
        &self,
    ) -> Result<SqliteInteractiveTransactionState, SqliteInteractiveSessionError<Self::Error>> {
        record(&self.events, Event::LuaInspect);
        Ok(self.state.lock().expect("会话状态锁不应中毒").transaction)
    }

    async fn close(&self) -> Result<(), SqliteInteractiveSessionError<Self::Error>> {
        let mut state = self.state.lock().expect("会话状态锁不应中毒");
        assert_eq!(state.transaction, SqliteInteractiveTransactionState::Idle);
        state.closed = true;
        drop(state);
        record(&self.events, Event::LuaClose);
        Ok(())
    }
}

#[derive(Clone)]
struct FakeSqliteInteractiveSessionFactory {
    events: EventLog,
    session: FakeSqliteInteractiveSession,
    calls: Arc<AtomicUsize>,
}

impl SqliteInteractiveSessionFactory for FakeSqliteInteractiveSessionFactory {
    type Session = FakeSqliteInteractiveSession;
    type Error = FakeRootError;

    async fn open_existing(
        &self,
        path: PathBuf,
    ) -> Result<Self::Session, OpenSqliteInteractiveSessionError<Self::Error>> {
        assert_eq!(
            path,
            PathBuf::from("C:/projects").join("demo").join("project.db")
        );
        self.calls.fetch_add(1, Ordering::SeqCst);
        record(&self.events, Event::OpenLuaDatabase(path));
        Ok(self.session.clone())
    }
}

#[derive(Clone)]
struct FakeTrustedLuaRuntimeExecutor {
    events: EventLog,
    calls: Arc<AtomicUsize>,
}

impl TrustedLuaRuntimeExecutor for FakeTrustedLuaRuntimeExecutor {
    type Error = FakeRootError;

    async fn execute<B>(
        &self,
        program: OwnedLuaProgram,
        bindings: Arc<B>,
    ) -> TrustedLuaRuntimeExecutionReport<Self::Error, B::Error>
    where
        B: TrustedLuaHostBindings,
    {
        self.calls.fetch_add(1, Ordering::SeqCst);
        record(&self.events, Event::LuaRuntime);
        assert_eq!(
            program.main_script_path(),
            std::path::Path::new("C:/resolved/scripts/translate.lua")
        );
        assert_eq!(program.source(), b"return true");
        assert_eq!(bindings.phase(), LuaPhase::Translate);
        assert_eq!(bindings.project().name().as_str(), "demo");

        let runtime = async {
            bindings
                .begin()
                .await
                .map_err(TrustedLuaRuntimeExecutionError::Binding)?;
            bindings
                .execute(SqliteCommand::new(
                    "INSERT INTO lua_owned(value) VALUES (?)",
                    vec![SqliteValue::Text("自由译文".to_owned())],
                ))
                .await
                .map_err(TrustedLuaRuntimeExecutionError::Binding)?;
            let response = bindings
                .request_llm(vec![ChatMessage::new(
                    ChatMessageRole::User,
                    "# Lua full messages",
                )])
                .await
                .map_err(TrustedLuaRuntimeExecutionError::Binding)?;
            assert_eq!(response.content(), "lua raw response");
            bindings
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
        let finalization = bindings.finalize(termination).await;
        TrustedLuaRuntimeExecutionReport::new(runtime, finalization)
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
        transaction: SqliteInteractiveTransactionState::Idle,
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
        FakeLlmProfile {
            name: "shared-llm-config",
        },
    );
    let resolver = InMemoryTranslationExecutionProfileResolver::new(
        TranslationProfileCatalog::new([TranslationExecutionProfile::new(
            "quality",
            non_zero(1),
            payload,
        )])
        .expect("测试 Profile 目录应合法"),
    );

    let project_reader = ProjectDatabaseRecordReadingService::new(
        PathBuf::from("C:/projects"),
        sqlite_query.clone(),
    );
    let asset_reader = MzStandardTranslationAssetReadingService::new(
        sqlite_query,
        cpu.clone(),
        MzStandardTranslationAssetReadingConfig::new(non_zero(1), non_zero(1)),
    );
    let languages = language_catalog();
    let resources =
        JsonTranslationPlanningResourceReadingService::new(file_reader.clone(), cpu.clone());
    let planner = MzStandardTranslationTaskPlanningService::<_, _, FakeLlmProfile>::new(
        resources,
        languages.clone(),
        Pcre2PlaceholderService::new().expect("内置占位符规格应可编译"),
        cpu.clone(),
    );
    let response_processor = TranslationTaskResponseProcessingService::new(cpu.clone(), languages);
    type SelectedProfile =
        Arc<TranslationExecutionProfile<MzTranslationExecutionPayload<FakeLlmProfile>>>;
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
    );

    let lua_host =
        TrustedLuaExecutionHostingService::new(file_reader, llm, lua_runtime, session_factory);
    let use_case = TranslateService::new(
        resolver,
        project_reader,
        standard,
        LuaTranslationService::new(lua_host),
    );

    let output = use_case
        .execute(TranslateInput {
            name: "demo".parse().expect("测试项目名称应合法"),
            llm_id: "quality".to_owned(),
            terminology_path: None,
            placeholder_rules_path: None,
            lua_script: Some(PathBuf::from("scripts/translate.lua")),
        })
        .await
        .expect("完整非根 Translate 树应该成功");

    assert_eq!(output.name.as_str(), "demo");
    assert_eq!(output.llm_id, "quality");
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
    let first_llm = event_position(&events, |event| {
        matches!(event, Event::LlmStandard { attempt: 1, .. })
    });
    let delay = event_position(&events, |event| matches!(event, Event::Delay(_)));
    let second_llm = event_position(&events, |event| {
        matches!(event, Event::LlmStandard { attempt: 2, .. })
    });
    let standard_transaction =
        event_position(&events, |event| matches!(event, Event::StandardTransaction));
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
    assert!(second_llm < standard_transaction);
    assert!(standard_transaction < log_task && log_task < log_run);
    assert!(log_run < read_lua);
    assert!(read_lua < open_lua && open_lua < lua_runtime);
    assert!(lua_runtime < lua_begin && lua_begin < lua_execute);
    assert!(lua_execute < lua_llm && lua_llm < lua_commit);
    assert!(lua_commit < lua_inspect && lua_inspect < lua_close);

    let profile_addresses = events
        .iter()
        .filter_map(|event| match event {
            Event::LlmStandard {
                profile_address, ..
            }
            | Event::LlmLua { profile_address } => Some(*profile_address),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(profile_addresses.len(), 3);
    assert!(
        profile_addresses
            .iter()
            .all(|address| *address == profile_addresses[0]),
        "Standard 和 Lua 必须使用同一份 LLM Profile 快照"
    );
}
