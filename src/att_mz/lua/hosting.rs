//! 可信 Lua 程序的项目上下文、数据库、LLM 与资源终态编排。

use std::error::Error;
use std::fmt;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::pin::Pin;
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
    OwnedLuaProgram, TrustedLuaBindingFinalization, TrustedLuaBindingFinalizationError,
    TrustedLuaBindingFinalizer, TrustedLuaHostCallError, TrustedLuaHostCalls,
    TrustedLuaRuntimeBindings, TrustedLuaRuntimeExecutionError, TrustedLuaRuntimeExecutor,
    TrustedLuaRuntimeReservation, TrustedLuaRuntimeTermination,
};
use super::session::{
    OpenSqliteInteractiveSessionError, SqliteInteractiveConnectionCloseOutcome,
    SqliteInteractiveRollbackOutcome, SqliteInteractiveSessionError,
    SqliteInteractiveSessionFactory, SqliteInteractiveSessionFinalizationReport,
    SqliteInteractiveSessionFinalizer, SqliteInteractiveSessionOperations,
    SqliteInteractiveTransactionObservation,
};
use super::{LuaInvocation, LuaPhase, LuaProjectContext, TrustedLuaExecutionHost};

/// 使用四个根能力完成可信 Lua 程序生命周期。
pub(crate) struct TrustedLuaExecutionHostingService<F, L, R, S> {
    file_reader: F,
    llm: LuaLlmCapability<L>,
    runtime: R,
    session_factory: S,
}

impl<F, L, R, S> TrustedLuaExecutionHostingService<F, L, R, S> {
    pub(crate) fn with_llm(file_reader: F, llm: L, runtime: R, session_factory: S) -> Self {
        Self {
            file_reader,
            llm: LuaLlmCapability::Enabled(Arc::new(llm)),
            runtime,
            session_factory,
        }
    }

    pub(crate) fn without_llm(file_reader: F, runtime: R, session_factory: S) -> Self {
        Self {
            file_reader,
            llm: LuaLlmCapability::Disabled(PhantomData),
            runtime,
            session_factory,
        }
    }
}

enum LuaLlmCapability<L> {
    Disabled(PhantomData<fn() -> L>),
    Enabled(Arc<L>),
}

