use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;

use crate::diagnostic::{
    DiagnosticAction, DiagnosticImpact, DiagnosticStage, FailureReport, RecoveryFact,
    SafeDiagnostic, SafeDiagnosticSource,
};
use crate::execution::OperationCompletion;
use crate::rpg_maker::lua::LuaProjectContext;
use crate::rpg_maker::lua::hosting::TrustedLuaExecutionHostingError;
use crate::rpg_maker::lua::runtime::{OwnedLuaProgram, TrustedLuaExtractIntent};
pub(crate) use crate::rpg_maker::lua::{
    LuaInvocation, TrustedLuaExecutionHost, TrustedLuaExecutionOutcome,
};
use crate::rpg_maker::project::OpenedProject;

use super::store::LuaSnapshotStore;
use super::{ExtractProgress, ExtractProgressPhase};

/// 执行一次可信 Lua 提取，并在 Host 干净结束后收敛其可选标准快照意图。
pub(crate) trait LuaExtraction: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn run(
        &self,
        project: &OpenedProject,
        program: OwnedLuaProgram,
        progress: ExtractProgress,
    ) -> impl Future<Output = Result<OperationCompletion<()>, Self::Error>> + Send;
}

/// 把 Extract 阶段已经建立的项目事实交给可信 Lua Host。
pub(crate) struct LuaExtractionService<H, S> {
    host: H,
    store: S,
}

impl<H, S> LuaExtractionService<H, S> {
    pub(crate) fn new(host: H, store: S) -> Self {
        Self { host, store }
    }
}

impl<H, S> LuaExtraction for LuaExtractionService<H, S>
where
    H: TrustedLuaExecutionHost,
    S: LuaSnapshotStore,
{
    type Error = LuaExtractionError<H::Error, S::Error>;

    async fn run(
        &self,
        project: &OpenedProject,
        program: OwnedLuaProgram,
        progress: ExtractProgress,
    ) -> Result<OperationCompletion<()>, Self::Error> {
        if program.source().is_empty() {
            progress.indeterminate(ExtractProgressPhase::LuaCommit);
            return self
                .store
                .deactivate_lua(project)
                .await
                .map(|_| OperationCompletion::Completed(()))
                .map_err(LuaExtractionError::StoreSnapshot);
        }
        let error_path = program.main_script_path().to_path_buf();
        let invocation = LuaInvocation::extract(
            program,
            LuaProjectContext::for_frozen_source(
                project.name().as_str(),
                project.layout().rpg_maker_layout().engine(),
                project.source_content_root(),
                project.database_path().to_path_buf(),
                project.language_pair().clone(),
            ),
        );

        progress.indeterminate(ExtractProgressPhase::LuaExecution);
        let completion = self.host.execute(invocation).await.map_err(|source| {
            LuaExtractionError::ExecuteHost {
                script_path: error_path,
                source,
            }
        })?;
        let OperationCompletion::Completed(outcome) = completion else {
            return Ok(OperationCompletion::Cancelled);
        };

        match outcome {
            TrustedLuaExecutionOutcome::Empty => Ok(OperationCompletion::Completed(())),
            TrustedLuaExecutionOutcome::ExtractIntent(TrustedLuaExtractIntent::Replace(
                snapshot,
            )) => {
                progress.indeterminate(ExtractProgressPhase::LuaCommit);
                self.store
                    .replace_lua(project, snapshot)
                    .await
                    .map(|_| OperationCompletion::Completed(()))
                    .map_err(LuaExtractionError::StoreSnapshot)
            }
            TrustedLuaExecutionOutcome::ExtractIntent(TrustedLuaExtractIntent::Deactivate) => {
                progress.indeterminate(ExtractProgressPhase::LuaCommit);
                self.store
                    .deactivate_lua(project)
                    .await
                    .map(|_| OperationCompletion::Completed(()))
                    .map_err(LuaExtractionError::StoreSnapshot)
            }
        }
    }
}

/// Lua Extract 阶段的 Host 执行失败。
#[derive(Debug)]
pub(crate) enum LuaExtractionError<E, S> {
    ExecuteHost { script_path: PathBuf, source: E },
    StoreSnapshot(S),
}

