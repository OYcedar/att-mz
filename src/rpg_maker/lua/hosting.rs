//! 可信 Lua 程序的冻结来源、项目上下文、数据库、LLM 与资源终态编排。

use std::error::Error;
use std::fmt;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, FailureReport, RecoveryFact, ReportedFailure,
    SafeDiagnostic, SafeDiagnosticSource,
};
use crate::execution::OperationCompletion;
use crate::llm::{
    ChatMessage, LlmRequestDiagnosticSource, LlmRequestError, LlmRequestExecutor, LlmResponse,
};
use crate::storage::file_system::{DirectoryLister, FileReader, ListDirectoryError, ReadFileError};
use crate::storage::scoped_path::{
    ExactDirectoryEntryResolutionError, resolve_exact_directory_entry,
};
use crate::storage::sqlite::{SqliteCommand, SqliteQuery, SqliteRow};
use crate::storage::sqlite_session::{
    OpenSqliteInteractiveSessionError, SqliteInteractiveSessionError,
    SqliteInteractiveSessionFactory, SqliteInteractiveSessionFinalizationFailure,
    SqliteInteractiveSessionFinalizer, SqliteInteractiveSessionOperations,
};
use crate::windows_path::WindowsOrdinalCaseKeyError;

#[cfg(test)]
use super::runtime::OwnedLuaProgram;
use super::runtime::{
    TrustedLuaBindingFinalization, TrustedLuaBindingFinalizationError, TrustedLuaBindingFinalizer,
    TrustedLuaCommonBindings, TrustedLuaCommonHostCalls, TrustedLuaExtractHostCalls,
    TrustedLuaExtractIntent, TrustedLuaHostCallError, TrustedLuaRuntimeBindings,
    TrustedLuaRuntimeExecutionError, TrustedLuaRuntimeExecutor, TrustedLuaStandardAcceptance,
    TrustedLuaStandardCandidate, TrustedLuaStandardHostCalls, TrustedLuaStandardSession,
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
    <F as FileReader>::Error: SafeDiagnosticSource,
    L: LlmRequestExecutor + 'static,
    L::Error: LlmRequestDiagnosticSource,
    R: TrustedLuaRuntimeExecutor,
    R::Error: SafeDiagnosticSource,
    S: SqliteInteractiveSessionFactory,
    S::Error: SafeDiagnosticSource,
    <S::Operations as SqliteInteractiveSessionOperations>::Error: SafeDiagnosticSource,
{
    type TranslationClient = L::Client;
    type Error = TrustedLuaExecutionHostingError<S::Error, R::Error>;

    async fn execute(
        &self,
        invocation: LuaInvocation<Self::TranslationClient>,
    ) -> Result<OperationCompletion<TrustedLuaExecutionOutcome>, Self::Error> {
        let (phase, program, project) = match invocation {
            LuaInvocation::Extract { program, project } => {
                (HostingPhase::Extract, program, project)
            }
            LuaInvocation::Translate {
                program,
                project,
                llm_client,
                semantics,
            } => (
                HostingPhase::Translate {
                    llm_client,
                    semantics,
                },
                program,
                project,
            ),
            LuaInvocation::WriteBack {
                program,
                project,
                calls,
            } => (HostingPhase::WriteBack(calls), program, project),
            LuaInvocation::Project {
                program,
                project,
                arguments,
                standard,
            } => (
                HostingPhase::Project {
                    arguments,
                    standard,
                },
                program,
                project,
            ),
        };

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
        let transaction_state = Arc::clone(&common);
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
            HostingPhase::Project {
                arguments,
                standard,
            } => TrustedLuaRuntimeBindings::project(
                common,
                arguments,
                Arc::new(TransactionAwareLuaStandardHostCalls {
                    inner: standard,
                    transaction_state,
                }),
                finalizer,
            ),
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
    Project {
        arguments: Vec<String>,
        standard: Arc<dyn TrustedLuaStandardHostCalls>,
    },
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
    <F as FileReader>::Error: SafeDiagnosticSource,
    S: SqliteInteractiveSessionOperations,
    S::Error: SafeDiagnosticSource,
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
        let source_root = self.project.source_root().to_path_buf();
        Box::pin(async move {
            let requested =
                resolve_exact_source_path(file_system.as_ref(), &source_root, &path).await?;
            file_system
                .read_file(requested)
                .await
                .map(|file| file.into_bytes())
                .map_err(|error| source_read_error("source.read", error))
        })
    }

    fn list_source(
        &self,
        path: LuaSourcePath,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, TrustedLuaHostCallError>> + Send + 'static>>
    {
        let file_system = Arc::clone(&self.file_system);
        let source_root = self.project.source_root().to_path_buf();
        Box::pin(async move {
            let requested =
                resolve_exact_source_path(file_system.as_ref(), &source_root, &path).await?;
            let entries = file_system
                .list_directory(requested)
                .await
                .map_err(|error| source_list_error("source.list", error))?;
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
                        "来源目录项无法构造安全相对路径",
                        None,
                        Some(Arc::new(error)),
                    )
                    .with_operation("source.list")
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
        Box::pin(async move {
            operations
                .query(query)
                .await
                .map_err(|error| database_call_error("db.query", error))
        })
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
                .map_err(|error| database_call_error("db.execute", error))
        })
    }

    fn begin(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>> {
        let operations = Arc::clone(&self.operations);
        Box::pin(async move {
            operations
                .begin()
                .await
                .map_err(|error| database_call_error("db.begin", error))
        })
    }

    fn commit(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>> {
        let operations = Arc::clone(&self.operations);
        Box::pin(async move {
            operations
                .commit()
                .await
                .map_err(|error| database_call_error("db.commit", error))
        })
    }

    fn rollback(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>> {
        let operations = Arc::clone(&self.operations);
        Box::pin(async move {
            operations
                .rollback()
                .await
                .map_err(|error| database_call_error("db.rollback", error))
        })
    }

    fn transaction_active(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<bool, TrustedLuaHostCallError>> + Send + 'static>> {
        let operations = Arc::clone(&self.operations);
        Box::pin(async move {
            operations
                .transaction_active()
                .await
                .map_err(|error| database_call_error("db.transaction_active", error))
        })
    }
}

struct TransactionAwareLuaStandardHostCalls {
    inner: Arc<dyn TrustedLuaStandardHostCalls>,
    transaction_state: Arc<dyn TrustedLuaCommonHostCalls>,
}

impl TrustedLuaStandardHostCalls for TransactionAwareLuaStandardHostCalls {
    fn open(
        &self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Arc<dyn TrustedLuaStandardSession>, TrustedLuaHostCallError>>
                + Send
                + 'static,
        >,
    > {
        let inner = Arc::clone(&self.inner);
        let transaction_state = Arc::clone(&self.transaction_state);
        Box::pin(async move {
            let session = inner.open().await?;
            Ok(Arc::new(TransactionAwareLuaStandardSession {
                inner: session,
                transaction_state,
            }) as Arc<dyn TrustedLuaStandardSession>)
        })
    }
}

struct TransactionAwareLuaStandardSession {
    inner: Arc<dyn TrustedLuaStandardSession>,
    transaction_state: Arc<dyn TrustedLuaCommonHostCalls>,
}

impl TrustedLuaStandardSession for TransactionAwareLuaStandardSession {
    fn units(&self) -> Vec<super::runtime::TrustedLuaStandardUnit> {
        self.inner.units()
    }

    fn get(
        &self,
        owner: crate::rpg_maker::standard_asset::RpgMakerStandardAssetOwner,
        group_location: crate::rpg_maker::text::RpgMakerLocation,
        role: crate::rpg_maker::model::TextUnitRole,
    ) -> Option<super::runtime::TrustedLuaStandardUnit> {
        self.inner.get(owner, group_location, role)
    }

    fn accept(
        &self,
        candidates: Vec<TrustedLuaStandardCandidate>,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<TrustedLuaStandardAcceptance>, TrustedLuaHostCallError>>
                + Send
                + 'static,
        >,
    > {
        let transaction_state = Arc::clone(&self.transaction_state);
        let inner = Arc::clone(&self.inner);
        Box::pin(async move {
            if transaction_state.transaction_active().await? {
                return Err(TrustedLuaHostCallError::new(
                    "standard",
                    "transaction_conflict",
                    "活动 ctx.db 事务中不能提交 Standard 人工译文",
                    None,
                    None,
                )
                .with_operation("standard.accept"));
            }
            inner.accept(candidates).await
        })
    }
}

async fn resolve_exact_source_path<F>(
    file_system: &F,
    source_root: &Path,
    logical_path: &LuaSourcePath,
) -> Result<PathBuf, TrustedLuaHostCallError>
where
    F: DirectoryLister,
    F::Error: SafeDiagnosticSource,
{
    let mut current = source_root.to_path_buf();
    for component in logical_path.components() {
        let entries = file_system
            .list_directory(current.clone())
            .await
            .map_err(|error| source_list_error("source.resolve", error))?;
        let resolved = resolve_exact_directory_entry(
            &current,
            component,
            entries.iter().map(|entry| entry.resolved_path()),
        )
        .map_err(|error| source_resolution_error("source.resolve", error))?;
        match resolved {
            Some(resolved) => current = resolved,
            None => {
                // 缺失不是身份冲突：让最终 read/list 或下一层目录列举通过各自
                // 已有的 NotFound 契约报告，避免解析器制造第二套文件系统错误。
                current = current.join(component);
            }
        };
    }
    Ok(current)
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
    L::Error: LlmRequestDiagnosticSource,
{
    llm_client: Arc<L::Client>,
    semantics: Arc<dyn TrustedLuaTranslationSemantics>,
    llm: LuaLlmCapability<L>,
}

impl<L> TrustedLuaTranslateHostCalls for LuaTranslationHostCalls<L>
where
    L: LlmRequestExecutor + 'static,
    L::Error: LlmRequestDiagnosticSource,
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
        semantic_context: String,
    ) -> Result<Arc<dyn super::runtime::TrustedLuaPreparedTranslation>, TrustedLuaHostCallError>
    {
        self.semantics
            .prepare_translation(kind, original, semantic_context)
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
                .map_err(|error| error.with_operation("llm.request"))
        })
    }
}

fn database_call_error<E>(
    operation: &'static str,
    error: SqliteInteractiveSessionError<E>,
) -> TrustedLuaHostCallError
where
    E: Error + SafeDiagnosticSource + Send + Sync + 'static,
{
    let (kind, message, diagnostic) = match &error {
        SqliteInteractiveSessionError::Closed => (
            "closed",
            "SQLite Host 会话已经进入终结阶段",
            sqlite_session_state_diagnostic(
                operation,
                DiagnosticFailureKind::ExecutorClosed,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::Retry,
            ),
        ),
        SqliteInteractiveSessionError::Indeterminate => (
            "indeterminate",
            "SQLite Host 会话的事务终态未知",
            sqlite_session_state_diagnostic(
                operation,
                DiagnosticFailureKind::TransactionOutcomeUnknown,
                DiagnosticImpact::OutcomeUnknown,
                DiagnosticAction::PreserveRecoveryArtifacts,
            ),
        ),
        SqliteInteractiveSessionError::TransactionAlreadyActive => (
            "transaction_already_active",
            "SQLite Host 事务已经开始",
            sqlite_session_state_diagnostic(
                operation,
                DiagnosticFailureKind::StateMismatch,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixInput,
            ),
        ),
        SqliteInteractiveSessionError::NoActiveTransaction => (
            "no_active_transaction",
            "SQLite Host 当前没有活动事务",
            sqlite_session_state_diagnostic(
                operation,
                DiagnosticFailureKind::StateMismatch,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixInput,
            ),
        ),
        SqliteInteractiveSessionError::OperationFailed(source) => (
            "operation_failed",
            "SQLite Host 操作失败",
            source.safe_diagnostic_source(
                DiagnosticStage::ProjectOpening,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
            ),
        ),
        SqliteInteractiveSessionError::OutcomeUnknown(source) => (
            "outcome_unknown",
            "SQLite Host 操作的事务终态未知",
            source.safe_diagnostic_source(
                DiagnosticStage::ProjectOpening,
                DiagnosticImpact::OutcomeUnknown,
                DiagnosticAction::PreserveRecoveryArtifacts,
            ),
        ),
    };
    TrustedLuaHostCallError::new("sqlite", kind, message, None, Some(Arc::new(error)))
        .with_operation(operation)
        .with_safe_diagnostic(diagnostic)
}

fn sqlite_session_state_diagnostic(
    operation: &'static str,
    failure: DiagnosticFailureKind,
    impact: DiagnosticImpact,
    action: DiagnosticAction,
) -> SafeDiagnostic {
    SafeDiagnostic::new(
        DiagnosticCode::SqliteOperation,
        DiagnosticStage::ProjectOpening,
        DiagnosticSubject::operation(operation),
        DiagnosticReason::failure(failure),
        impact,
        action,
    )
}

fn source_read_error<E>(operation: &'static str, error: ReadFileError<E>) -> TrustedLuaHostCallError
where
    E: Error + SafeDiagnosticSource + Send + Sync + 'static,
{
    let (kind, message, diagnostic) = match &error {
        ReadFileError::NotFound { path } => (
            "not_found",
            "Lua Host 要读取的来源文件不存在",
            file_state_diagnostic(path, DiagnosticFailureKind::NotFound),
        ),
        ReadFileError::NotFile { path } => (
            "not_file",
            "Lua Host 要读取的来源路径不是普通文件",
            file_state_diagnostic(path, DiagnosticFailureKind::InvalidPath),
        ),
        ReadFileError::Io { path, source } => {
            let mut diagnostic = source.safe_diagnostic_source(
                DiagnosticStage::ProjectOpening,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckPathAndPermissions,
            );
            diagnostic.subject = DiagnosticSubject::path(path);
            ("io", "Lua Host 读取来源文件失败", diagnostic)
        }
    };
    TrustedLuaHostCallError::new("filesystem", kind, message, None, Some(Arc::new(error)))
        .with_operation(operation)
        .with_safe_diagnostic(diagnostic)
}

fn source_list_error<E>(
    operation: &'static str,
    error: ListDirectoryError<E>,
) -> TrustedLuaHostCallError
where
    E: Error + SafeDiagnosticSource + Send + Sync + 'static,
{
    let (kind, message, diagnostic) = match &error {
        ListDirectoryError::NotFound { path } => (
            "not_found",
            "Lua Host 要列举的来源目录不存在",
            file_state_diagnostic(path, DiagnosticFailureKind::NotFound),
        ),
        ListDirectoryError::NotDirectory { path } => (
            "not_directory",
            "Lua Host 要列举的来源路径不是目录",
            file_state_diagnostic(path, DiagnosticFailureKind::InvalidPath),
        ),
        ListDirectoryError::Io { path, source } => {
            let mut diagnostic = source.safe_diagnostic_source(
                DiagnosticStage::ProjectOpening,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckPathAndPermissions,
            );
            diagnostic.subject = DiagnosticSubject::path(path);
            ("io", "Lua Host 列举来源目录失败", diagnostic)
        }
    };
    TrustedLuaHostCallError::new("filesystem", kind, message, None, Some(Arc::new(error)))
        .with_operation(operation)
        .with_safe_diagnostic(diagnostic)
}

fn file_state_diagnostic(path: &Path, failure: DiagnosticFailureKind) -> SafeDiagnostic {
    SafeDiagnostic::new(
        DiagnosticCode::FileSystemOperation,
        DiagnosticStage::ProjectOpening,
        DiagnosticSubject::path(path),
        DiagnosticReason::failure(failure),
        DiagnosticImpact::Unchanged,
        DiagnosticAction::CheckPathAndPermissions,
    )
}

fn source_resolution_error(
    operation: &'static str,
    error: ExactDirectoryEntryResolutionError,
) -> TrustedLuaHostCallError {
    let (kind, message, diagnostic) = match &error {
        ExactDirectoryEntryResolutionError::CaseMismatch(error) => (
            "case_mismatch",
            "Lua Host 来源路径的大小写与磁盘上的真实名称不一致",
            SafeDiagnostic::new(
                DiagnosticCode::FileSystemOperation,
                DiagnosticStage::ProjectOpening,
                DiagnosticSubject::path(error.requested()),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::InvalidPath,
                    "requested path casing does not match the actual directory entry",
                ),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixInput,
            )
            .with_recovery(RecoveryFact::path(error.actual())),
        ),
        ExactDirectoryEntryResolutionError::CaseKey { path, source } => {
            let diagnostic = match source {
                WindowsOrdinalCaseKeyError::InputTooLarge { maximum, observed } => {
                    SafeDiagnostic::new(
                        DiagnosticCode::FileSystemOperation,
                        DiagnosticStage::ProjectOpening,
                        DiagnosticSubject::path(path),
                        DiagnosticReason::Resource {
                            resource: "Windows 文件名 UTF-16 单元数".to_owned(),
                            actual: *observed,
                            maximum: Some(*maximum),
                        },
                        DiagnosticImpact::Unchanged,
                        DiagnosticAction::FixInput,
                    )
                }
                WindowsOrdinalCaseKeyError::WindowsApi { phase, source } => SafeDiagnostic::io(
                    DiagnosticCode::FileSystemOperation,
                    DiagnosticStage::ProjectOpening,
                    DiagnosticSubject::path(path),
                    "windows_ordinal_case_key",
                    source,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::Retry,
                )
                .with_recovery(RecoveryFact::component(format!(
                    "windows_ordinal_case_key_phase={}",
                    phase.as_str()
                ))),
            };
            (
                "io",
                "Lua Host 无法建立来源路径的 Windows 非大小写身份",
                diagnostic,
            )
        }
    };
    TrustedLuaHostCallError::new("filesystem", kind, message, None, Some(Arc::new(error)))
        .with_operation(operation)
        .with_safe_diagnostic(diagnostic)
}

fn llm_call_error<E>(error: LlmRequestError<E>) -> TrustedLuaHostCallError
where
    E: Error + LlmRequestDiagnosticSource + Send + Sync + 'static,
{
    let (kind, retry_after_ms, diagnostic) = match &error {
        LlmRequestError::Retryable {
            source,
            retry_after,
        } => (
            "retryable",
            retry_after.map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)),
            source.request_diagnostic(*retry_after, DiagnosticImpact::Unchanged),
        ),
        LlmRequestError::Fatal(source) => (
            "fatal",
            None,
            source.request_diagnostic(None, DiagnosticImpact::Unchanged),
        ),
    };
    TrustedLuaHostCallError::new(
        "llm",
        kind,
        "Lua Host 模型请求失败",
        retry_after_ms,
        Some(Arc::new(error)),
    )
    .with_safe_diagnostic(diagnostic)
}

struct LuaSessionFinalizer<F> {
    finalizer: F,
}

impl<F> TrustedLuaBindingFinalizer for LuaSessionFinalizer<F>
where
    F: SqliteInteractiveSessionFinalizer,
    F::Error: SafeDiagnosticSource,
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
                    let (message, primary_impact) = match error.primary() {
                        SqliteInteractiveSessionFinalizationFailure::CleanupFailed(_) => (
                            "Lua Host 无法完整终结 SQLite 会话",
                            DiagnosticImpact::RecoveryRequired,
                        ),
                        SqliteInteractiveSessionFinalizationFailure::OutcomeUnknown(_) => (
                            "Lua Host 终结 SQLite 会话后的事务终态未知",
                            DiagnosticImpact::OutcomeUnknown,
                        ),
                    };
                    let mut diagnostics = vec![error.primary().source().safe_diagnostic_source(
                        DiagnosticStage::ProjectOpening,
                        primary_impact,
                        DiagnosticAction::PreserveRecoveryArtifacts,
                    )];
                    if let Some(source) = error.connection_close() {
                        diagnostics.push(source.safe_diagnostic_source(
                            DiagnosticStage::ProjectOpening,
                            DiagnosticImpact::RecoveryRequired,
                            DiagnosticAction::PreserveRecoveryArtifacts,
                        ));
                    }
                    Err(
                        TrustedLuaBindingFinalizationError::new(message, Some(Arc::new(error)))
                            .with_safe_diagnostics(diagnostics),
                    )
                }
            }
        })
    }
}