impl<L> Clone for LuaLlmCapability<L> {
    fn clone(&self) -> Self {
        match self {
            Self::Disabled(_) => Self::Disabled(PhantomData),
            Self::Enabled(llm) => Self::Enabled(Arc::clone(llm)),
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
    type Error = TrustedLuaExecutionHostingError<F::Error, S::Error, R::Error>;

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

        // 先预留 Runtime 容量，避免在长时间排队期间占用 SQLite 连接。
        let reservation = self
            .runtime
            .reserve()
            .await
            .map_err(TrustedLuaExecutionHostingError::ReserveRuntime)?;

        let database_path = project.database_path().to_path_buf();
        let opened = self
            .session_factory
            .open_existing(database_path.clone())
            .await
            .map_err(|source| TrustedLuaExecutionHostingError::OpenDatabase {
                database_path,
                source,
            })?;
        let (operations, finalizer) = opened.into_parts();

        let calls: Arc<dyn TrustedLuaHostCalls> = Arc::new(LuaHostCalls {
            phase,
            project,
            profile,
            operations,
            llm: self.llm.clone(),
        });
        let finalizer: Box<dyn TrustedLuaBindingFinalizer> =
            Box::new(LuaSessionFinalizer { finalizer });
        let handle = reservation.start(program, TrustedLuaRuntimeBindings::new(calls, finalizer));
        let (runtime, finalization) = handle.await.into_parts();

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

struct LuaHostCalls<S, L>
where
    S: SqliteInteractiveSessionOperations,
    L: LlmRequestExecutor + 'static,
{
    phase: LuaPhase,
    project: LuaProjectContext,
    profile: Option<Arc<TranslationExecutionProfile<MzTranslationExecutionPayload<L::Profile>>>>,
    operations: Arc<S>,
    llm: LuaLlmCapability<L>,
}

impl<S, L> TrustedLuaHostCalls for LuaHostCalls<S, L>
where
    S: SqliteInteractiveSessionOperations,
    L: LlmRequestExecutor + 'static,
{
    fn phase(&self) -> LuaPhase {
        self.phase
    }

    fn project(&self) -> &LuaProjectContext {
        &self.project
    }

    fn query(
        &self,
        query: SqliteQuery,
    ) -> Pin<
        Box<dyn Future<Output = Result<Vec<SqliteRow>, TrustedLuaHostCallError>> + Send + 'static>,
    > {
        let operations = Arc::clone(&self.operations);
        Box::pin(async move { operations.query(query).await.map_err(database_call_error) })
    }

    fn execute(
        &self,
        command: SqliteCommand,
    ) -> Pin<Box<dyn Future<Output = Result<u64, TrustedLuaHostCallError>> + Send + 'static>> {
        let operations = Arc::clone(&self.operations);
        Box::pin(async move {
            operations
                .execute(command)
                .await
                .map_err(database_call_error)
        })
    }

    fn begin(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>> {
        let operations = Arc::clone(&self.operations);
        Box::pin(async move { operations.begin().await.map_err(database_call_error) })
    }

    fn commit(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>> {
        let operations = Arc::clone(&self.operations);
        Box::pin(async move { operations.commit().await.map_err(database_call_error) })
    }

    fn rollback(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>> {
        let operations = Arc::clone(&self.operations);
        Box::pin(async move { operations.rollback().await.map_err(database_call_error) })
    }

    fn request_llm(
        &self,
        messages: Vec<ChatMessage>,
    ) -> Pin<Box<dyn Future<Output = Result<LlmResponse, TrustedLuaHostCallError>> + Send + 'static>>
    {
        let Some(profile) = self.profile.as_ref().map(Arc::clone) else {
            return Box::pin(async {
                Err(TrustedLuaHostCallError::new(
                    "llm",
                    "unavailable",
                    "当前 Lua 阶段没有 ctx.llm",
                    None,
                    None,
                ))
            });
        };
        let LuaLlmCapability::Enabled(llm) = &self.llm else {
            return Box::pin(async {
                Err(TrustedLuaHostCallError::new(
                    "llm",
                    "unavailable",
                    "当前 Lua Host 未构造 LLM 能力",
                    None,
                    None,
                ))
            });
        };
        let llm = Arc::clone(llm);
        Box::pin(async move {
            llm.request(profile.llm_profile(), &messages)
                .await
                .map_err(llm_call_error)
        })
    }
}

fn database_call_error<E>(error: SqliteInteractiveSessionError<E>) -> TrustedLuaHostCallError
where
    E: Error + Send + Sync + 'static,
{
    let kind = match &error {
        SqliteInteractiveSessionError::Closed => "closed",
        SqliteInteractiveSessionError::Indeterminate => "indeterminate",
        SqliteInteractiveSessionError::TransactionAlreadyActive => "transaction_already_active",
        SqliteInteractiveSessionError::NoActiveTransaction => "no_active_transaction",
        SqliteInteractiveSessionError::OperationFailed(_) => "operation_failed",
        SqliteInteractiveSessionError::OutcomeUnknown(_) => "outcome_unknown",
    };
    let message = error.to_string();
    TrustedLuaHostCallError::new("sqlite", kind, message, None, Some(Arc::new(error)))
}

fn llm_call_error<E>(error: LlmRequestError<E>) -> TrustedLuaHostCallError
where
    E: Error + Send + Sync + 'static,
{
    let (kind, retry_after_ms) = match &error {
        LlmRequestError::Retryable { retry_after, .. } => (
            "retryable",
            retry_after.map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)),
        ),
        LlmRequestError::Fatal(_) => ("fatal", None),
    };
    let message = error.to_string();
    TrustedLuaHostCallError::new("llm", kind, message, retry_after_ms, Some(Arc::new(error)))
}

struct LuaSessionFinalizer<F> {
    finalizer: F,
}

impl<F> TrustedLuaBindingFinalizer for LuaSessionFinalizer<F>
where
    F: SqliteInteractiveSessionFinalizer,
{
    fn finalize(
        self: Box<Self>,
        _termination: TrustedLuaRuntimeTermination,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        TrustedLuaBindingFinalization,
                        TrustedLuaBindingFinalizationError,
                    >,
                > + Send
                + 'static,
        >,
    > {
        Box::pin(async move {
            let report = self.finalizer.finalize().await;
            let had_active_transaction = matches!(
                report.transaction(),
                SqliteInteractiveTransactionObservation::Active
            );
            if sqlite_finalization_succeeded(&report) {
                Ok(TrustedLuaBindingFinalization::new(had_active_transaction))
            } else {
                let failure = SqliteFinalizationFailure { report };
                Err(TrustedLuaBindingFinalizationError::new(
                    failure.to_string(),
                    Some(Arc::new(failure)),
                ))
            }
        })
    }
}

fn sqlite_finalization_succeeded<E>(
    report: &SqliteInteractiveSessionFinalizationReport<E>,
) -> bool {
    matches!(
        report.transaction(),
        SqliteInteractiveTransactionObservation::Idle
            | SqliteInteractiveTransactionObservation::Active
    ) && matches!(
        report.rollback(),
        SqliteInteractiveRollbackOutcome::NotRequired
            | SqliteInteractiveRollbackOutcome::RolledBack
    ) && matches!(
        report.connection(),
        SqliteInteractiveConnectionCloseOutcome::Closed
    )
}

#[derive(Debug)]
struct SqliteFinalizationFailure<E> {
    report: SqliteInteractiveSessionFinalizationReport<E>,
}

impl<E> fmt::Display for SqliteFinalizationFailure<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Lua SQLite 会话终结失败：")?;
        match self.report.transaction() {
            SqliteInteractiveTransactionObservation::Idle => formatter.write_str("事务空闲")?,
            SqliteInteractiveTransactionObservation::Active => {
                formatter.write_str("发现活动事务")?
            }
            SqliteInteractiveTransactionObservation::Indeterminate => {
                formatter.write_str("事务状态不可确定")?
            }
            SqliteInteractiveTransactionObservation::Unavailable(error) => {
                write!(formatter, "无法观测事务（{error}）")?
            }
        }
        match self.report.rollback() {
            SqliteInteractiveRollbackOutcome::NotRequired => {}
            SqliteInteractiveRollbackOutcome::RolledBack => formatter.write_str("；已回滚")?,
            SqliteInteractiveRollbackOutcome::Failed(error) => {
                write!(formatter, "；回滚失败（{error}）")?
            }
            SqliteInteractiveRollbackOutcome::OutcomeUnknown(error) => {
                write!(formatter, "；回滚结果未知（{error}）")?
            }
            SqliteInteractiveRollbackOutcome::NotAttempted => {
                formatter.write_str("；未尝试回滚")?
            }
        }
        match self.report.connection() {
            SqliteInteractiveConnectionCloseOutcome::Closed => formatter.write_str("；连接已关闭"),
            SqliteInteractiveConnectionCloseOutcome::Failed(error) => {
                write!(formatter, "；关闭失败（{error}）")
            }
            SqliteInteractiveConnectionCloseOutcome::OutcomeUnknown(error) => {
                write!(formatter, "；关闭结果未知（{error}）")
            }
        }
    }
}

impl<E> Error for SqliteFinalizationFailure<E> where E: Error + Send + Sync + 'static {}

/// Host 在脚本加载、Runtime 预留、数据库建立、VM 或收尾阶段遇到的失败。
#[derive(Debug)]
pub(crate) enum TrustedLuaExecutionHostingError<F, O, R> {
    ReadScript {
        script_path: PathBuf,
        source: ReadFileError<F>,
    },
    ReserveRuntime(R),
    OpenDatabase {
        database_path: PathBuf,
        source: OpenSqliteInteractiveSessionError<O>,
    },
    Runtime(TrustedLuaRuntimeExecutionError<R>),
    Cleanup(TrustedLuaBindingFinalizationError),
    UnclosedTransaction,
    RuntimeAndCleanup {
        runtime: TrustedLuaRuntimeExecutionError<R>,
        cleanup: TrustedLuaBindingFinalizationError,
    },
}

impl<F, O, R> fmt::Display for TrustedLuaExecutionHostingError<F, O, R>
where
    F: fmt::Display,
    O: fmt::Display,
    R: fmt::Display,
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
            Self::ReserveRuntime(source) => write!(formatter, "无法预留 Lua 执行容量：{source}"),
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

impl<F, O, R> Error for TrustedLuaExecutionHostingError<F, O, R>
where
    F: Error + 'static,
    O: Error + 'static,
    R: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadScript { source, .. } => Some(source),
            Self::ReserveRuntime(source) => Some(source),
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
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::att_mz::ProjectName;
    use crate::att_mz::lua::runtime::{
        TrustedLuaExecutionHandle, TrustedLuaRuntimeExecutionReport, TrustedLuaRuntimeReservation,
    };
    use crate::att_mz::lua::session::OpenedSqliteInteractiveSession;
    use crate::att_mz::project::OpenedProject;
    use crate::att_mz::translate::executor::{LlmFinishReason, LlmUsage};
    use crate::storage::file_system::ReadFile;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeError {}

    type Events = Arc<Mutex<Vec<&'static str>>>;

    fn record(events: &Events, event: &'static str) {
        events.lock().expect("事件锁不应中毒").push(event);
    }

    #[derive(Clone)]
    struct FakeFileReader {
        events: Events,
    }

    impl FileReader for FakeFileReader {
        type Error = FakeError;

        async fn read_file(&self, _path: PathBuf) -> Result<ReadFile, ReadFileError<Self::Error>> {
            record(&self.events, "read");
            Ok(ReadFile::new(
                PathBuf::from("C:/resolved/main.lua"),
                b"return true".to_vec(),
            ))
        }
    }

    #[derive(Clone)]
    struct FakeLlm;

    impl LlmRequestExecutor for FakeLlm {
        type Profile = ();
        type Error = FakeError;

        async fn request<'a>(
            &'a self,
            _profile: &'a Self::Profile,
            _messages: &'a [ChatMessage],
        ) -> Result<LlmResponse, LlmRequestError<Self::Error>> {
            Ok(LlmResponse::new(
                "response",
                LlmFinishReason::Stop,
                None,
                "response-id",
                Some(LlmUsage::new(1, 1, 2)),
            ))
        }
    }

