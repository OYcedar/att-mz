//! 可信 Lua 程序的项目上下文、数据库、LLM 与资源终态编排。

use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use crate::att_mz::translate::executor::{
    LlmRequestError, LlmRequestExecutor, LlmResponse, TranslationTaskExecutionProfile,
};
use crate::att_mz::translate::profile::{
    MzTranslationExecutionPayload, TranslationExecutionProfile,
};
use crate::att_mz::translate::standard::ChatMessage;
use crate::storage::file_system::{FileReader, ReadFileError};
use crate::storage::sqlite::{SqliteCommand, SqliteQuery, SqliteRow};

use super::runtime::{
    OwnedLuaProgram, TrustedLuaBindingFinalization, TrustedLuaHostBindings,
    TrustedLuaRuntimeExecutionError, TrustedLuaRuntimeExecutor, TrustedLuaRuntimeTermination,
};
use super::session::{
    OpenSqliteInteractiveSessionError, SqliteInteractiveSession, SqliteInteractiveSessionError,
    SqliteInteractiveSessionFactory, SqliteInteractiveTransactionState,
};
use super::{LuaInvocation, LuaPhase, LuaProjectContext, TrustedLuaExecutionHost};

/// 使用四个根能力完成可信 Lua 程序生命周期。
pub(crate) struct TrustedLuaExecutionHostingService<F, L, R, S> {
    file_reader: F,
    llm: Arc<L>,
    runtime: R,
    session_factory: S,
}

impl<F, L, R, S> TrustedLuaExecutionHostingService<F, L, R, S> {
    pub(crate) fn new(file_reader: F, llm: L, runtime: R, session_factory: S) -> Self {
        Self {
            file_reader,
            llm: Arc::new(llm),
            runtime,
            session_factory,
        }
    }
}

impl<F, L, R, S> TrustedLuaExecutionHost for TrustedLuaExecutionHostingService<F, L, R, S>
where
    F: FileReader,
    L: LlmRequestExecutor + 'static,
    R: TrustedLuaRuntimeExecutor,
    S: SqliteInteractiveSessionFactory,
{
    type TranslationProfile =
        TranslationExecutionProfile<MzTranslationExecutionPayload<L::Profile>>;
    type Error = TrustedLuaExecutionHostingError<
        F::Error,
        S::Error,
        R::Error,
        LuaHostBindingError<<S::Session as SqliteInteractiveSession>::Error, L::Error>,
    >;

    async fn execute(
        &self,
        invocation: LuaInvocation<Self::TranslationProfile>,
    ) -> Result<(), Self::Error> {
        let (phase, script_path, project, profile) = match invocation {
            LuaInvocation::Extract {
                script_path,
                project,
            } => (LuaPhase::Extract, script_path, project, None),
            LuaInvocation::Translate {
                script_path,
                project,
                profile,
            } => (LuaPhase::Translate, script_path, project, Some(profile)),
            LuaInvocation::WriteBack {
                script_path,
                project,
            } => (LuaPhase::WriteBack, script_path, project, None),
        };

        let requested_script_path = script_path.clone();
        let read_file = self
            .file_reader
            .read_file(script_path)
            .await
            .map_err(|source| TrustedLuaExecutionHostingError::ReadScript {
                script_path: requested_script_path,
                source,
            })?;
        let program = OwnedLuaProgram::new(
            read_file.resolved_path().to_path_buf(),
            read_file.into_bytes(),
        );

        let database_path = project.database_path().to_path_buf();
        let session = self
            .session_factory
            .open_existing(database_path.clone())
            .await
            .map_err(|source| TrustedLuaExecutionHostingError::OpenDatabase {
                database_path,
                source,
            })?;

        let bindings = Arc::new(LuaHostBindings {
            phase,
            project,
            profile,
            session,
            llm: Arc::clone(&self.llm),
        });
        let (runtime, finalization) = self.runtime.execute(program, bindings).await.into_parts();

        match (runtime, finalization) {
            (Ok(()), Ok(finalization)) if finalization.had_active_transaction() => {
                Err(TrustedLuaExecutionHostingError::UnclosedTransaction)
            }
            (Ok(()), Ok(_)) => Ok(()),
            (Ok(()), Err(cleanup)) => Err(TrustedLuaExecutionHostingError::Cleanup(cleanup)),
            (Err(runtime), Ok(_)) => Err(TrustedLuaExecutionHostingError::Runtime(runtime)),
            (Err(runtime), Err(cleanup)) => {
                Err(TrustedLuaExecutionHostingError::RuntimeAndCleanup { runtime, cleanup })
            }
        }
    }
}