/// Host 在数据库建立、VM 或收尾阶段遇到的失败。
#[derive(Debug)]
pub(crate) enum TrustedLuaExecutionHostingError<O, R> {
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

impl<O, R> TrustedLuaExecutionHostingError<O, R>
where
    O: SafeDiagnosticSource,
    R: SafeDiagnosticSource,
{
    /// 在 Host 仍持有数据库路径、VM 子阶段与稳定 Host code 时建立公开投影。
    ///
    /// VM 任意文本、Lua 正文、SQL/参数与底层 `Display` 永不进入该投影。
    pub(crate) fn safe_diagnostic(
        &self,
        stage: DiagnosticStage,
        script_path: &Path,
        impact: DiagnosticImpact,
    ) -> SafeDiagnostic {
        match self {
            Self::OpenDatabase {
                database_path,
                source,
            } => match source {
                OpenSqliteInteractiveSessionError::NotFound => SafeDiagnostic::new(
                    DiagnosticCode::LuaExecution,
                    stage,
                    DiagnosticSubject::path(database_path),
                    DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
                    impact,
                    DiagnosticAction::CheckProjectState,
                )
                .with_recovery(RecoveryFact::path(script_path))
                .with_recovery(RecoveryFact::component("lua_runtime_phase=open_database")),
                OpenSqliteInteractiveSessionError::OpenFailed(source) => {
                    let mut diagnostic = source.safe_diagnostic_source(
                        stage,
                        impact,
                        DiagnosticAction::CheckProjectState,
                    );
                    diagnostic.subject = DiagnosticSubject::path(database_path);
                    with_lua_context(diagnostic, stage, script_path, impact, "open_database")
                }
            },
            Self::Runtime(runtime) => lua_runtime_diagnostic(runtime, stage, script_path, impact),
            Self::Cleanup(cleanup) => lua_cleanup_diagnostics(cleanup, stage, script_path)
                .into_iter()
                .next()
                .expect("Lua cleanup 必须至少产生一条安全诊断"),
            Self::UnclosedTransaction => SafeDiagnostic::new(
                DiagnosticCode::LuaExecution,
                stage,
                DiagnosticSubject::path(script_path),
                DiagnosticReason::failure(DiagnosticFailureKind::LuaUnclosedTransaction),
                impact,
                DiagnosticAction::FixInput,
            )
            .with_recovery(RecoveryFact::transaction("rolled_back")),
            Self::RuntimeAndCleanup { runtime, .. } => lua_runtime_diagnostic(
                runtime,
                stage,
                script_path,
                DiagnosticImpact::RecoveryRequired,
            )
            .with_recovery(RecoveryFact::component("lua_binding_finalization=failed")),
        }
    }

    /// 消费 Host 错误并保留 VM 主错误与资源收尾错误两个独立终态。
    pub(crate) fn into_failure_report(
        self,
        stage: DiagnosticStage,
        script_path: &Path,
        impact: DiagnosticImpact,
    ) -> FailureReport
    where
        O: Error + Send + Sync + 'static,
        R: Error + Send + Sync + 'static,
    {
        let diagnostic = self.safe_diagnostic(stage, script_path, impact);
        match self {
            Self::RuntimeAndCleanup { runtime, cleanup } => {
                let mut report = FailureReport::new(ReportedFailure::new(diagnostic, runtime));
                for public in lua_cleanup_diagnostics(&cleanup, stage, script_path) {
                    report = report.with_related(ReportedFailure::new(public, cleanup.clone()));
                }
                report
            }
            Self::Cleanup(cleanup) => {
                let mut diagnostics = lua_cleanup_diagnostics(&cleanup, stage, script_path);
                let primary = diagnostics.remove(0);
                let mut report = FailureReport::new(ReportedFailure::new(primary, cleanup.clone()));
                for public in diagnostics {
                    report = report.with_related(ReportedFailure::new(public, cleanup.clone()));
                }
                report
            }
            source => FailureReport::new(ReportedFailure::new(diagnostic, source)),
        }
    }
}

fn lua_runtime_diagnostic<R>(
    source: &TrustedLuaRuntimeExecutionError<R>,
    stage: DiagnosticStage,
    script_path: &Path,
    impact: DiagnosticImpact,
) -> SafeDiagnostic
where
    R: SafeDiagnosticSource,
{
    match source {
        TrustedLuaRuntimeExecutionError::Unavailable(source) => with_lua_context(
            source.safe_diagnostic_source(stage, impact, DiagnosticAction::Retry),
            stage,
            script_path,
            impact,
            "unavailable",
        ),
        TrustedLuaRuntimeExecutionError::Context(source) => with_lua_context(
            classify_lua_vm_phase(
                source.safe_diagnostic_source(stage, impact, DiagnosticAction::FixInput),
                DiagnosticFailureKind::LuaContextCreationFailed,
                DiagnosticAction::FixInput,
            ),
            stage,
            script_path,
            impact,
            "context",
        ),
        TrustedLuaRuntimeExecutionError::Compile(source) => with_lua_context(
            classify_lua_vm_phase(
                source.safe_diagnostic_source(stage, impact, DiagnosticAction::FixInput),
                DiagnosticFailureKind::LuaCompilationFailed,
                DiagnosticAction::FixInput,
            ),
            stage,
            script_path,
            impact,
            "compile",
        ),
        TrustedLuaRuntimeExecutionError::Execute(source) => with_lua_context(
            classify_lua_vm_phase(
                source.safe_diagnostic_source(stage, impact, DiagnosticAction::FixInput),
                DiagnosticFailureKind::LuaExecutionFailed,
                DiagnosticAction::FixInput,
            ),
            stage,
            script_path,
            impact,
            "execute",
        ),
        TrustedLuaRuntimeExecutionError::Binding(source) => {
            let diagnostic = source.safe_diagnostic().cloned().unwrap_or_else(|| {
                SafeDiagnostic::new(
                    DiagnosticCode::LuaExecution,
                    stage,
                    DiagnosticSubject::path(script_path),
                    DiagnosticReason::failure(DiagnosticFailureKind::LuaHostCallFailed),
                    impact,
                    DiagnosticAction::FixInput,
                )
            });
            let mut diagnostic =
                with_lua_context(diagnostic, stage, script_path, impact, "host_call")
                    .with_recovery(RecoveryFact::component(format!(
                        "host_domain={}; host_kind={}",
                        source.domain(),
                        source.kind()
                    )));
            if let Some(operation) = source.operation() {
                diagnostic = diagnostic.with_recovery(RecoveryFact::component(format!(
                    "host_operation={operation}"
                )));
            }
            diagnostic
        }
        TrustedLuaRuntimeExecutionError::Cancelled => with_lua_context(
            SafeDiagnostic::new(
                DiagnosticCode::LuaExecution,
                stage,
                DiagnosticSubject::path(script_path),
                DiagnosticReason::failure(DiagnosticFailureKind::LockCancelled),
                impact,
                DiagnosticAction::Retry,
            ),
            stage,
            script_path,
            impact,
            "cancelled",
        ),
        TrustedLuaRuntimeExecutionError::WorkerPanicked => with_lua_context(
            SafeDiagnostic::new(
                DiagnosticCode::LuaExecution,
                stage,
                DiagnosticSubject::path(script_path),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::WorkerPanicked,
                    "lua_runtime_kind=worker_panicked",
                ),
                impact,
                DiagnosticAction::ReportBug,
            ),
            stage,
            script_path,
            impact,
            "worker",
        ),
        TrustedLuaRuntimeExecutionError::SupervisorLost => with_lua_context(
            SafeDiagnostic::new(
                DiagnosticCode::LuaExecution,
                stage,
                DiagnosticSubject::path(script_path),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::WorkerChannelClosed,
                    "lua_runtime_kind=supervisor_lost",
                ),
                impact,
                DiagnosticAction::ReportBug,
            ),
            stage,
            script_path,
            impact,
            "supervisor",
        ),
    }
}

