//! 可信 Lua 程序的冻结来源、项目上下文、数据库、LLM 与资源终态编排。

use std::error::Error;
use std::fmt;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::execution::OperationCompletion;
use crate::llm::{ChatMessage, LlmRequestError, LlmRequestExecutor, LlmResponse};
use crate::storage::file_system::{DirectoryLister, FileReader, ListDirectoryError, ReadFileError};
use crate::storage::sqlite::{SqliteCommand, SqliteQuery, SqliteRow};
use crate::storage::sqlite_session::{
    OpenSqliteInteractiveSessionError, SqliteInteractiveSessionError,
    SqliteInteractiveSessionFactory, SqliteInteractiveSessionFinalizer,
    SqliteInteractiveSessionOperations,
};

use super::runtime::{
    OwnedLuaProgram, TrustedLuaBindingFinalization, TrustedLuaBindingFinalizationError,
    TrustedLuaBindingFinalizer, TrustedLuaCommonBindings, TrustedLuaCommonHostCalls,
    TrustedLuaExtractHostCalls, TrustedLuaExtractIntent, TrustedLuaHostCallError,
    TrustedLuaRuntimeBindings, TrustedLuaRuntimeExecutionError, TrustedLuaRuntimeExecutor,
    TrustedLuaTranslateHostCalls, TrustedLuaTranslationSemantics, TrustedLuaWriteBackHostCalls,
};
use super::{
    LuaInvocation, LuaProjectContext, LuaSourcePath, TrustedLuaExecutionHost,
    TrustedLuaExecutionOutcome,
};

/// 使用四个根能力完成可信 Lua 程序生命周期。
pub(crate) struct TrustedLuaExecutionHostingService<F, L, R, S> {
    file_system: Arc<F>,
    llm: LuaLlmCapability<L>,
    runtime: R,
    session_factory: S,
}

impl<F, L, R, S> TrustedLuaExecutionHostingService<F, L, R, S> {
    pub(crate) fn with_llm(file_reader: F, llm: L, runtime: R, session_factory: S) -> Self {
        Self {
            file_system: Arc::new(file_reader),
            llm: LuaLlmCapability::Enabled(Arc::new(llm)),
            runtime,
            session_factory,
        }
    }