struct LuaHostBindings<S, L>
where
    L: LlmRequestExecutor + 'static,
{
    phase: LuaPhase,
    project: LuaProjectContext,
    profile: Option<Arc<TranslationExecutionProfile<MzTranslationExecutionPayload<L::Profile>>>>,
    session: S,
    llm: Arc<L>,
}

impl<S, L> TrustedLuaHostBindings for LuaHostBindings<S, L>
where
    S: SqliteInteractiveSession,
    L: LlmRequestExecutor + 'static,
{
    type Error = LuaHostBindingError<S::Error, L::Error>;

    fn phase(&self) -> LuaPhase {
        self.phase
    }

    fn project(&self) -> &LuaProjectContext {
        &self.project
    }

    async fn query(&self, query: SqliteQuery) -> Result<Vec<SqliteRow>, Self::Error> {
        self.session
            .query(query)
            .await
            .map_err(LuaHostBindingError::Database)
    }

    async fn execute(&self, command: SqliteCommand) -> Result<u64, Self::Error> {
        self.session
            .execute(command)
            .await
            .map_err(LuaHostBindingError::Database)
    }

    async fn begin(&self) -> Result<(), Self::Error> {
        self.session
            .begin()
            .await
            .map_err(LuaHostBindingError::Database)
    }

    async fn commit(&self) -> Result<(), Self::Error> {
        self.session
            .commit()
            .await
            .map_err(LuaHostBindingError::Database)
    }

    async fn rollback(&self) -> Result<(), Self::Error> {
        self.session
            .rollback()
            .await
            .map_err(LuaHostBindingError::Database)
    }

    async fn request_llm(&self, messages: Vec<ChatMessage>) -> Result<LlmResponse, Self::Error> {
        let profile = self
            .profile
            .as_ref()
            .ok_or(LuaHostBindingError::LlmUnavailable)?;
        self.llm
            .request(profile.llm_profile(), &messages)
            .await
            .map_err(LuaHostBindingError::LlmRequest)
    }

    async fn finalize(
        &self,
        _termination: TrustedLuaRuntimeTermination,
    ) -> Result<TrustedLuaBindingFinalization, Self::Error> {
        let mut failures = Vec::new();
        let state = match self.session.transaction_state().await {
            Ok(state) => Some(state),
            Err(error) => {
                failures.push(error);
                None
            }
        };
        let had_active_transaction = state == Some(SqliteInteractiveTransactionState::Active);

        if had_active_transaction && let Err(error) = self.session.rollback().await {
            failures.push(error);
        }
        if let Err(error) = self.session.close().await {
            failures.push(error);
        }

        if failures.is_empty() {
            Ok(TrustedLuaBindingFinalization::new(had_active_transaction))
        } else {
            Err(LuaHostBindingError::Cleanup(LuaSessionCleanupError::new(
                failures,
            )))
        }
    }
}

/// Lua VM 访问 Host 注入能力时的失败。
#[derive(Debug)]
pub(crate) enum LuaHostBindingError<S, L> {
    Database(SqliteInteractiveSessionError<S>),
    LlmUnavailable,
    LlmRequest(LlmRequestError<L>),
    Cleanup(LuaSessionCleanupError<S>),
}

impl<S, L> fmt::Display for LuaHostBindingError<S, L>
where
    S: fmt::Display,
    L: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(source) => write!(formatter, "Lua 数据库调用失败：{source}"),
            Self::LlmUnavailable => formatter.write_str("当前 Lua 阶段没有 ctx.llm"),
            Self::LlmRequest(source) => write!(formatter, "Lua LLM 调用失败：{source}"),
            Self::Cleanup(source) => source.fmt(formatter),
        }
    }
}