    #[derive(Clone)]
    struct FakeOperations;

    impl SqliteInteractiveSessionOperations for FakeOperations {
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
            Ok(0)
        }

        async fn begin(&self) -> Result<(), SqliteInteractiveSessionError<Self::Error>> {
            Ok(())
        }

        async fn commit(&self) -> Result<(), SqliteInteractiveSessionError<Self::Error>> {
            Ok(())
        }

        async fn rollback(&self) -> Result<(), SqliteInteractiveSessionError<Self::Error>> {
            Ok(())
        }
    }

    #[derive(Clone, Copy)]
    enum FinalizationBehavior {
        Idle,
        Active,
        CloseFailed,
    }

    struct FakeSessionFinalizer {
        events: Events,
        behavior: FinalizationBehavior,
    }

    impl SqliteInteractiveSessionFinalizer for FakeSessionFinalizer {
        type Error = FakeError;

        async fn finalize(self) -> SqliteInteractiveSessionFinalizationReport<Self::Error> {
            record(&self.events, "finalize");
            match self.behavior {
                FinalizationBehavior::Idle => SqliteInteractiveSessionFinalizationReport::new(
                    SqliteInteractiveTransactionObservation::Idle,
                    SqliteInteractiveRollbackOutcome::NotRequired,
                    SqliteInteractiveConnectionCloseOutcome::Closed,
                ),
                FinalizationBehavior::Active => SqliteInteractiveSessionFinalizationReport::new(
                    SqliteInteractiveTransactionObservation::Active,
                    SqliteInteractiveRollbackOutcome::RolledBack,
                    SqliteInteractiveConnectionCloseOutcome::Closed,
                ),
                FinalizationBehavior::CloseFailed => {
                    SqliteInteractiveSessionFinalizationReport::new(
                        SqliteInteractiveTransactionObservation::Idle,
                        SqliteInteractiveRollbackOutcome::NotRequired,
                        SqliteInteractiveConnectionCloseOutcome::Failed(FakeError("close")),
                    )
                }
            }
        }
    }

    #[derive(Clone)]
    struct FakeSessionFactory {
        events: Events,
        fail: bool,
        finalization: FinalizationBehavior,
    }

    impl SqliteInteractiveSessionFactory for FakeSessionFactory {
        type Operations = FakeOperations;
        type Finalizer = FakeSessionFinalizer;
        type Error = FakeError;

        async fn open_existing(
            &self,
            _path: PathBuf,
        ) -> Result<
            OpenedSqliteInteractiveSession<Self::Operations, Self::Finalizer>,
            OpenSqliteInteractiveSessionError<Self::Error>,
        > {
            record(&self.events, "open");
            if self.fail {
                Err(OpenSqliteInteractiveSessionError::OpenFailed(FakeError(
                    "open",
                )))
            } else {
                Ok(OpenedSqliteInteractiveSession::new(
                    Arc::new(FakeOperations),
                    FakeSessionFinalizer {
                        events: Arc::clone(&self.events),
                        behavior: self.finalization,
                    },
                ))
            }
        }
    }

    #[derive(Clone, Copy)]
    enum RuntimeBehavior {
        Complete,
        Fail,
    }

    #[derive(Clone)]
    struct FakeRuntime {
        events: Events,
        fail_reserve: bool,
        behavior: RuntimeBehavior,
    }

    impl TrustedLuaRuntimeExecutor for FakeRuntime {
        type Error = FakeError;
        type Reservation = FakeReservation;

        async fn reserve(&self) -> Result<Self::Reservation, Self::Error> {
            record(&self.events, "reserve");
            if self.fail_reserve {
                Err(FakeError("reserve"))
            } else {
                Ok(FakeReservation {
                    events: Arc::clone(&self.events),
                    behavior: self.behavior,
                    started: false,
                })
            }
        }
    }

    struct FakeReservation {
        events: Events,
        behavior: RuntimeBehavior,
        started: bool,
    }

    impl Drop for FakeReservation {
        fn drop(&mut self) {
            if !self.started {
                record(&self.events, "reservation_drop");
            }
        }
    }

    impl TrustedLuaRuntimeReservation for FakeReservation {
        type Error = FakeError;

        fn start(
            mut self,
            _program: OwnedLuaProgram,
            bindings: TrustedLuaRuntimeBindings,
        ) -> TrustedLuaExecutionHandle<Self::Error> {
            self.started = true;
            record(&self.events, "start");
            let behavior = self.behavior;
            let (calls, finalizer) = bindings.into_parts();
            assert_eq!(calls.phase(), LuaPhase::Extract);
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let cancelled = Arc::new(AtomicBool::new(false));
            tokio::spawn(async move {
                let runtime = match behavior {
                    RuntimeBehavior::Complete => Ok(()),
                    RuntimeBehavior::Fail => {
                        Err(TrustedLuaRuntimeExecutionError::Execute(FakeError("vm")))
                    }
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

    type Service =
        TrustedLuaExecutionHostingService<FakeFileReader, FakeLlm, FakeRuntime, FakeSessionFactory>;

    fn service(
        events: Events,
        fail_reserve: bool,
        fail_open: bool,
        runtime: RuntimeBehavior,
        finalization: FinalizationBehavior,
    ) -> Service {
        TrustedLuaExecutionHostingService::<_, FakeLlm, _, _>::without_llm(
            FakeFileReader {
                events: Arc::clone(&events),
            },
            FakeRuntime {
                events: Arc::clone(&events),
                fail_reserve,
                behavior: runtime,
            },
            FakeSessionFactory {
                events,
                fail: fail_open,
                finalization,
            },
        )
    }

    fn invocation() -> LuaInvocation<TranslationExecutionProfile<MzTranslationExecutionPayload<()>>>
    {
        let project = OpenedProject::new(
            "demo".parse::<ProjectName>().unwrap(),
            PathBuf::from("C:/projects/demo"),
            PathBuf::from("C:/projects/demo/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::att_mz::project::test_layout_profile(),
        );
        LuaInvocation::extract(
            PathBuf::from("main.lua"),
            LuaProjectContext::from_opened_project(&project),
        )
    }

    #[tokio::test]
    async fn reserves_before_opening_and_synchronously_hands_off_the_session() {
        let events = Arc::new(Mutex::new(Vec::new()));
        service(
            Arc::clone(&events),
            false,
            false,
            RuntimeBehavior::Complete,
            FinalizationBehavior::Idle,
        )
        .execute(invocation())
        .await
        .unwrap();

        assert_eq!(
            *events.lock().unwrap(),
            ["read", "reserve", "open", "start", "finalize"]
        );
    }

    #[tokio::test]
    async fn reserve_failure_never_opens_a_database() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let error = service(
            Arc::clone(&events),
            true,
            false,
            RuntimeBehavior::Complete,
            FinalizationBehavior::Idle,
        )
        .execute(invocation())
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            TrustedLuaExecutionHostingError::ReserveRuntime(FakeError("reserve"))
        ));
        assert_eq!(*events.lock().unwrap(), ["read", "reserve"]);
    }

    #[tokio::test]
    async fn open_failure_releases_the_reservation_without_starting_runtime() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let error = service(
            Arc::clone(&events),
            false,
            true,
            RuntimeBehavior::Complete,
            FinalizationBehavior::Idle,
        )
        .execute(invocation())
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            TrustedLuaExecutionHostingError::OpenDatabase { .. }
        ));
        assert_eq!(
            *events.lock().unwrap(),
            ["read", "reserve", "open", "reservation_drop"]
        );
    }

    #[tokio::test]
    async fn active_transaction_is_rolled_back_and_reported_after_normal_return() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let error = service(
            events,
            false,
            false,
            RuntimeBehavior::Complete,
            FinalizationBehavior::Active,
        )
        .execute(invocation())
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            TrustedLuaExecutionHostingError::UnclosedTransaction
        ));
    }

    #[tokio::test]
    async fn runtime_and_session_cleanup_failures_are_both_preserved() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let error = service(
            events,
            false,
            false,
            RuntimeBehavior::Fail,
            FinalizationBehavior::CloseFailed,
        )
        .execute(invocation())
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            TrustedLuaExecutionHostingError::RuntimeAndCleanup {
                runtime: TrustedLuaRuntimeExecutionError::Execute(FakeError("vm")),
                ..
            }
        ));
        assert!(error.to_string().contains("vm"));
        assert!(error.to_string().contains("close"));
    }

    #[test]
    fn execution_future_is_send() {
        fn assert_send(_: impl Send) {}

        assert_send(
            service(
                Arc::new(Mutex::new(Vec::new())),
                false,
                false,
                RuntimeBehavior::Complete,
                FinalizationBehavior::Idle,
            )
            .execute(invocation()),
        );
    }
}