impl<O, R, S> LuaExtractionError<TrustedLuaExecutionHostingError<O, R>, S>
where
    O: SafeDiagnosticSource,
    R: SafeDiagnosticSource,
    S: SafeDiagnosticSource,
{
    /// Extract 适配器的唯一安全投影；顶层无需遍历 source 链或 downcast。
    pub(crate) fn safe_diagnostic(&self) -> SafeDiagnostic {
        match self {
            Self::ExecuteHost {
                script_path,
                source,
            } => source.safe_diagnostic(
                DiagnosticStage::Extract,
                script_path,
                DiagnosticImpact::Unchanged,
            ),
            Self::StoreSnapshot(source) => source
                .safe_diagnostic_source(
                    DiagnosticStage::Extract,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckProjectState,
                )
                .with_recovery(RecoveryFact::component("owner=lua")),
        }
    }

    /// 消费 Lua Extract 错误，并保留下层事务错误携带的全部相关诊断。
    pub(crate) fn into_failure_report(self) -> FailureReport
    where
        O: Error + Send + Sync + 'static,
        R: Error + Send + Sync + 'static,
        S: Error + Send + Sync + 'static,
    {
        match self {
            Self::ExecuteHost {
                script_path,
                source,
            } => source.into_failure_report(
                DiagnosticStage::Extract,
                &script_path,
                DiagnosticImpact::Unchanged,
            ),
            Self::StoreSnapshot(source) => source
                .into_failure_report(
                    DiagnosticStage::Extract,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckProjectState,
                )
                .with_primary_recovery(RecoveryFact::component("owner=lua")),
        }
    }
}

impl<E, S> fmt::Display for LuaExtractionError<E, S>
where
    E: Error,
    S: Error,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExecuteHost {
                script_path,
                source,
            } => write!(
                formatter,
                "执行可信 Lua 提取 Host 失败 {}：{source}",
                script_path.display()
            ),
            Self::StoreSnapshot(source) => write!(formatter, "保存 Lua 标准资产快照失败：{source}"),
        }
    }
}