impl<S, L> Error for LuaHostBindingError<S, L>
where
    S: Error + 'static,
    L: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(source) => Some(source),
            Self::LlmUnavailable => None,
            Self::LlmRequest(source) => Some(source),
            Self::Cleanup(source) => Some(source),
        }
    }
}

/// 关闭会话期间可能同时发生的多个错误。
#[derive(Debug)]
pub(crate) struct LuaSessionCleanupError<E> {
    failures: Vec<SqliteInteractiveSessionError<E>>,
}

impl<E> LuaSessionCleanupError<E> {
    fn new(failures: Vec<SqliteInteractiveSessionError<E>>) -> Self {
        debug_assert!(!failures.is_empty());
        Self { failures }
    }

    pub(crate) fn failures(&self) -> &[SqliteInteractiveSessionError<E>] {
        &self.failures
    }
}

impl<E> fmt::Display for LuaSessionCleanupError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Lua 数据库会话清理失败")?;
        for failure in &self.failures {
            write!(formatter, "；{failure}")?;
        }
        Ok(())
    }
}

impl<E> Error for LuaSessionCleanupError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.failures.first().map(|error| error as _)
    }
}

/// Host 在脚本加载、数据库建立、VM 或收尾阶段遇到的失败。
#[derive(Debug)]
pub(crate) enum TrustedLuaExecutionHostingError<F, O, R, B> {
    ReadScript {
        script_path: PathBuf,
        source: ReadFileError<F>,
    },
    OpenDatabase {
        database_path: PathBuf,
        source: OpenSqliteInteractiveSessionError<O>,
    },
    Runtime(TrustedLuaRuntimeExecutionError<R, B>),
    Cleanup(B),
    UnclosedTransaction,
    RuntimeAndCleanup {
        runtime: TrustedLuaRuntimeExecutionError<R, B>,
        cleanup: B,
    },
}

impl<F, O, R, B> fmt::Display for TrustedLuaExecutionHostingError<F, O, R, B>
where
    F: fmt::Display,
    O: fmt::Display,
    R: fmt::Display,
    B: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadScript {
                script_path,
                source,
            } => write!(
                formatter,
                "无法读取可信 Lua 主程序 {}：{source}",
                script_path.display()
            ),
            Self::OpenDatabase {
                database_path,
                source,
            } => write!(
                formatter,
                "无法为可信 Lua 打开项目数据库 {}：{source}",
                database_path.display()
            ),
            Self::Runtime(source) => source.fmt(formatter),
            Self::Cleanup(source) => source.fmt(formatter),
            Self::UnclosedTransaction => {
                formatter.write_str("Lua 主程序正常结束时仍有未关闭事务；已回滚")
            }
            Self::RuntimeAndCleanup { runtime, cleanup } => {
                write!(formatter, "{runtime}；随后清理失败：{cleanup}")
            }
        }
    }
}