    pub(crate) fn without_llm(file_reader: F, runtime: R, session_factory: S) -> Self {
        Self {
            file_system: Arc::new(file_reader),
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
    F: FileReader + DirectoryLister<Error = <F as FileReader>::Error> + 'static,
    L: LlmRequestExecutor + 'static,
    R: TrustedLuaRuntimeExecutor,
    S: SqliteInteractiveSessionFactory,
{
    type TranslationClient = L::Client;
    type Error = TrustedLuaExecutionHostingError<<F as FileReader>::Error, S::Error, R::Error>;

    async fn execute(
        &self,
        invocation: LuaInvocation<Self::TranslationClient>,
    ) -> Result<OperationCompletion<TrustedLuaExecutionOutcome>, Self::Error> {
        let (phase, script_path, project) = match invocation {
            LuaInvocation::Extract {
                script_path,
                project,
            } => (HostingPhase::Extract, script_path, project),
            LuaInvocation::Translate {
                script_path,
                project,
                llm_client,
                semantics,
            } => (
                HostingPhase::Translate {
                    llm_client,
                    semantics,
                },
                script_path,
                project,
            ),
            LuaInvocation::WriteBack {
                script_path,
                project,
                calls,
            } => (HostingPhase::WriteBack(calls), script_path, project),
        };

        let requested_script_path = script_path.clone();
        let read_file = self
            .file_system
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
        let opened = self
            .session_factory
            .open_existing(database_path.clone())
            .await
            .map_err(|source| TrustedLuaExecutionHostingError::OpenDatabase {
                database_path,
                source,
            })?;
        let (operations, finalizer) = opened.into_parts();

        let common: Arc<dyn TrustedLuaCommonHostCalls> = Arc::new(LuaCommonHostCalls {
            project,
            operations,
            file_system: Arc::clone(&self.file_system),
        });
        let finalizer: Box<dyn TrustedLuaBindingFinalizer> =
            Box::new(LuaSessionFinalizer { finalizer });
        let common = TrustedLuaCommonBindings::new(common);
        let mut extract_calls = None;
        let bindings = match phase {
            HostingPhase::Extract => {
                let calls = Arc::new(LuaExtractHostCalls::default());
                extract_calls = Some(Arc::clone(&calls));
                TrustedLuaRuntimeBindings::extract(common, calls, finalizer)
            }
            HostingPhase::Translate {
                llm_client,
                semantics,
            } => TrustedLuaRuntimeBindings::translate(
                common,
                Arc::new(LuaTranslationHostCalls {
                    llm_client,
                    semantics,
                    llm: self.llm.clone(),
                }),
                finalizer,
            ),
            HostingPhase::WriteBack(calls) => {
                TrustedLuaRuntimeBindings::write_back(common, calls, finalizer)
            }
        };
        let handle = self.runtime.start(program, bindings);
        let (runtime, finalization) = handle.await.into_parts();

        match (runtime, finalization) {
            (Err(TrustedLuaRuntimeExecutionError::Cancelled), Ok(_)) => {
                Ok(OperationCompletion::Cancelled)
            }
            (Ok(()), Ok(finalization)) if finalization.had_unclosed_transaction() => {
                Err(TrustedLuaExecutionHostingError::UnclosedTransaction)
            }
            (Ok(()), Ok(_)) => Ok(OperationCompletion::Completed(
                extract_calls
                    .and_then(|calls| calls.take_intent())
                    .map_or(TrustedLuaExecutionOutcome::Empty, |intent| {
                        TrustedLuaExecutionOutcome::ExtractIntent(intent)
                    }),
            )),
            (Ok(()), Err(cleanup)) => Err(TrustedLuaExecutionHostingError::Cleanup(cleanup)),
            (Err(runtime), Ok(_)) => Err(TrustedLuaExecutionHostingError::Runtime(runtime)),
            (Err(runtime), Err(cleanup)) => {
                Err(TrustedLuaExecutionHostingError::RuntimeAndCleanup { runtime, cleanup })
            }
        }
    }
}

enum HostingPhase<P> {
    Extract,
    Translate {
        llm_client: Arc<P>,
        semantics: Arc<dyn TrustedLuaTranslationSemantics>,
    },
    WriteBack(Arc<dyn TrustedLuaWriteBackHostCalls>),
}

struct LuaCommonHostCalls<F, S>
where
    F: FileReader + DirectoryLister<Error = <F as FileReader>::Error>,
    S: SqliteInteractiveSessionOperations,
{
    project: LuaProjectContext,
    operations: Arc<S>,
    file_system: Arc<F>,
}

impl<F, S> TrustedLuaCommonHostCalls for LuaCommonHostCalls<F, S>
where
    F: FileReader + DirectoryLister<Error = <F as FileReader>::Error> + 'static,
    S: SqliteInteractiveSessionOperations,
{
    fn project(&self) -> &LuaProjectContext {
        &self.project
    }

    fn read_source(
        &self,
        path: LuaSourcePath,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, TrustedLuaHostCallError>> + Send + 'static>>
    {
        let file_system = Arc::clone(&self.file_system);
        let requested = path.join_to(self.project.source_root());
        Box::pin(async move {
            file_system
                .read_file(requested)
                .await
                .map(|file| file.into_bytes())
                .map_err(source_read_error)
        })
    }

    fn list_source(
        &self,
        path: LuaSourcePath,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, TrustedLuaHostCallError>> + Send + 'static>>
    {
        let file_system = Arc::clone(&self.file_system);
        let requested = path.join_to(self.project.source_root());
        Box::pin(async move {
            let entries = file_system
                .list_directory(requested)
                .await
                .map_err(source_list_error)?;
            let mut result = Vec::with_capacity(entries.len());
            for entry in entries {
                let name = entry
                    .resolved_path()
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| {
                        TrustedLuaHostCallError::new(
                            "filesystem",
                            "invalid_utf8",
                            "来源目录项名称无法无损转换为 UTF-8",
                            None,
                            None,
                        )
                    })?;
                let child = path.child(name).map_err(|error| {
                    TrustedLuaHostCallError::new(
                        "filesystem",
                        "invalid_path",
                        error.to_string(),
                        None,
                        Some(Arc::new(error)),
                    )
                })?;
                result.push(child.as_str().to_owned());
            }
            result.sort();
            Ok(result)
        })
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
}

#[derive(Default)]
struct LuaExtractHostCalls {
    intent: Mutex<Option<TrustedLuaExtractIntent>>,
}

impl LuaExtractHostCalls {
    fn record_intent(
        &self,
        intent: TrustedLuaExtractIntent,
    ) -> Result<(), TrustedLuaHostCallError> {
        let mut current = self.intent.lock().expect("Lua Extract intent 锁不应中毒");
        if current.is_some() {
            return Err(TrustedLuaHostCallError::new(
                "extract",
                "intent_already_declared",
                "一次 Lua Extract 主程序只能声明一个标准快照意图",
                None,
                None,
            ));
        }
        *current = Some(intent);
        Ok(())
    }

    fn take_intent(&self) -> Option<TrustedLuaExtractIntent> {
        self.intent
            .lock()
            .expect("Lua Extract intent 锁不应中毒")
            .take()
    }
}

impl TrustedLuaExtractHostCalls for LuaExtractHostCalls {
    fn replace_standard(
        &self,
        snapshot: crate::rpg_maker::extract::store::LuaSnapshot,
    ) -> Result<(), TrustedLuaHostCallError> {
        self.record_intent(TrustedLuaExtractIntent::Replace(snapshot))
    }

    fn clear_standard(&self) -> Result<(), TrustedLuaHostCallError> {
        self.record_intent(TrustedLuaExtractIntent::Deactivate)
    }
}

struct LuaTranslationHostCalls<L>
where
    L: LlmRequestExecutor + 'static,
{
    llm_client: Arc<L::Client>,
    semantics: Arc<dyn TrustedLuaTranslationSemantics>,
    llm: LuaLlmCapability<L>,
}

impl<L> TrustedLuaTranslateHostCalls for LuaTranslationHostCalls<L>
where
    L: LlmRequestExecutor + 'static,
{
    fn system_prompt(&self) -> &str {
        self.semantics.system_prompt()
    }

    fn source_language(&self) -> &str {
        self.semantics.source_language()
    }

    fn target_language(&self) -> &str {
        self.semantics.target_language()
    }

    fn prepare_translation(
        &self,
        kind: crate::rpg_maker::text::TextGroupKind,
        original: String,
    ) -> Result<Arc<dyn super::runtime::TrustedLuaPreparedTranslation>, TrustedLuaHostCallError>
    {
        self.semantics.prepare_translation(kind, original)
    }

    fn request_llm(
        &self,
        messages: Vec<ChatMessage>,
    ) -> Pin<Box<dyn Future<Output = Result<LlmResponse, TrustedLuaHostCallError>> + Send + 'static>>
    {
        let llm_client = Arc::clone(&self.llm_client);
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
            llm.request(llm_client.as_ref(), &messages)
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

fn source_read_error<E>(error: ReadFileError<E>) -> TrustedLuaHostCallError
where
    E: Error + Send + Sync + 'static,
{
    let kind = match &error {
        ReadFileError::NotFound { .. } => "not_found",
        ReadFileError::NotFile { .. } => "not_file",
        ReadFileError::Io { .. } => "io",
    };
    let message = error.to_string();
    TrustedLuaHostCallError::new("filesystem", kind, message, None, Some(Arc::new(error)))
}

fn source_list_error<E>(error: ListDirectoryError<E>) -> TrustedLuaHostCallError
where
    E: Error + Send + Sync + 'static,
{
    let kind = match &error {
        ListDirectoryError::NotFound { .. } => "not_found",
        ListDirectoryError::NotDirectory { .. } => "not_directory",
        ListDirectoryError::Io { .. } => "io",
    };
    let message = error.to_string();
    TrustedLuaHostCallError::new("filesystem", kind, message, None, Some(Arc::new(error)))
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
            match self.finalizer.finalize().await {
                Ok(finalization) => Ok(TrustedLuaBindingFinalization::new(
                    finalization.had_unclosed_transaction(),
                )),
                Err(error) => {
                    let message = error.to_string();
                    Err(TrustedLuaBindingFinalizationError::new(
                        message,
                        Some(Arc::new(error)),
                    ))
                }
            }
        })
    }
}

/// Host 在脚本加载、数据库建立、VM 或收尾阶段遇到的失败。
#[derive(Debug)]
pub(crate) enum TrustedLuaExecutionHostingError<F, O, R> {
    ReadScript {
        script_path: PathBuf,
        source: ReadFileError<F>,
    },
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
    use crate::llm::{LlmFinishReason, LlmUsage};
    use crate::rpg_maker::ProjectName;
    use crate::rpg_maker::lua::runtime::{
        TrustedLuaExecutionHandle, TrustedLuaPhaseBindings, TrustedLuaRuntimeExecutionReport,
    };
    use crate::rpg_maker::project::OpenedProject;
    use crate::storage::file_system::{
        DirectoryEntry, DirectoryEntryKind, DirectoryLister, ListDirectoryError, ReadFile,
    };
    use crate::storage::sqlite_session::{
        OpenedSqliteInteractiveSession, SqliteInteractiveSessionFinalization,
        SqliteInteractiveSessionFinalizationError, SqliteInteractiveSessionFinalizationFailure,
    };

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

    impl DirectoryLister for FakeFileReader {
        type Error = FakeError;

        async fn list_directory(
            &self,
            _path: PathBuf,
        ) -> Result<Vec<DirectoryEntry>, ListDirectoryError<Self::Error>> {
            Ok(Vec::new())
        }
    }

    #[derive(Clone)]
    struct FakeLlm;

    impl LlmRequestExecutor for FakeLlm {
        type Client = ();
        type Error = FakeError;

        async fn request<'a>(
            &'a self,
            _client: &'a Self::Client,
            _messages: &'a [ChatMessage],
        ) -> Result<LlmResponse, LlmRequestError<Self::Error>> {
            Ok(LlmResponse::new(
                "response",
                LlmFinishReason::Stop,
                None,
                Some("response-id".to_owned()),
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

        async fn finalize(
            self,
        ) -> Result<
            SqliteInteractiveSessionFinalization,
            SqliteInteractiveSessionFinalizationError<Self::Error>,
        > {
            record(&self.events, "finalize");
            match self.behavior {
                FinalizationBehavior::Idle => Ok(SqliteInteractiveSessionFinalization::new(false)),
                FinalizationBehavior::Active => Ok(SqliteInteractiveSessionFinalization::new(true)),
                FinalizationBehavior::CloseFailed => {
                    Err(SqliteInteractiveSessionFinalizationError::new(
                        SqliteInteractiveSessionFinalizationFailure::CleanupFailed(FakeError(
                            "close",
                        )),
                        None,
                    ))
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
        DeclareDeactivate,
        Fail,
        Unavailable,
        Cancelled,
    }

    #[derive(Clone)]
    struct FakeRuntime {
        events: Events,
        behavior: RuntimeBehavior,
    }

    impl TrustedLuaRuntimeExecutor for FakeRuntime {
        type Error = FakeError;

        fn start(
            &self,
            _program: OwnedLuaProgram,
            bindings: TrustedLuaRuntimeBindings,
        ) -> TrustedLuaExecutionHandle<Self::Error> {
            record(&self.events, "start");
            let behavior = self.behavior;
            let (_common, phase, finalizer) = bindings.into_parts();
            let TrustedLuaPhaseBindings::Extract(extract) = phase else {
                panic!("Hosting Extract 测试只应接收 Extract bindings")
            };
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let cancelled = Arc::new(AtomicBool::new(false));
            tokio::spawn(async move {
                let runtime = match behavior {
                    RuntimeBehavior::Complete => Ok(()),
                    RuntimeBehavior::DeclareDeactivate => extract
                        .clear_standard()
                        .map_err(TrustedLuaRuntimeExecutionError::Binding),
                    RuntimeBehavior::Fail => {
                        Err(TrustedLuaRuntimeExecutionError::Execute(FakeError("vm")))
                    }
                    RuntimeBehavior::Unavailable => Err(
                        TrustedLuaRuntimeExecutionError::Unavailable(FakeError("unavailable")),
                    ),
                    RuntimeBehavior::Cancelled => Err(TrustedLuaRuntimeExecutionError::Cancelled),
                };
                let finalization = finalizer.finalize().await;
                let _ = sender.send(TrustedLuaRuntimeExecutionReport::new(runtime, finalization));
            });
            TrustedLuaExecutionHandle::new(receiver, cancelled)
        }
    }

    type Service =
        TrustedLuaExecutionHostingService<FakeFileReader, FakeLlm, FakeRuntime, FakeSessionFactory>;

    fn service(
        events: Events,
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
                behavior: runtime,
            },
            FakeSessionFactory {
                events,
                fail: fail_open,
                finalization,
            },
        )
    }

    fn invocation() -> LuaInvocation<()> {
        let project = OpenedProject::new(
            "demo".parse::<ProjectName>().unwrap(),
            PathBuf::from("C:/projects/demo"),
            PathBuf::from("C:/projects/demo/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::rpg_maker::project::test_layout_profile(),
        );
        LuaInvocation::extract(
            PathBuf::from("main.lua"),
            LuaProjectContext::for_frozen_source(
                project.name().as_str(),
                project.source_root().to_path_buf(),
                project.database_path().to_path_buf(),
                project.language_pair().clone(),
            ),
        )
    }

    #[tokio::test]
    async fn opens_the_session_before_synchronously_handing_it_to_the_runtime() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let outcome = service(
            Arc::clone(&events),
            false,
            RuntimeBehavior::Complete,
            FinalizationBehavior::Idle,
        )
        .execute(invocation())
        .await
        .unwrap();

        assert_eq!(
            outcome,
            OperationCompletion::Completed(TrustedLuaExecutionOutcome::Empty)
        );

        assert_eq!(
            *events.lock().unwrap(),
            ["read", "open", "start", "finalize"]
        );
    }

    #[tokio::test]
    async fn returns_extract_intent_only_after_runtime_and_finalizer_both_succeed() {
        let outcome = service(
            Arc::new(Mutex::new(Vec::new())),
            false,
            RuntimeBehavior::DeclareDeactivate,
            FinalizationBehavior::Idle,
        )
        .execute(invocation())
        .await
        .expect("VM 与会话清理成功后应该交还 Extract 意图");

        assert_eq!(
            outcome,
            OperationCompletion::Completed(TrustedLuaExecutionOutcome::ExtractIntent(
                TrustedLuaExtractIntent::Deactivate
            ))
        );
    }

    #[tokio::test]
    async fn discards_extract_intent_when_session_finishes_with_active_transaction() {
        let error = service(
            Arc::new(Mutex::new(Vec::new())),
            false,
            RuntimeBehavior::DeclareDeactivate,
            FinalizationBehavior::Active,
        )
        .execute(invocation())
        .await
        .expect_err("未闭合事务必须阻止托管快照意图离开 Host");

        assert!(matches!(
            error,
            TrustedLuaExecutionHostingError::UnclosedTransaction
        ));
    }

    #[tokio::test]
    async fn discards_extract_intent_when_session_cleanup_fails() {
        let error = service(
            Arc::new(Mutex::new(Vec::new())),
            false,
            RuntimeBehavior::DeclareDeactivate,
            FinalizationBehavior::CloseFailed,
        )
        .execute(invocation())
        .await
        .expect_err("清理失败必须阻止托管快照意图离开 Host");

        assert!(matches!(error, TrustedLuaExecutionHostingError::Cleanup(_)));
    }

    #[tokio::test]
    async fn runtime_unavailability_still_finalizes_the_open_session() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let error = service(
            Arc::clone(&events),
            false,
            RuntimeBehavior::Unavailable,
            FinalizationBehavior::Idle,
        )
        .execute(invocation())
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            TrustedLuaExecutionHostingError::Runtime(TrustedLuaRuntimeExecutionError::Unavailable(
                FakeError("unavailable")
            ))
        ));
        assert_eq!(
            *events.lock().unwrap(),
            ["read", "open", "start", "finalize"]
        );
    }

    #[tokio::test]
    async fn runtime_cancellation_is_normal_only_after_session_finalization_succeeds() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let completion = service(
            Arc::clone(&events),
            false,
            RuntimeBehavior::Cancelled,
            FinalizationBehavior::Active,
        )
        .execute(invocation())
        .await
        .expect("取消时回滚活动事务并关闭会话应是正常结果");

        assert_eq!(completion, OperationCompletion::Cancelled);
        assert_eq!(
            *events.lock().unwrap(),
            ["read", "open", "start", "finalize"]
        );
    }

    #[tokio::test]
    async fn open_failure_does_not_start_the_runtime() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let error = service(
            Arc::clone(&events),
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
        assert_eq!(*events.lock().unwrap(), ["read", "open"]);
    }

    #[tokio::test]
    async fn active_transaction_is_rolled_back_and_reported_after_normal_return() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let error = service(
            events,
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
                RuntimeBehavior::Complete,
                FinalizationBehavior::Idle,
            )
            .execute(invocation()),
        );
    }

    #[derive(Clone, Default)]
    struct SourceFileSystem {
        reads: Arc<Mutex<Vec<PathBuf>>>,
        lists: Arc<Mutex<Vec<PathBuf>>>,
    }

    impl FileReader for SourceFileSystem {
        type Error = FakeError;

        async fn read_file(&self, path: PathBuf) -> Result<ReadFile, ReadFileError<Self::Error>> {
            self.reads.lock().unwrap().push(path.clone());
            Ok(ReadFile::new(path, b"source".to_vec()))
        }
    }

    impl DirectoryLister for SourceFileSystem {
        type Error = FakeError;

        async fn list_directory(
            &self,
            path: PathBuf,
        ) -> Result<Vec<DirectoryEntry>, ListDirectoryError<Self::Error>> {
            self.lists.lock().unwrap().push(path.clone());
            Ok(vec![
                DirectoryEntry::new(path.join("z.json"), DirectoryEntryKind::RegularFile),
                DirectoryEntry::new(path.join("a.json"), DirectoryEntryKind::RegularFile),
            ])
        }
    }

    #[tokio::test]
    async fn source_calls_join_the_frozen_root_and_return_sorted_relative_paths() {
        let file_system = Arc::new(SourceFileSystem::default());
        let opened = OpenedProject::new(
            "demo".parse::<ProjectName>().unwrap(),
            PathBuf::from("C:/projects/demo"),
            PathBuf::from("C:/projects/demo/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::rpg_maker::project::test_layout_profile(),
        );
        let calls = LuaCommonHostCalls::<_, _> {
            project: LuaProjectContext::for_frozen_source(
                opened.name().as_str(),
                opened.source_root().to_path_buf(),
                opened.database_path().to_path_buf(),
                opened.language_pair().clone(),
            ),
            operations: Arc::new(FakeOperations),
            file_system: Arc::clone(&file_system),
        };

        let bytes = calls
            .read_source(LuaSourcePath::parse("data/Items.json").unwrap())
            .await
            .unwrap();
        assert_eq!(bytes, b"source");
        let entries = calls
            .list_source(LuaSourcePath::parse("data").unwrap())
            .await
            .unwrap();
        assert_eq!(
            entries,
            vec!["data/a.json".to_owned(), "data/z.json".to_owned()]
        );
        assert_eq!(
            *file_system.reads.lock().unwrap(),
            [PathBuf::from("C:/projects/demo/source/data/Items.json")]
        );
        assert_eq!(
            *file_system.lists.lock().unwrap(),
            [PathBuf::from("C:/projects/demo/source/data")]
        );
    }
}