fn classify_lua_vm_phase(
    mut diagnostic: SafeDiagnostic,
    phase_failure: DiagnosticFailureKind,
    action: DiagnosticAction,
) -> SafeDiagnostic {
    if matches!(
        &diagnostic.reason,
        DiagnosticReason::Failure {
            failure: DiagnosticFailureKind::LuaExecutionFailed
        }
    ) {
        diagnostic.reason = DiagnosticReason::failure(phase_failure);
        diagnostic.action = action;
    }
    diagnostic
}

fn lua_cleanup_diagnostics(
    cleanup: &TrustedLuaBindingFinalizationError,
    stage: DiagnosticStage,
    script_path: &Path,
) -> Vec<SafeDiagnostic> {
    let diagnostics = cleanup.safe_diagnostics();
    if diagnostics.is_empty() {
        return vec![with_lua_context(
            SafeDiagnostic::new(
                DiagnosticCode::LuaExecution,
                stage,
                DiagnosticSubject::path(script_path),
                DiagnosticReason::failure(DiagnosticFailureKind::LuaFinalizationFailed),
                DiagnosticImpact::RecoveryRequired,
                DiagnosticAction::Retry,
            ),
            stage,
            script_path,
            DiagnosticImpact::RecoveryRequired,
            "finalization",
        )];
    }
    diagnostics
        .iter()
        .cloned()
        .map(|diagnostic| {
            let impact = diagnostic.impact;
            with_lua_context(diagnostic, stage, script_path, impact, "finalization")
        })
        .collect()
}