impl<F, O, R, B> Error for TrustedLuaExecutionHostingError<F, O, R, B>
where
    F: Error + 'static,
    O: Error + 'static,
    R: Error + 'static,
    B: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadScript { source, .. } => Some(source),
            Self::OpenDatabase { source, .. } => Some(source),
            Self::Runtime(source) => Some(source),
            Self::Cleanup(source) => Some(source),
            Self::UnclosedTransaction => None,
            Self::RuntimeAndCleanup { runtime, .. } => Some(runtime),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::path::Path;
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;
    use crate::att_mz::ProjectName;
    use crate::att_mz::lua::runtime::TrustedLuaRuntimeExecutionReport;
    use crate::att_mz::translate::executor::{LlmFinishReason, LlmRequestError};
    use crate::att_mz::translate::profile::{
        MzTranslationExecutionConfiguration, MzTranslationPlanningConfiguration,
        TranslationProfileLanguagePair,
    };
    use crate::att_mz::translate::standard::ChatMessageRole;
    use crate::project_database::StoredProjectRecord;
    use crate::storage::file_system::{ReadFile, ReadFileError};
    use crate::storage::sqlite::SqliteValue;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeError {}

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum Event {
        Read(PathBuf),
        Open(PathBuf),
        Runtime {
            script_path: PathBuf,
            phase: LuaPhase,
            project_name: String,
            output_root: Option<PathBuf>,
        },
        Llm(Vec<ChatMessage>),
        Begin,
        Rollback,
        Inspect,
        Close,
    }

    #[derive(Clone)]
    struct FakeFileReader {
        events: Arc<Mutex<Vec<Event>>>,
    }

    impl FileReader for FakeFileReader {
        type Error = FakeError;

        async fn read_file(&self, path: PathBuf) -> Result<ReadFile, ReadFileError<Self::Error>> {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Read(path));
            Ok(ReadFile::new(
                PathBuf::from("C:/resolved/scripts/translate.lua"),
                b"return true".to_vec(),
            ))
        }
    }

    struct SessionState {
        transaction: SqliteInteractiveTransactionState,
        closed: bool,
        fail_close: bool,
    }

    #[derive(Clone)]
    struct FakeSession {
        state: Arc<Mutex<SessionState>>,
        events: Arc<Mutex<Vec<Event>>>,
    }

    impl SqliteInteractiveSession for FakeSession {
        type Error = FakeError;

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
            Ok(1)
        }

        async fn begin(&self) -> Result<(), SqliteInteractiveSessionError<Self::Error>> {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Begin);
            let mut state = self.state.lock().expect("会话锁不应中毒");
            if state.transaction == SqliteInteractiveTransactionState::Active {
                return Err(SqliteInteractiveSessionError::TransactionAlreadyActive);
            }
            state.transaction = SqliteInteractiveTransactionState::Active;
            Ok(())
        }

        async fn commit(&self) -> Result<(), SqliteInteractiveSessionError<Self::Error>> {
            let mut state = self.state.lock().expect("会话锁不应中毒");
            if state.transaction == SqliteInteractiveTransactionState::Idle {
                return Err(SqliteInteractiveSessionError::NoActiveTransaction);
            }
            state.transaction = SqliteInteractiveTransactionState::Idle;
            Ok(())
        }

        async fn rollback(&self) -> Result<(), SqliteInteractiveSessionError<Self::Error>> {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Rollback);
            let mut state = self.state.lock().expect("会话锁不应中毒");
            if state.transaction == SqliteInteractiveTransactionState::Idle {
                return Err(SqliteInteractiveSessionError::NoActiveTransaction);
            }
            state.transaction = SqliteInteractiveTransactionState::Idle;
            Ok(())
        }

        async fn transaction_state(
            &self,
        ) -> Result<SqliteInteractiveTransactionState, SqliteInteractiveSessionError<Self::Error>>
        {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Inspect);
            Ok(self.state.lock().expect("会话锁不应中毒").transaction)
        }

        async fn close(&self) -> Result<(), SqliteInteractiveSessionError<Self::Error>> {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Close);
            let mut state = self.state.lock().expect("会话锁不应中毒");
            state.transaction = SqliteInteractiveTransactionState::Idle;
            state.closed = true;
            if state.fail_close {
                Err(SqliteInteractiveSessionError::OperationFailed(FakeError(
                    "close",
                )))
            } else {
                Ok(())
            }
        }
    }

    #[derive(Clone)]
    struct FakeSessionFactory {
        session: FakeSession,
        events: Arc<Mutex<Vec<Event>>>,
    }

    impl SqliteInteractiveSessionFactory for FakeSessionFactory {
        type Session = FakeSession;
        type Error = FakeError;

        async fn open_existing(
            &self,
            path: PathBuf,
        ) -> Result<Self::Session, OpenSqliteInteractiveSessionError<Self::Error>> {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Open(path));
            Ok(self.session.clone())
        }
    }

    #[derive(Clone)]
    struct FakeLlm {
        events: Arc<Mutex<Vec<Event>>>,
    }

    impl LlmRequestExecutor for FakeLlm {
        type Profile = String;
        type Error = FakeError;

        async fn request<'a>(
            &'a self,
            profile: &'a Self::Profile,
            messages: &'a [ChatMessage],
        ) -> Result<LlmResponse, LlmRequestError<Self::Error>> {
            assert_eq!(profile, "llm-config");
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Llm(messages.to_vec()));
            Ok(LlmResponse::new(
                "raw response",
                LlmFinishReason::Stop,
                Some("request-1".to_owned()),
                None,
            ))
        }
    }

    #[derive(Clone, Copy)]
    enum RuntimeBehavior {
        Complete,
        RequestLlm,
        LeaveTransactionOpen,
        FailAfterBegin,
        Cancelled,
        RequestLlmWithoutProfile,
    }

    #[derive(Clone)]
    struct FakeRuntime {
        behavior: RuntimeBehavior,
        events: Arc<Mutex<Vec<Event>>>,
    }

    impl TrustedLuaRuntimeExecutor for FakeRuntime {
        type Error = FakeError;

        async fn execute<B>(
            &self,
            program: OwnedLuaProgram,
            bindings: Arc<B>,
        ) -> TrustedLuaRuntimeExecutionReport<Self::Error, B::Error>
        where
            B: TrustedLuaHostBindings,
        {
            self.events
                .lock()
                .expect("事件锁不应中毒")
                .push(Event::Runtime {
                    script_path: program.main_script_path().to_path_buf(),
                    phase: bindings.phase(),
                    project_name: bindings.project().name().as_str().to_owned(),
                    output_root: bindings.project().output_root().map(Path::to_path_buf),
                });
            assert_eq!(program.source(), b"return true");

            let runtime = match self.behavior {
                RuntimeBehavior::Complete => Ok(()),
                RuntimeBehavior::RequestLlm => bindings
                    .request_llm(vec![ChatMessage::new(
                        ChatMessageRole::User,
                        "# Lua messages",
                    )])
                    .await
                    .map(|_| ())
                    .map_err(TrustedLuaRuntimeExecutionError::Binding),
                RuntimeBehavior::LeaveTransactionOpen => bindings
                    .begin()
                    .await
                    .map_err(TrustedLuaRuntimeExecutionError::Binding),
                RuntimeBehavior::FailAfterBegin => match bindings.begin().await {
                    Ok(()) => Err(TrustedLuaRuntimeExecutionError::Execute(FakeError(
                        "runtime",
                    ))),
                    Err(error) => Err(TrustedLuaRuntimeExecutionError::Binding(error)),
                },
                RuntimeBehavior::Cancelled => Err(TrustedLuaRuntimeExecutionError::Cancelled),
                RuntimeBehavior::RequestLlmWithoutProfile => bindings
                    .request_llm(Vec::new())
                    .await
                    .map(|_| ())
                    .map_err(TrustedLuaRuntimeExecutionError::Binding),
            };
            let termination = match &runtime {
                Ok(()) => TrustedLuaRuntimeTermination::Completed,
                Err(TrustedLuaRuntimeExecutionError::Cancelled) => {
                    TrustedLuaRuntimeTermination::Cancelled
                }
                Err(_) => TrustedLuaRuntimeTermination::Failed,
            };
            let finalization = bindings.finalize(termination).await;
            TrustedLuaRuntimeExecutionReport::new(runtime, finalization)
        }
    }

    type Service =
        TrustedLuaExecutionHostingService<FakeFileReader, FakeLlm, FakeRuntime, FakeSessionFactory>;

    struct Harness {
        service: Service,
        events: Arc<Mutex<Vec<Event>>>,
        state: Arc<Mutex<SessionState>>,
    }

    fn harness(behavior: RuntimeBehavior, fail_close: bool) -> Harness {
        let events = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(Mutex::new(SessionState {
            transaction: SqliteInteractiveTransactionState::Idle,
            closed: false,
            fail_close,
        }));
        let session = FakeSession {
            state: Arc::clone(&state),
            events: Arc::clone(&events),
        };
        let service = TrustedLuaExecutionHostingService::new(
            FakeFileReader {
                events: Arc::clone(&events),
            },
            FakeLlm {
                events: Arc::clone(&events),
            },
            FakeRuntime {
                behavior,
                events: Arc::clone(&events),
            },
            FakeSessionFactory {
                session,
                events: Arc::clone(&events),
            },
        );
        Harness {
            service,
            events,
            state,
        }
    }

    fn project() -> LuaProjectContext {
        LuaProjectContext::from_stored_record(&StoredProjectRecord::new(
            "alice".parse::<ProjectName>().expect("项目名应合法"),
            PathBuf::from("C:/projects/alice"),
            PathBuf::from("C:/projects/alice/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::att_mz::project::test_layout_profile(),
        ))
    }

    fn write_back_project() -> LuaProjectContext {
        let project = crate::att_mz::project::OpenedProject::new(
            "alice".parse::<ProjectName>().expect("项目名应合法"),
            PathBuf::from("C:/projects/alice"),
            PathBuf::from("C:/projects/alice/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::att_mz::project::test_layout_profile(),
        );
        LuaProjectContext::for_published_write_back(
            &project,
            PathBuf::from("C:/projects/alice/write_back"),
        )
    }

    fn profile() -> Arc<TranslationExecutionProfile<MzTranslationExecutionPayload<String>>> {
        let pair = TranslationProfileLanguagePair::new("ja", "zh-Hans").expect("语言对应合法");
        Arc::new(TranslationExecutionProfile::new(
            "quality",
            NonZeroUsize::new(2).expect("非零"),
            MzTranslationExecutionPayload::new(
                MzTranslationPlanningConfiguration::new(
                    NonZeroUsize::new(2).expect("非零"),
                    NonZeroUsize::new(10_000).expect("非零"),
                    [(pair, "# system".to_owned())],
                )
                .expect("规划配置应合法"),
                MzTranslationExecutionConfiguration::new(
                    vec![Duration::from_millis(10)],
                    Duration::from_secs(1),
                ),
                "llm-config".to_owned(),
            ),
        ))
    }

    #[tokio::test]
    async fn translate_injects_raw_llm_and_closes_the_same_idle_session() {
        let harness = harness(RuntimeBehavior::RequestLlm, false);

        harness
            .service
            .execute(LuaInvocation::translate(
                PathBuf::from("translate.lua"),
                project(),
                profile(),
            ))
            .await
            .expect("Lua 翻译应成功");

        let events = harness.events.lock().expect("事件锁不应中毒").clone();
        assert_eq!(events[0], Event::Read(PathBuf::from("translate.lua")));
        assert_eq!(
            events[1],
            Event::Open(PathBuf::from("C:/projects/alice/project.db"))
        );
        assert!(matches!(
            &events[2],
            Event::Runtime {
                script_path,
                phase: LuaPhase::Translate,
                project_name,
                output_root: None,
            } if script_path == Path::new("C:/resolved/scripts/translate.lua") && project_name == "alice"
        ));
        assert!(matches!(&events[3], Event::Llm(messages) if messages.len() == 1));
        assert_eq!(&events[4..], &[Event::Inspect, Event::Close]);
        assert!(harness.state.lock().expect("会话锁不应中毒").closed);
    }

    #[tokio::test]
    async fn normal_return_with_open_transaction_rolls_back_and_fails_explicitly() {
        let harness = harness(RuntimeBehavior::LeaveTransactionOpen, false);

        let error = harness
            .service
            .execute(LuaInvocation::translate(
                PathBuf::from("translate.lua"),
                project(),
                profile(),
            ))
            .await
            .expect_err("未关闭事务必须使调用失败");

        assert!(matches!(
            error,
            TrustedLuaExecutionHostingError::UnclosedTransaction
        ));
        let events = harness.events.lock().expect("事件锁不应中毒").clone();
        assert!(events.ends_with(&[Event::Inspect, Event::Rollback, Event::Close]));
        let state = harness.state.lock().expect("会话锁不应中毒");
        assert_eq!(state.transaction, SqliteInteractiveTransactionState::Idle);
        assert!(state.closed);
    }

    #[tokio::test]
    async fn runtime_and_cleanup_failures_are_both_preserved() {
        let harness = harness(RuntimeBehavior::FailAfterBegin, true);

        let error = harness
            .service
            .execute(LuaInvocation::translate(
                PathBuf::from("translate.lua"),
                project(),
                profile(),
            ))
            .await
            .expect_err("运行与关闭失败必须向上返回");

        assert!(matches!(
            &error,
            TrustedLuaExecutionHostingError::RuntimeAndCleanup {
                runtime: TrustedLuaRuntimeExecutionError::Execute(FakeError("runtime")),
                cleanup: LuaHostBindingError::Cleanup(cleanup),
            } if cleanup.failures().len() == 1
        ));
        assert!(error.to_string().contains("runtime"));
        assert!(error.to_string().contains("close"));
    }

    #[tokio::test]
    async fn cancelled_runtime_still_finalizes_and_closes_the_opened_session() {
        let harness = harness(RuntimeBehavior::Cancelled, false);

        let error = harness
            .service
            .execute(LuaInvocation::translate(
                PathBuf::from("translate.lua"),
                project(),
                profile(),
            ))
            .await
            .expect_err("取消必须作为明确终态返回");

        assert!(matches!(
            error,
            TrustedLuaExecutionHostingError::Runtime(TrustedLuaRuntimeExecutionError::Cancelled)
        ));
        assert!(
            harness
                .events
                .lock()
                .expect("事件锁不应中毒")
                .ends_with(&[Event::Inspect, Event::Close])
        );
        assert!(harness.state.lock().expect("会话锁不应中毒").closed);
    }

    #[tokio::test]
    async fn extract_does_not_receive_an_llm_profile() {
        let harness = harness(RuntimeBehavior::RequestLlmWithoutProfile, false);

        let error = harness
            .service
            .execute(LuaInvocation::extract(
                PathBuf::from("extract.lua"),
                project(),
            ))
            .await
            .expect_err("Extract 的 ctx.llm 调用必须失败");

        assert!(matches!(
            error,
            TrustedLuaExecutionHostingError::Runtime(TrustedLuaRuntimeExecutionError::Binding(
                LuaHostBindingError::LlmUnavailable
            ))
        ));
        assert!(
            !harness
                .events
                .lock()
                .expect("事件锁不应中毒")
                .iter()
                .any(|event| matches!(event, Event::Llm(_)))
        );
    }

    #[tokio::test]
    async fn write_back_phase_exposes_output_but_not_llm_and_still_closes_database() {
        let harness = harness(RuntimeBehavior::RequestLlmWithoutProfile, false);

        let error = harness
            .service
            .execute(LuaInvocation::write_back(
                PathBuf::from("write.lua"),
                write_back_project(),
            ))
            .await
            .expect_err("WriteBack 的 ctx.llm 调用必须失败");

        assert!(matches!(
            error,
            TrustedLuaExecutionHostingError::Runtime(TrustedLuaRuntimeExecutionError::Binding(
                LuaHostBindingError::LlmUnavailable
            ))
        ));
        let events = harness.events.lock().expect("事件锁不应中毒").clone();
        assert!(matches!(
            &events[2],
            Event::Runtime {
                phase: LuaPhase::WriteBack,
                output_root: Some(output_root),
                ..
            } if output_root == Path::new("C:/projects/alice/write_back")
        ));
        assert!(!events.iter().any(|event| matches!(event, Event::Llm(_))));
        assert!(events.ends_with(&[Event::Inspect, Event::Close]));
        assert!(harness.state.lock().expect("会话锁不应中毒").closed);
    }

    #[test]
    fn execution_future_is_send() {
        fn assert_send(_: impl Send) {}

        let harness = harness(RuntimeBehavior::Complete, false);
        assert_send(harness.service.execute(LuaInvocation::translate(
            PathBuf::from("translate.lua"),
            project(),
            profile(),
        )));
    }

    #[test]
    fn interactive_query_types_remain_owned_rust_values() {
        let query = SqliteQuery::new(
            "SELECT value FROM lua_data WHERE id = ?1",
            vec![SqliteValue::Integer(1)],
        );
        assert_eq!(query.parameters(), &[SqliteValue::Integer(1)]);
    }
}