impl<E, S> Error for LuaExtractionError<E, S>
where
    E: Error + 'static,
    S: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ExecuteHost { source, .. } => Some(source),
            Self::StoreSnapshot(source) => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::diagnostic::{
        DiagnosticCode, DiagnosticFailureKind, DiagnosticReason, DiagnosticSubject, ReportedFailure,
    };
    use crate::progress::{ProgressObserver, ProgressSnapshot};
    use crate::rpg_maker::ProjectName;
    use crate::rpg_maker::extract::store::LuaSnapshot;
    use crate::rpg_maker::lua::runtime::{
        TrustedLuaExtractIntent, TrustedLuaRuntimeExecutionError,
    };
    use crate::rpg_maker::lua::{LuaPhase, LuaProjectContext};

    #[derive(Clone, Default)]
    struct RecordingProgress(Arc<Mutex<Vec<ProgressSnapshot<ExtractProgressPhase>>>>);

    impl ProgressObserver<ExtractProgressPhase> for RecordingProgress {
        fn observe(&self, snapshot: ProgressSnapshot<ExtractProgressPhase>) {
            self.0.lock().expect("进度记录锁不应中毒").push(snapshot);
        }
    }

    impl RecordingProgress {
        fn snapshots(&self) -> Vec<ProgressSnapshot<ExtractProgressPhase>> {
            self.0.lock().expect("进度记录锁不应中毒").clone()
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct RecordedInvocation {
        phase: LuaPhase,
        script_path: PathBuf,
        project: LuaProjectContext,
    }

    #[derive(Clone)]
    struct FakeHost {
        invocation: Arc<Mutex<Option<RecordedInvocation>>>,
        fail: bool,
        cancelled: bool,
        outcome: TrustedLuaExecutionOutcome,
    }

    impl TrustedLuaExecutionHost for FakeHost {
        type TranslationClient = ();
        type Error = FakeError;

        async fn execute(
            &self,
            invocation: LuaInvocation<Self::TranslationClient>,
        ) -> Result<OperationCompletion<TrustedLuaExecutionOutcome>, Self::Error> {
            let recorded = match invocation {
                LuaInvocation::Extract { program, project } => RecordedInvocation {
                    phase: LuaPhase::Extract,
                    script_path: program.main_script_path().to_path_buf(),
                    project,
                },
                LuaInvocation::Translate { .. } => {
                    panic!("提取服务不应提交 Translate 调用")
                }
                LuaInvocation::WriteBack { .. } => {
                    panic!("提取服务不应提交 WriteBack 调用")
                }
                LuaInvocation::Project { .. } => {
                    panic!("提取服务不应提交独立项目 Lua 调用")
                }
            };
            *self.invocation.lock().expect("调用记录锁不应中毒") = Some(recorded);

            if self.fail {
                Err(FakeError)
            } else if self.cancelled {
                Ok(OperationCompletion::Cancelled)
            } else {
                Ok(OperationCompletion::Completed(self.outcome.clone()))
            }
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum StoreCall {
        Replace(usize),
        Deactivate,
    }

    #[derive(Clone, Default)]
    struct FakeStore {
        calls: Arc<Mutex<Vec<StoreCall>>>,
        fail: bool,
    }

    impl LuaSnapshotStore for FakeStore {
        type Error = FakeError;

        async fn replace_lua(
            &self,
            _project: &OpenedProject,
            snapshot: LuaSnapshot,
        ) -> Result<(), Self::Error> {
            if self.fail {
                return Err(FakeError);
            }
            self.calls
                .lock()
                .expect("Store 调用锁不应中毒")
                .push(StoreCall::Replace(snapshot.groups().len()));
            Ok(())
        }

        async fn deactivate_lua(&self, _project: &OpenedProject) -> Result<(), Self::Error> {
            if self.fail {
                return Err(FakeError);
            }
            self.calls
                .lock()
                .expect("Store 调用锁不应中毒")
                .push(StoreCall::Deactivate);
            Ok(())
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError;

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("host failed")
        }
    }

    impl Error for FakeError {}

    impl SafeDiagnosticSource for FakeError {
        fn safe_diagnostic_source(
            &self,
            stage: DiagnosticStage,
            impact: DiagnosticImpact,
            _action: DiagnosticAction,
        ) -> SafeDiagnostic {
            SafeDiagnostic::new(
                DiagnosticCode::LuaExecution,
                stage,
                DiagnosticSubject::component("fake Lua VM"),
                DiagnosticReason::failure(DiagnosticFailureKind::LuaCompilationFailed),
                impact,
                DiagnosticAction::FixInput,
            )
        }
    }

    const STORE_SOURCE_SENTINEL: &str =
        "store-source; prompt=ordinary; lua=return-ordinary; sql=DELETE-ordinary";

    #[derive(Clone, Debug)]
    struct TypedStoreError {
        database_path: PathBuf,
    }

    impl fmt::Display for TypedStoreError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(STORE_SOURCE_SENTINEL)
        }
    }

    impl Error for TypedStoreError {}

    impl SafeDiagnosticSource for TypedStoreError {
        fn safe_diagnostic_source(
            &self,
            stage: DiagnosticStage,
            _impact: DiagnosticImpact,
            _fallback_action: DiagnosticAction,
        ) -> SafeDiagnostic {
            SafeDiagnostic::new(
                DiagnosticCode::SqliteOperation,
                stage,
                DiagnosticSubject::path(&self.database_path),
                DiagnosticReason::Sqlite {
                    primary_code: 5,
                    extended_code: 517,
                },
                DiagnosticImpact::OutcomeUnknown,
                DiagnosticAction::PreserveRecoveryArtifacts,
            )
            .with_recovery(RecoveryFact::transaction("outcome_unknown"))
        }

        fn into_failure_report(
            self,
            stage: DiagnosticStage,
            impact: DiagnosticImpact,
            fallback_action: DiagnosticAction,
        ) -> FailureReport {
            let primary = self.safe_diagnostic_source(stage, impact, fallback_action);
            let related_source = TypedClaimConflictEvidence {
                database_path: self.database_path.clone(),
            };
            let related = SafeDiagnostic::new(
                DiagnosticCode::ProjectState,
                stage,
                DiagnosticSubject::path(&related_source.database_path),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::ConflictingValues,
                    "mutation_resource=data/Actors.json",
                ),
                DiagnosticImpact::OutcomeUnknown,
                DiagnosticAction::PreserveRecoveryArtifacts,
            )
            .with_recovery(RecoveryFact::component("incoming_owner=lua"))
            .with_recovery(RecoveryFact::component("current_owner=builtin"))
            .with_recovery(RecoveryFact::transaction("outcome_unknown"));
            FailureReport::new(ReportedFailure::new(primary, self))
                .with_related(ReportedFailure::new(related, related_source))
        }
    }

    #[derive(Debug)]
    struct TypedClaimConflictEvidence {
        database_path: PathBuf,
    }

    impl fmt::Display for TypedClaimConflictEvidence {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(STORE_SOURCE_SENTINEL)
        }
    }

    impl Error for TypedClaimConflictEvidence {}

    fn opened_project() -> OpenedProject {
        OpenedProject::new(
            "alice".parse::<ProjectName>().expect("项目名应合法"),
            PathBuf::from("C:/projects/alice"),
            PathBuf::from("C:/projects/alice/project.db"),
            "ja".to_owned(),
            "zh-CN".to_owned(),
            crate::rpg_maker::project::test_layout_profile(),
        )
    }

    fn program(path: &str) -> OwnedLuaProgram {
        OwnedLuaProgram::new(PathBuf::from(path), b"return nil".to_vec())
    }

    #[tokio::test]
    async fn passes_complete_extract_context_to_host_once() {
        let recorded = Arc::new(Mutex::new(None));
        let store = FakeStore::default();
        let calls = Arc::clone(&store.calls);
        let service = LuaExtractionService::new(
            FakeHost {
                invocation: Arc::clone(&recorded),
                fail: false,
                cancelled: false,
                outcome: TrustedLuaExecutionOutcome::Empty,
            },
            store,
        );

        service
            .run(
                &opened_project(),
                program("scripts/extract.lua"),
                ExtractProgress::default(),
            )
            .await
            .expect("Lua 提取应该成功");

        let invocation = recorded
            .lock()
            .expect("调用记录锁不应中毒")
            .clone()
            .expect("Host 应该收到一次调用");
        assert_eq!(invocation.phase, LuaPhase::Extract);
        assert_eq!(invocation.script_path, PathBuf::from("scripts/extract.lua"));
        assert_eq!(invocation.project.name(), "alice");
        assert_eq!(
            invocation.project.source_root(),
            Path::new("C:/projects/alice/source")
        );
        assert_eq!(
            invocation.project.database_path(),
            Path::new("C:/projects/alice/project.db")
        );
        assert_eq!(invocation.project.source_language().as_str(), "ja");
        assert_eq!(invocation.project.target_language().as_str(), "zh-CN");
        assert!(calls.lock().expect("Store 调用锁不应中毒").is_empty());
    }

    #[tokio::test]
    async fn preserves_script_path_and_host_source() {
        let service = LuaExtractionService::new(
            FakeHost {
                invocation: Arc::new(Mutex::new(None)),
                fail: true,
                cancelled: false,
                outcome: TrustedLuaExecutionOutcome::ExtractIntent(
                    TrustedLuaExtractIntent::Deactivate,
                ),
            },
            FakeStore::default(),
        );

        let error = service
            .run(
                &opened_project(),
                program("broken extract.lua"),
                ExtractProgress::default(),
            )
            .await
            .expect_err("Host 失败应该传播");

        assert!(matches!(
            &error,
            LuaExtractionError::ExecuteHost {
                script_path,
                source: FakeError
            } if script_path == &PathBuf::from("broken extract.lua")
        ));
        assert!(error.to_string().contains("broken extract.lua"));
    }

    #[test]
    fn typed_projection_keeps_lua_stage_and_path_without_vm_text() {
        let error: LuaExtractionError<
            TrustedLuaExecutionHostingError<FakeError, FakeError>,
            FakeError,
        > = LuaExtractionError::ExecuteHost {
            script_path: PathBuf::from("scripts/broken.lua"),
            source: TrustedLuaExecutionHostingError::Runtime(
                TrustedLuaRuntimeExecutionError::Compile(FakeError),
            ),
        };
        let diagnostic = error.safe_diagnostic();
        let serialized = serde_json::to_string(&diagnostic).expect("诊断应可序列化");

        assert!(!serialized.contains("host failed"));
        assert!(serialized.contains("lua.execution"));
        assert!(serialized.contains("scripts/broken.lua"));
        assert!(serialized.contains("lua_compilation_failed"));
        assert_eq!(diagnostic.stage, DiagnosticStage::Extract);
        assert_eq!(
            diagnostic.reason.render(),
            "the Lua main program could not be compiled"
        );
    }

    #[test]
    fn store_snapshot_forwards_typed_report_without_copying_untyped_source_text() {
        type Error = LuaExtractionError<
            TrustedLuaExecutionHostingError<FakeError, FakeError>,
            TypedStoreError,
        >;
        let database_path = PathBuf::from(r"C:\projects\alice\project.db");

        let diagnostic = Error::StoreSnapshot(TypedStoreError {
            database_path: database_path.clone(),
        })
        .safe_diagnostic();
        let serialized = serde_json::to_string(&diagnostic).expect("诊断应可序列化");
        assert!(serialized.contains("sqlite.operation"));
        assert!(serialized.contains("project.db"));
        assert!(serialized.contains("\"primary_code\":5"));
        assert!(serialized.contains("\"extended_code\":517"));
        assert!(serialized.contains("\"stage\":\"extract\""));
        assert!(serialized.contains("outcome_unknown"));
        assert!(serialized.contains("owner=lua"));
        assert!(!serialized.contains(STORE_SOURCE_SENTINEL));

        let report = Error::StoreSnapshot(TypedStoreError { database_path }).into_failure_report();
        let public = report
            .public_diagnostics()
            .map(|diagnostic| serde_json::to_string(diagnostic).expect("诊断应可序列化"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(report.public_diagnostics().count(), 2);
        assert!(public.contains("mutation_resource=data/Actors.json"));
        assert!(public.contains("incoming_owner=lua"));
        assert!(public.contains("current_owner=builtin"));
        assert!(public.contains("owner=lua"));
        assert!(public.contains("outcome_unknown"));
        assert!(!public.contains(STORE_SOURCE_SENTINEL));
    }

    #[tokio::test]
    async fn commits_exactly_the_extract_intent_returned_by_clean_host() {
        let store = FakeStore::default();
        let calls = Arc::clone(&store.calls);
        let progress = RecordingProgress::default();
        let service = LuaExtractionService::new(
            FakeHost {
                invocation: Arc::new(Mutex::new(None)),
                fail: false,
                cancelled: false,
                outcome: TrustedLuaExecutionOutcome::ExtractIntent(
                    TrustedLuaExtractIntent::Replace(LuaSnapshot::empty()),
                ),
            },
            store,
        );

        service
            .run(
                &opened_project(),
                program("extract.lua"),
                ExtractProgress::new(progress.clone()),
            )
            .await
            .expect("Host 已确认的 active 空快照应该提交");

        assert_eq!(
            calls.lock().expect("Store 调用锁不应中毒").as_slice(),
            &[StoreCall::Replace(0)]
        );
        assert_eq!(
            progress.snapshots(),
            [
                ProgressSnapshot::indeterminate(ExtractProgressPhase::LuaExecution),
                ProgressSnapshot::indeterminate(ExtractProgressPhase::LuaCommit),
            ]
        );
    }

    #[tokio::test]
    async fn cancellation_is_a_normal_result_and_never_commits_the_extract_intent() {
        let store = FakeStore::default();
        let calls = Arc::clone(&store.calls);
        let service = LuaExtractionService::new(
            FakeHost {
                invocation: Arc::new(Mutex::new(None)),
                fail: false,
                cancelled: true,
                outcome: TrustedLuaExecutionOutcome::ExtractIntent(
                    TrustedLuaExtractIntent::Deactivate,
                ),
            },
            store,
        );

        let completion = service
            .run(
                &opened_project(),
                program("extract.lua"),
                ExtractProgress::default(),
            )
            .await
            .expect("Lua 取消应是正常结果");

        assert_eq!(completion, OperationCompletion::Cancelled);
        assert!(calls.lock().expect("Store 调用锁不应中毒").is_empty());
    }

    #[tokio::test]
    async fn reports_store_failure_after_host_success() {
        let service = LuaExtractionService::new(
            FakeHost {
                invocation: Arc::new(Mutex::new(None)),
                fail: false,
                cancelled: false,
                outcome: TrustedLuaExecutionOutcome::ExtractIntent(
                    TrustedLuaExtractIntent::Deactivate,
                ),
            },
            FakeStore {
                calls: Arc::new(Mutex::new(Vec::new())),
                fail: true,
            },
        );

        let error = service
            .run(
                &opened_project(),
                program("extract.lua"),
                ExtractProgress::default(),
            )
            .await
            .expect_err("Store 失败必须传播");
        assert!(matches!(
            error,
            LuaExtractionError::StoreSnapshot(FakeError)
        ));
    }

    #[test]
    fn execution_future_is_send() {
        fn assert_send<T: Send>(_: T) {}

        let service = LuaExtractionService::new(
            FakeHost {
                invocation: Arc::new(Mutex::new(None)),
                fail: false,
                cancelled: false,
                outcome: TrustedLuaExecutionOutcome::Empty,
            },
            FakeStore::default(),
        );
        let project = opened_project();
        assert_send(service.run(&project, program("extract.lua"), ExtractProgress::default()));
    }
}