fn with_lua_context(
    mut diagnostic: SafeDiagnostic,
    stage: DiagnosticStage,
    script_path: &Path,
    outer_impact: DiagnosticImpact,
    runtime_phase: &'static str,
) -> SafeDiagnostic {
    diagnostic.stage = stage;
    diagnostic.impact = merge_diagnostic_impact(diagnostic.impact, outer_impact);
    diagnostic
        .with_recovery(RecoveryFact::path(script_path))
        .with_recovery(RecoveryFact::component(format!(
            "lua_runtime_phase={runtime_phase}"
        )))
}

fn merge_diagnostic_impact(inner: DiagnosticImpact, outer: DiagnosticImpact) -> DiagnosticImpact {
    if inner == DiagnosticImpact::Unchanged {
        outer
    } else {
        inner
    }
}

impl<O, R> fmt::Display for TrustedLuaExecutionHostingError<O, R>
where
    O: fmt::Display,
    R: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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

impl<O, R> Error for TrustedLuaExecutionHostingError<O, R>
where
    O: Error + 'static,
    R: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
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
    use std::fs;
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
    use crate::runtime::filesystem::{
        SystemFileSystem, SystemFileSystemConfig, SystemFileSystemError,
    };
    use crate::runtime::sqlite::SqliteRuntimeError;
    use crate::storage::file_system::{
        DirectoryEntry, DirectoryEntryKind, DirectoryLister, ListDirectoryError, ReadFile,
    };
    use crate::storage::sqlite_session::{
        OpenedSqliteInteractiveSession, SqliteInteractiveSessionFinalization,
        SqliteInteractiveSessionFinalizationError, SqliteInteractiveSessionFinalizationFailure,
    };

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeError {
        Untyped(&'static str),
        Operation {
            source_detail: &'static str,
            operation: &'static str,
        },
    }

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Untyped(message)
                | Self::Operation {
                    source_detail: message,
                    ..
                } => formatter.write_str(message),
            }
        }
    }

    impl Error for FakeError {}

    impl SafeDiagnosticSource for FakeError {
        fn safe_diagnostic_source(
            &self,
            stage: DiagnosticStage,
            impact: DiagnosticImpact,
            action: DiagnosticAction,
        ) -> SafeDiagnostic {
            let subject = match self {
                Self::Untyped(_) => DiagnosticSubject::component("fake Lua root"),
                Self::Operation { operation, .. } => DiagnosticSubject::operation(operation),
            };
            SafeDiagnostic::new(
                DiagnosticCode::LuaExecution,
                stage,
                subject,
                DiagnosticReason::failure(DiagnosticFailureKind::LuaExecutionFailed),
                impact,
                action,
            )
        }
    }

    impl LlmRequestDiagnosticSource for FakeError {
        fn request_diagnostic(
            &self,
            retry_after: Option<std::time::Duration>,
            impact: DiagnosticImpact,
        ) -> SafeDiagnostic {
            SafeDiagnostic::new(
                DiagnosticCode::ModelRequest,
                DiagnosticStage::ModelRequest,
                DiagnosticSubject::component("fake Lua LLM"),
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

    struct DiagnosticFinalizer;

    impl SqliteInteractiveSessionFinalizer for DiagnosticFinalizer {
        type Error = SqliteRuntimeError;

        async fn finalize(
            self,
        ) -> Result<
            SqliteInteractiveSessionFinalization,
            SqliteInteractiveSessionFinalizationError<Self::Error>,
        > {
            Err(SqliteInteractiveSessionFinalizationError::new(
                SqliteInteractiveSessionFinalizationFailure::OutcomeUnknown(
                    SqliteRuntimeError::Driver {
                        operation: "rollback_transaction",
                        source: rusqlite::Error::SqliteFailure(
                            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR_FSYNC),
                            Some("SQL_FINALIZATION_SENTINEL".to_owned()),
                        ),
                    },
                ),
                Some(SqliteRuntimeError::Io {
                    operation: "close_connection",
                    path: PathBuf::from("C:/project/project.db"),
                    source: std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "CLOSE_SOURCE_SENTINEL",
                    ),
                }),
            ))
        }
    }

    #[test]
    fn host_call_diagnostic_keeps_stable_codes_without_copying_lua_message() {
        let error: TrustedLuaExecutionHostingError<FakeError, FakeError> =
            TrustedLuaExecutionHostingError::Runtime(TrustedLuaRuntimeExecutionError::Binding(
                TrustedLuaHostCallError::new(
                    "filesystem",
                    "permission_denied",
                    "LUA_VM_AND_SOURCE_BODY_SENTINEL",
                    None,
                    None,
                ),
            ));
        let diagnostic = error.safe_diagnostic(
            DiagnosticStage::WriteBack,
            Path::new("scripts/write-back.lua"),
            DiagnosticImpact::Unchanged,
        );
        let serialized = serde_json::to_string(&diagnostic).expect("诊断应可序列化");

        assert!(!serialized.contains("LUA_VM_AND_SOURCE_BODY_SENTINEL"));
        assert!(serialized.contains("lua_host_call_failed"));
        assert!(serialized.contains("scripts/write-back.lua"));
        assert!(serialized.contains("host_domain=filesystem; host_kind=permission_denied"));
    }

    #[test]
    fn worker_panic_and_supervisor_loss_keep_distinct_stable_kind_and_phase() {
        let worker: TrustedLuaExecutionHostingError<FakeError, FakeError> =
            TrustedLuaExecutionHostingError::Runtime(
                TrustedLuaRuntimeExecutionError::WorkerPanicked,
            );
        let supervisor: TrustedLuaExecutionHostingError<FakeError, FakeError> =
            TrustedLuaExecutionHostingError::Runtime(
                TrustedLuaRuntimeExecutionError::SupervisorLost,
            );

        let worker = worker.safe_diagnostic(
            DiagnosticStage::Translate,
            Path::new("C:/project/scripts/translate.lua"),
            DiagnosticImpact::ProgressPreserved,
        );
        let supervisor = supervisor.safe_diagnostic(
            DiagnosticStage::Translate,
            Path::new("C:/project/scripts/translate.lua"),
            DiagnosticImpact::ProgressPreserved,
        );
        let worker = serde_json::to_string(&worker).expect("worker 诊断应可序列化");
        let supervisor = serde_json::to_string(&supervisor).expect("supervisor 诊断应可序列化");

        assert!(worker.contains("\"failure\":\"worker_panicked\""));
        assert!(worker.contains("lua_runtime_kind=worker_panicked"));
        assert!(worker.contains("lua_runtime_phase=worker"));
        assert!(supervisor.contains("\"failure\":\"worker_channel_closed\""));
        assert!(supervisor.contains("lua_runtime_kind=supervisor_lost"));
        assert!(supervisor.contains("lua_runtime_phase=supervisor"));
        assert_ne!(worker, supervisor);
    }

    #[test]
    fn sqlite_host_diagnostic_keeps_codes_operation_phase_and_path_without_sql_text() {
        const SQL_AND_PARAMETER_SENTINEL: &str = "SQL_AND_PARAMETER_SENTINEL";
        let driver = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE),
            Some(SQL_AND_PARAMETER_SENTINEL.to_owned()),
        );
        let host_error = database_call_error(
            "db.execute",
            SqliteInteractiveSessionError::OperationFailed(SqliteRuntimeError::Driver {
                operation: "execute_statement",
                source: driver,
            }),
        );
        let error: TrustedLuaExecutionHostingError<FakeError, FakeError> =
            TrustedLuaExecutionHostingError::Runtime(TrustedLuaRuntimeExecutionError::Binding(
                host_error,
            ));

        let diagnostic = error.safe_diagnostic(
            DiagnosticStage::Translate,
            Path::new("C:/project/scripts/translate.lua"),
            DiagnosticImpact::ProgressPreserved,
        );
        let serialized = serde_json::to_string(&diagnostic).expect("SQLite 诊断应可序列化");

        assert!(serialized.contains("\"primary_code\":19"));
        assert!(serialized.contains("\"extended_code\":2067"));
        assert!(serialized.contains("host_operation=db.execute"));
        assert!(serialized.contains("lua_runtime_phase=host_call"));
        assert!(serialized.contains("C:/project/scripts/translate.lua"));
        assert_eq!(diagnostic.impact, DiagnosticImpact::ProgressPreserved);
        assert!(!serialized.contains(SQL_AND_PARAMETER_SENTINEL));
    }

    #[test]
    fn filesystem_host_diagnostic_keeps_safe_os_reason_and_paths_without_source_text() {
        const FILE_SOURCE_SENTINEL: &str = "FILE_SOURCE_SENTINEL";
        let path = PathBuf::from("C:/game/data/Actors.json");
        let host_error = source_read_error(
            "source.read_json",
            ReadFileError::Io {
                path: path.clone(),
                source: SystemFileSystemError::Io {
                    operation: "read_file",
                    path: path.clone(),
                    source: std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        FILE_SOURCE_SENTINEL,
                    ),
                },
            },
        );
        let error: TrustedLuaExecutionHostingError<FakeError, FakeError> =
            TrustedLuaExecutionHostingError::Runtime(TrustedLuaRuntimeExecutionError::Binding(
                host_error,
            ));

        let diagnostic = error.safe_diagnostic(
            DiagnosticStage::Extract,
            Path::new("C:/project/scripts/extract.lua"),
            DiagnosticImpact::Unchanged,
        );
        let serialized = serde_json::to_string(&diagnostic).expect("文件诊断应可序列化");

        assert!(serialized.contains("permission_denied"));
        assert!(serialized.contains("read_file"));
        assert!(serialized.contains("C:/game/data/Actors.json"));
        assert!(serialized.contains("C:/project/scripts/extract.lua"));
        assert!(serialized.contains("host_operation=source.read_json"));
        assert!(!serialized.contains(FILE_SOURCE_SENTINEL));
    }

    #[tokio::test]
    async fn sqlite_finalization_keeps_outcome_unknown_and_close_failure_separately() {
        let cleanup = Box::new(LuaSessionFinalizer {
            finalizer: DiagnosticFinalizer,
        })
        .finalize()
        .await
        .expect_err("测试终结器必须失败");
        let error: TrustedLuaExecutionHostingError<FakeError, FakeError> =
            TrustedLuaExecutionHostingError::Cleanup(cleanup);
        let report = error.into_failure_report(
            DiagnosticStage::WriteBack,
            Path::new("C:/project/scripts/write-back.lua"),
            DiagnosticImpact::Unchanged,
        );

        assert_eq!(
            report.primary.public().impact,
            DiagnosticImpact::OutcomeUnknown
        );
        assert_eq!(report.related.len(), 1);
        let serialized = format!(
            "{} {}",
            serde_json::to_string(report.primary.public()).expect("主诊断应可序列化"),
            serde_json::to_string(report.related[0].public()).expect("相关诊断应可序列化")
        );
        assert!(serialized.contains("\"primary_code\":10"));
        assert!(serialized.contains("\"extended_code\":1034"));
        assert!(serialized.contains("close_connection"));
        assert!(serialized.contains("permission_denied"));
        assert!(serialized.contains("lua_runtime_phase=finalization"));
        assert!(serialized.contains("C:/project/scripts/write-back.lua"));
        assert!(!serialized.contains("SQL_FINALIZATION_SENTINEL"));
        assert!(!serialized.contains("CLOSE_SOURCE_SENTINEL"));
    }

    #[test]
    fn runtime_and_cleanup_become_primary_and_related_safe_failures() {
        let error: TrustedLuaExecutionHostingError<FakeError, FakeError> =
            TrustedLuaExecutionHostingError::RuntimeAndCleanup {
                runtime: TrustedLuaRuntimeExecutionError::Execute(FakeError::Untyped(
                    "LUA_VM_BODY_SENTINEL",
                )),
                cleanup: TrustedLuaBindingFinalizationError::new(
                    "SQL_AND_PARAMETER_SENTINEL",
                    Some(Arc::new(FakeError::Untyped("CLEANUP_SOURCE_SENTINEL"))),
                ),
            };
        let report = error.into_failure_report(
            DiagnosticStage::Extract,
            Path::new("scripts/extract.lua"),
            DiagnosticImpact::Unchanged,
        );

        assert_eq!(report.related.len(), 1);
        assert_eq!(
            report.primary.public().impact,
            DiagnosticImpact::RecoveryRequired
        );
        let serialized = format!(
            "{} {}",
            serde_json::to_string(report.primary.public()).expect("主诊断应可序列化"),
            serde_json::to_string(report.related[0].public()).expect("相关诊断应可序列化")
        );
        assert!(serialized.contains("lua_execution_failed"));
        assert!(serialized.contains("lua_finalization_failed"));
        assert!(serialized.contains("scripts/extract.lua"));
        for sentinel in [
            "LUA_VM_BODY_SENTINEL",
            "SQL_AND_PARAMETER_SENTINEL",
            "CLEANUP_SOURCE_SENTINEL",
        ] {
            assert!(
                !serialized.contains(sentinel),
                "公开投影不应包含 {sentinel}"
            );
        }
    }

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
            record(&self.events, "source-read");
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

        async fn transaction_active(
            &self,
        ) -> Result<bool, SqliteInteractiveSessionError<Self::Error>> {
            Ok(false)
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
                        SqliteInteractiveSessionFinalizationFailure::CleanupFailed(
                            FakeError::Operation {
                                source_detail: "SESSION_CLOSE_SOURCE_SENTINEL",
                                operation: "close_connection",
                            },
                        ),
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
                Err(OpenSqliteInteractiveSessionError::OpenFailed(
                    FakeError::Untyped("open"),
                ))
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
                    RuntimeBehavior::Fail => Err(TrustedLuaRuntimeExecutionError::Execute(
                        FakeError::Untyped("vm"),
                    )),
                    RuntimeBehavior::Unavailable => {
                        Err(TrustedLuaRuntimeExecutionError::Unavailable(
                            FakeError::Untyped("unavailable"),
                        ))
                    }
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
            OwnedLuaProgram::new(PathBuf::from("main.lua"), b"return nil".to_vec()),
            LuaProjectContext::for_frozen_source(
                project.name().as_str(),
                crate::rpg_maker::RpgMakerEngine::Mz,
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

        assert_eq!(*events.lock().unwrap(), ["open", "start", "finalize"]);
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
                FakeError::Untyped("unavailable")
            ))
        ));
        assert_eq!(*events.lock().unwrap(), ["open", "start", "finalize"]);
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
        assert_eq!(*events.lock().unwrap(), ["open", "start", "finalize"]);
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
        assert_eq!(*events.lock().unwrap(), ["open"]);
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

        let report = error.into_failure_report(
            DiagnosticStage::Extract,
            Path::new("C:/project/scripts/extract.lua"),
            DiagnosticImpact::Unchanged,
        );

        assert_eq!(report.related.len(), 1);
        assert_eq!(
            report.primary.public().impact,
            DiagnosticImpact::RecoveryRequired
        );
        assert_eq!(
            report.related[0].public().subject,
            DiagnosticSubject::operation("close_connection")
        );
        let serialized = format!(
            "{} {}",
            serde_json::to_string(report.primary.public()).expect("主诊断应可序列化"),
            serde_json::to_string(report.related[0].public()).expect("相关诊断应可序列化")
        );
        assert!(serialized.contains("lua_execution_failed"));
        assert!(serialized.contains("lua_runtime_phase=execute"));
        assert!(serialized.contains("close_connection"));
        assert!(serialized.contains("lua_runtime_phase=finalization"));
        assert!(serialized.contains("C:/project/scripts/extract.lua"));
        for sentinel in ["vm", "SESSION_CLOSE_SOURCE_SENTINEL"] {
            assert!(
                !serialized.contains(sentinel),
                "公开投影不应包含 {sentinel}"
            );
        }
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

    #[derive(Clone)]
    struct SourceFileSystem {
        reads: Arc<Mutex<Vec<PathBuf>>>,
        lists: Arc<Mutex<Vec<PathBuf>>>,
        data_name: &'static str,
        items_name: &'static str,
    }

    impl Default for SourceFileSystem {
        fn default() -> Self {
            Self {
                reads: Arc::new(Mutex::new(Vec::new())),
                lists: Arc::new(Mutex::new(Vec::new())),
                data_name: "data",
                items_name: "Items.json",
            }
        }
    }

    impl FileReader for SourceFileSystem {
        type Error = FakeError;

        async fn read_file(&self, path: PathBuf) -> Result<ReadFile, ReadFileError<Self::Error>> {
            self.reads.lock().unwrap().push(path.clone());
            if path.file_name().and_then(|name| name.to_str()) == Some(self.items_name) {
                Ok(ReadFile::new(path, b"source".to_vec()))
            } else {
                Err(ReadFileError::NotFound { path })
            }
        }
    }

    impl DirectoryLister for SourceFileSystem {
        type Error = FakeError;

        async fn list_directory(
            &self,
            path: PathBuf,
        ) -> Result<Vec<DirectoryEntry>, ListDirectoryError<Self::Error>> {
            self.lists.lock().unwrap().push(path.clone());
            if path.ends_with("source") {
                Ok(vec![
                    DirectoryEntry::new(path.join(self.data_name), DirectoryEntryKind::Directory),
                    DirectoryEntry::new(path.join("js"), DirectoryEntryKind::Directory),
                ])
            } else if path.file_name().and_then(|name| name.to_str()) == Some(self.data_name) {
                Ok(vec![
                    DirectoryEntry::new(
                        path.join(self.items_name),
                        DirectoryEntryKind::RegularFile,
                    ),
                    DirectoryEntry::new(path.join("z.json"), DirectoryEntryKind::RegularFile),
                    DirectoryEntry::new(path.join("a.json"), DirectoryEntryKind::RegularFile),
                ])
            } else {
                Err(ListDirectoryError::NotFound { path })
            }
        }
    }

    fn source_calls(
        file_system: Arc<SourceFileSystem>,
    ) -> LuaCommonHostCalls<SourceFileSystem, FakeOperations> {
        let opened = OpenedProject::new(
            "demo".parse::<ProjectName>().unwrap(),
            PathBuf::from("C:/projects/demo"),
            PathBuf::from("C:/projects/demo/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::rpg_maker::project::test_layout_profile(),
        );
        LuaCommonHostCalls::<_, _> {
            project: LuaProjectContext::for_frozen_source(
                opened.name().as_str(),
                crate::rpg_maker::RpgMakerEngine::Mz,
                opened.source_root().to_path_buf(),
                opened.database_path().to_path_buf(),
                opened.language_pair().clone(),
            ),
            operations: Arc::new(FakeOperations),
            file_system,
        }
    }

    #[tokio::test]
    async fn source_calls_join_the_frozen_root_and_return_sorted_relative_paths() {
        let file_system = Arc::new(SourceFileSystem::default());
        let calls = source_calls(Arc::clone(&file_system));

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
            vec![
                "data/Items.json".to_owned(),
                "data/a.json".to_owned(),
                "data/z.json".to_owned(),
            ]
        );
        assert_eq!(
            *file_system.reads.lock().unwrap(),
            [PathBuf::from("C:/projects/demo/source/data/Items.json")]
        );
        assert_eq!(
            *file_system.lists.lock().unwrap(),
            [
                PathBuf::from("C:/projects/demo/source"),
                PathBuf::from("C:/projects/demo/source/data"),
                PathBuf::from("C:/projects/demo/source"),
                PathBuf::from("C:/projects/demo/source/data"),
            ]
        );
    }

    #[tokio::test]
    async fn source_calls_reject_case_aliases_before_reading() {
        let file_system = Arc::new(SourceFileSystem {
            items_name: "items.json",
            ..SourceFileSystem::default()
        });
        let calls = source_calls(Arc::clone(&file_system));

        let error = calls
            .read_source(LuaSourcePath::parse("data/Items.json").unwrap())
            .await
            .expect_err("大小写别名必须显式失败");
        assert_eq!(error.domain(), "filesystem");
        assert_eq!(error.kind(), "case_mismatch");
        assert!(file_system.reads.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn source_calls_check_intermediate_directories_and_list_targets_exactly() {
        let directory_alias = Arc::new(SourceFileSystem {
            data_name: "Data",
            ..SourceFileSystem::default()
        });
        let error = source_calls(Arc::clone(&directory_alias))
            .read_source(LuaSourcePath::parse("data/Items.json").unwrap())
            .await
            .expect_err("中间目录的大小写别名必须在读取前失败");
        assert_eq!(error.kind(), "case_mismatch");
        assert!(directory_alias.reads.lock().unwrap().is_empty());
        assert_eq!(directory_alias.lists.lock().unwrap().len(), 1);

        let file_alias = Arc::new(SourceFileSystem {
            items_name: "items.json",
            ..SourceFileSystem::default()
        });
        let error = source_calls(Arc::clone(&file_alias))
            .list_source(LuaSourcePath::parse("data/Items.json").unwrap())
            .await
            .expect_err("list_source 的最终目录项也必须逐字匹配");
        assert_eq!(error.kind(), "case_mismatch");
        assert_eq!(file_alias.lists.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn missing_sources_keep_the_underlying_read_and_list_error_contracts() {
        let file_system = Arc::new(SourceFileSystem::default());
        let calls = source_calls(Arc::clone(&file_system));

        let read_error = calls
            .read_source(LuaSourcePath::parse("data/Missing.json").unwrap())
            .await
            .expect_err("缺失文件应由读取能力报告");
        assert_eq!(read_error.kind(), "not_found");
        assert!(read_error.to_string().contains("文件不存在"));

        let list_error = calls
            .list_source(LuaSourcePath::parse("data/Missing").unwrap())
            .await
            .expect_err("缺失目录应由列举能力报告");
        assert_eq!(list_error.kind(), "not_found");
        assert!(list_error.to_string().contains("目录不存在"));
        assert_eq!(file_system.reads.lock().unwrap().len(), 1);
        assert_eq!(file_system.lists.lock().unwrap().len(), 5);
    }

    #[tokio::test]
    async fn windows_file_system_cannot_open_a_case_alias_through_lua_source() {
        let workspace = tempfile::tempdir().expect("应能建立临时工作区");
        let source_root = workspace.path().join("source");
        let data_root = source_root.join("data");
        fs::create_dir_all(&data_root).expect("应能建立真实 data 目录");
        fs::write(data_root.join("Items.json"), b"{}").expect("应能写入真实来源文件");

        let file_system = Arc::new(
            SystemFileSystem::new(SystemFileSystemConfig::production())
                .expect("应能建立真实文件系统根"),
        );
        let calls = LuaCommonHostCalls::<_, _> {
            project: LuaProjectContext::for_frozen_source(
                "demo",
                crate::rpg_maker::RpgMakerEngine::Mz,
                source_root,
                workspace.path().join("project.db"),
                crate::language::LanguagePair::new(
                    "ja".parse().expect("测试源语言应合法"),
                    "zh-Hans".parse().expect("测试目标语言应合法"),
                ),
            ),
            operations: Arc::new(FakeOperations),
            file_system: Arc::clone(&file_system),
        };

        let error = calls
            .read_source(LuaSourcePath::parse("data/items.json").unwrap())
            .await
            .expect_err("真实 Windows 文件系统上的大小写别名也必须失败");
        assert_eq!(error.domain(), "filesystem");
        assert_eq!(error.kind(), "case_mismatch");
        drop(calls);
        file_system
            .shutdown()
            .await
            .expect("文件系统 worker 应正常关闭");
    }

    struct TransactionObservationCalls {
        result: Result<bool, TrustedLuaHostCallError>,
    }

    impl TrustedLuaCommonHostCalls for TransactionObservationCalls {
        fn project(&self) -> &LuaProjectContext {
            panic!("事务观察测试不读取项目上下文")
        }

        fn read_source(
            &self,
            _path: LuaSourcePath,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, TrustedLuaHostCallError>> + Send + 'static>>
        {
            Box::pin(async { panic!("事务观察测试不读取来源") })
        }

        fn list_source(
            &self,
            _path: LuaSourcePath,
        ) -> Pin<
            Box<dyn Future<Output = Result<Vec<String>, TrustedLuaHostCallError>> + Send + 'static>,
        > {
            Box::pin(async { panic!("事务观察测试不列举来源") })
        }

        fn query(
            &self,
            _query: SqliteQuery,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<Vec<SqliteRow>, TrustedLuaHostCallError>>
                    + Send
                    + 'static,
            >,
        > {
            Box::pin(async { panic!("事务观察测试不执行查询") })
        }

        fn execute(
            &self,
            _command: SqliteCommand,
        ) -> Pin<Box<dyn Future<Output = Result<u64, TrustedLuaHostCallError>> + Send + 'static>>
        {
            Box::pin(async { panic!("事务观察测试不执行命令") })
        }

        fn begin(
            &self,
        ) -> Pin<Box<dyn Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>>
        {
            Box::pin(async { panic!("事务观察测试不开始事务") })
        }

        fn commit(
            &self,
        ) -> Pin<Box<dyn Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>>
        {
            Box::pin(async { panic!("事务观察测试不提交事务") })
        }

        fn rollback(
            &self,
        ) -> Pin<Box<dyn Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>>
        {
            Box::pin(async { panic!("事务观察测试不回滚事务") })
        }

        fn transaction_active(
            &self,
        ) -> Pin<Box<dyn Future<Output = Result<bool, TrustedLuaHostCallError>> + Send + 'static>>
        {
            let result = self.result.clone();
            Box::pin(async move { result })
        }
    }

    struct RecordingStandardSession {
        called: Arc<AtomicBool>,
    }

    impl TrustedLuaStandardSession for RecordingStandardSession {
        fn units(&self) -> Vec<super::super::runtime::TrustedLuaStandardUnit> {
            Vec::new()
        }

        fn get(
            &self,
            _owner: crate::rpg_maker::standard_asset::RpgMakerStandardAssetOwner,
            _group_location: crate::rpg_maker::text::RpgMakerLocation,
            _role: crate::rpg_maker::model::TextUnitRole,
        ) -> Option<super::super::runtime::TrustedLuaStandardUnit> {
            None
        }

        fn accept(
            &self,
            _candidates: Vec<TrustedLuaStandardCandidate>,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<Vec<TrustedLuaStandardAcceptance>, TrustedLuaHostCallError>,
                    > + Send
                    + 'static,
            >,
        > {
            self.called
                .store(true, std::sync::atomic::Ordering::Release);
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    #[tokio::test]
    async fn standard_accept_uses_authoritative_transaction_state() {
        let called = Arc::new(AtomicBool::new(false));
        let inner: Arc<dyn TrustedLuaStandardSession> = Arc::new(RecordingStandardSession {
            called: Arc::clone(&called),
        });
        let active: Arc<dyn TrustedLuaCommonHostCalls> =
            Arc::new(TransactionObservationCalls { result: Ok(true) });
        let session = TransactionAwareLuaStandardSession {
            inner: Arc::clone(&inner),
            transaction_state: active,
        };
        let error = session
            .accept(Vec::new())
            .await
            .expect_err("活动事务必须阻止 Standard 提交");
        assert_eq!(error.domain(), "standard");
        assert_eq!(error.kind(), "transaction_conflict");
        assert!(!called.load(std::sync::atomic::Ordering::Acquire));

        let indeterminate: Arc<dyn TrustedLuaCommonHostCalls> =
            Arc::new(TransactionObservationCalls {
                result: Err(TrustedLuaHostCallError::new(
                    "sqlite",
                    "indeterminate",
                    "事务终态未知",
                    None,
                    None,
                )),
            });
        let session = TransactionAwareLuaStandardSession {
            inner: Arc::clone(&inner),
            transaction_state: indeterminate,
        };
        let error = session
            .accept(Vec::new())
            .await
            .expect_err("终态未知必须保留 SQLite 失败");
        assert_eq!(error.domain(), "sqlite");
        assert_eq!(error.kind(), "indeterminate");
        assert!(!called.load(std::sync::atomic::Ordering::Acquire));

        let idle: Arc<dyn TrustedLuaCommonHostCalls> =
            Arc::new(TransactionObservationCalls { result: Ok(false) });
        let session = TransactionAwareLuaStandardSession {
            inner,
            transaction_state: idle,
        };
        assert!(session.accept(Vec::new()).await.is_ok());
        assert!(called.load(std::sync::atomic::Ordering::Acquire));
    }
}
