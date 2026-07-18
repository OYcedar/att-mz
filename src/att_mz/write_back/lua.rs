//! 把尚未发布的完整写回候选交给共享可信 Lua Host。

use std::error::Error;
use std::fmt;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use crate::att_mz::lua::runtime::{
    TrustedLuaHostCallError, TrustedLuaOutputEntry, TrustedLuaOutputEntryKind,
    TrustedLuaWriteBackHostCalls, TrustedLuaWriteBackLayoutPair, TrustedLuaWriteBackLayoutRegion,
    TrustedLuaWriteBackLayoutResult, TrustedLuaWriteBackLayoutStatus,
};
use crate::att_mz::lua::{
    LuaInvocation, LuaProjectContext, TrustedLuaExecutionHost, TrustedLuaExecutionOutcome,
};
use crate::att_mz::project::MzWriteBackLayoutProfile;
use crate::att_mz::project::OpenedProject;
use crate::execution::OperationCompletion;
use crate::storage::file_system::{
    BoundScopedDirectory, ScopedDirectoryBindError, ScopedDirectoryEditError,
    ScopedDirectoryEditor, ScopedDirectoryEntryKind, ScopedDirectoryPath, StagedDirectory,
};

use super::standard::{MzLayoutTextPair, MzTextLayoutOutcome, MzWriteBackLayoutRegion};
use super::{LuaWriteBack, PreparedWriteBackCandidate};

/// 允许 Lua scope 绑定到候选、但不交出 Publisher 终结权的窄交接面。
pub(crate) trait ScopedPreparedWriteBackCandidate<S>: PreparedWriteBackCandidate
where
    S: Send + 'static,
{
    fn staged_directory(&self) -> &StagedDirectory<S>;
}

/// 在完整候选上运行可信 Lua 写回程序。
pub(crate) struct LuaWriteBackService<H, E> {
    host: H,
    editor: Arc<E>,
}

impl<H, E> LuaWriteBackService<H, E> {
    pub(crate) fn new(host: H, editor: E) -> Self {
        Self {
            host,
            editor: Arc::new(editor),
        }
    }
}

impl<H, E, C> LuaWriteBack<C> for LuaWriteBackService<H, E>
where
    H: TrustedLuaExecutionHost,
    E: ScopedDirectoryEditor + 'static,
    C: ScopedPreparedWriteBackCandidate<E::CandidateState>,
{
    type Error = LuaWriteBackServiceError<H::Error, E::Error>;

    fn run(
        &self,
        project: &OpenedProject,
        candidate: &C,
        script_path: PathBuf,
    ) -> impl std::future::Future<Output = Result<OperationCompletion<()>, Self::Error>> + Send
    {
        let prepared = if !candidate.belongs_to(project) {
            Err(LuaWriteBackServiceError::CandidateProjectMismatch {
                project_root: project.workspace_root().to_path_buf(),
                candidate_root: candidate.candidate_root().to_path_buf(),
            })
        } else {
            let error_path = script_path.clone();
            let candidate_root = candidate.candidate_root().to_path_buf();
            let bind = self
                .editor
                .bind_scoped_directory(candidate.staged_directory(), super::mz_output_scope());
            let editor = Arc::clone(&self.editor);
            let layout_profile = *project.layout_profile();
            Ok((
                script_path,
                LuaProjectContext::for_write_back_candidate(project, candidate_root.clone()),
                error_path,
                candidate_root,
                bind,
                editor,
                layout_profile,
            ))
        };

        async move {
            let (script_path, project, error_path, candidate_root, bind, editor, layout_profile) =
                prepared?;
            let scope = bind
                .await
                .map_err(|source| LuaWriteBackServiceError::BindCandidate {
                    candidate_root: candidate_root.clone(),
                    source,
                })?;
            let scope = Arc::new(scope);
            let calls: Arc<dyn TrustedLuaWriteBackHostCalls> =
                Arc::new(ScopedLuaWriteBackHostCalls {
                    editor,
                    scope: Arc::clone(&scope),
                    layout_profile,
                });
            let invocation = LuaInvocation::write_back(script_path, project, calls);
            match self.host.execute(invocation).await {
                Ok(OperationCompletion::Completed(TrustedLuaExecutionOutcome::Empty)) => {
                    Ok(OperationCompletion::Completed(()))
                }
                Ok(OperationCompletion::Cancelled) => Ok(OperationCompletion::Cancelled),
                Ok(OperationCompletion::Completed(TrustedLuaExecutionOutcome::ExtractIntent(
                    _,
                ))) => Err(LuaWriteBackServiceError::UnexpectedOutcome {
                    script_path: error_path,
                    candidate_root,
                }),
                Err(source) => Err(LuaWriteBackServiceError::ExecuteHost {
                    script_path: error_path,
                    candidate_root,
                    source,
                }),
            }
        }
    }
}

struct ScopedLuaWriteBackHostCalls<E>
where
    E: ScopedDirectoryEditor,
{
    editor: Arc<E>,
    scope: Arc<BoundScopedDirectory<E::ScopeState>>,
    layout_profile: MzWriteBackLayoutProfile,
}

impl<E> TrustedLuaWriteBackHostCalls for ScopedLuaWriteBackHostCalls<E>
where
    E: ScopedDirectoryEditor + 'static,
{
    fn read_output(
        &self,
        path: ScopedDirectoryPath,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<Vec<u8>, TrustedLuaHostCallError>>
                + Send
                + 'static,
        >,
    > {
        let editor = Arc::clone(&self.editor);
        let scope = Arc::clone(&self.scope);
        Box::pin(async move {
            editor
                .read_scoped_file(&scope, path)
                .await
                .map_err(output_edit_error)
        })
    }

    fn list_output(
        &self,
        path: ScopedDirectoryPath,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<Vec<TrustedLuaOutputEntry>, TrustedLuaHostCallError>,
                > + Send
                + 'static,
        >,
    > {
        let editor = Arc::clone(&self.editor);
        let scope = Arc::clone(&self.scope);
        Box::pin(async move {
            editor
                .list_scoped_directory(&scope, path)
                .await
                .map_err(output_edit_error)?
                .into_iter()
                .map(|entry| {
                    let name = entry.name().to_str().ok_or_else(|| {
                        TrustedLuaHostCallError::new(
                            "output",
                            "invalid_utf8_name",
                            "候选目录项名称无法无损转换为 UTF-8",
                            None,
                            None,
                        )
                    })?;
                    let kind = match entry.kind() {
                        ScopedDirectoryEntryKind::File => TrustedLuaOutputEntryKind::File,
                        ScopedDirectoryEntryKind::Directory => TrustedLuaOutputEntryKind::Directory,
                    };
                    Ok(TrustedLuaOutputEntry::new(name.to_owned(), kind))
                })
                .collect()
        })
    }

    fn create_output_directory(
        &self,
        path: ScopedDirectoryPath,
    ) -> Pin<
        Box<dyn std::future::Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>,
    > {
        let editor = Arc::clone(&self.editor);
        let scope = Arc::clone(&self.scope);
        Box::pin(async move {
            editor
                .create_scoped_directory(&scope, path)
                .await
                .map_err(output_edit_error)
        })
    }

    fn write_output(
        &self,
        path: ScopedDirectoryPath,
        bytes: Vec<u8>,
    ) -> Pin<
        Box<dyn std::future::Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>,
    > {
        let editor = Arc::clone(&self.editor);
        let scope = Arc::clone(&self.scope);
        Box::pin(async move {
            editor
                .write_scoped_file(&scope, path, bytes)
                .await
                .map_err(output_edit_error)
        })
    }

    fn remove_output(
        &self,
        path: ScopedDirectoryPath,
    ) -> Pin<
        Box<dyn std::future::Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>,
    > {
        let editor = Arc::clone(&self.editor);
        let scope = Arc::clone(&self.scope);
        Box::pin(async move {
            editor
                .remove_scoped_path(&scope, path)
                .await
                .map_err(output_edit_error)
        })
    }

    fn layout(
        &self,
        region: TrustedLuaWriteBackLayoutRegion,
        pairs: Vec<TrustedLuaWriteBackLayoutPair>,
    ) -> Result<TrustedLuaWriteBackLayoutResult, TrustedLuaHostCallError> {
        let region = match region {
            TrustedLuaWriteBackLayoutRegion::DialogueBody => MzWriteBackLayoutRegion::DialogueBody,
            TrustedLuaWriteBackLayoutRegion::ScrollingText => {
                MzWriteBackLayoutRegion::ScrollingText
            }
            TrustedLuaWriteBackLayoutRegion::HelpDescription => {
                MzWriteBackLayoutRegion::HelpDescription
            }
        };
        let pairs = pairs
            .into_iter()
            .map(|pair| {
                MzLayoutTextPair::new(
                    pair.original().to_owned(),
                    pair.translation().map(str::to_owned),
                )
            })
            .collect::<Vec<_>>();
        let (status, applied) =
            match super::standard::layout::layout(region, &pairs, &self.layout_profile) {
                MzTextLayoutOutcome::Applied(applied) => {
                    (TrustedLuaWriteBackLayoutStatus::Applied, applied)
                }
                MzTextLayoutOutcome::Manual(manual) => {
                    (TrustedLuaWriteBackLayoutStatus::Manual, manual)
                }
            };
        let (texts, inserted_line_breaks, inserted_fullwidth_indents) = applied.into_parts();
        Ok(TrustedLuaWriteBackLayoutResult::new(
            status,
            texts,
            inserted_line_breaks,
            inserted_fullwidth_indents,
        ))
    }
}

fn output_edit_error<E>(error: ScopedDirectoryEditError<E>) -> TrustedLuaHostCallError
where
    E: Error + Send + Sync + 'static,
{
    let kind = match &error {
        ScopedDirectoryEditError::WrongEditorInstance => "wrong_editor_instance",
        ScopedDirectoryEditError::OutsideScope { .. } => "outside_scope",
        ScopedDirectoryEditError::ScopeRootMutation { .. } => "scope_root_mutation",
        ScopedDirectoryEditError::NotFound { .. } => "not_found",
        ScopedDirectoryEditError::NotFile { .. } => "not_file",
        ScopedDirectoryEditError::NotDirectory { .. } => "not_directory",
        ScopedDirectoryEditError::DirectoryNotEmpty { .. } => "directory_not_empty",
        ScopedDirectoryEditError::CandidateIdentityChanged { .. } => "candidate_identity_changed",
        ScopedDirectoryEditError::Failed { .. } => "io",
    };
    let message = error.to_string();
    TrustedLuaHostCallError::new("output", kind, message, None, Some(Arc::new(error)))
}

/// Lua WriteBack 在项目交接或 Host 执行边界遇到的失败。
#[derive(Debug)]
pub(crate) enum LuaWriteBackServiceError<H, E> {
    CandidateProjectMismatch {
        project_root: PathBuf,
        candidate_root: PathBuf,
    },
    ExecuteHost {
        script_path: PathBuf,
        candidate_root: PathBuf,
        source: H,
    },
    BindCandidate {
        candidate_root: PathBuf,
        source: ScopedDirectoryBindError<E>,
    },
    UnexpectedOutcome {
        script_path: PathBuf,
        candidate_root: PathBuf,
    },
}

impl<H, E> fmt::Display for LuaWriteBackServiceError<H, E>
where
    H: fmt::Display,
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CandidateProjectMismatch {
                project_root,
                candidate_root,
            } => write!(
                formatter,
                "写回候选不属于当前项目（当前：{}，候选：{}）",
                project_root.display(),
                candidate_root.display()
            ),
            Self::ExecuteHost {
                script_path,
                candidate_root,
                source,
            } => write!(
                formatter,
                "执行可信 Lua 写回 Host 失败（脚本：{}，候选：{}）：{source}",
                script_path.display(),
                candidate_root.display()
            ),
            Self::BindCandidate {
                candidate_root,
                source,
            } => write!(
                formatter,
                "无法把 Lua 写回能力绑定到候选 {}：{source}",
                candidate_root.display()
            ),
            Self::UnexpectedOutcome {
                script_path,
                candidate_root,
            } => write!(
                formatter,
                "Lua 写回 Host 返回了其他阶段的结果（脚本：{}，候选：{}）",
                script_path.display(),
                candidate_root.display()
            ),
        }
    }
}

impl<H, E> Error for LuaWriteBackServiceError<H, E>
where
    H: Error + 'static,
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CandidateProjectMismatch { .. } | Self::UnexpectedOutcome { .. } => None,
            Self::ExecuteHost { source, .. } => Some(source),
            Self::BindCandidate { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::att_mz::ProjectName;
    use crate::att_mz::lua::LuaPhase;
    use crate::att_mz::project::MaxFullwidthChars;

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
        unexpected_outcome: bool,
    }

    impl TrustedLuaExecutionHost for FakeHost {
        type TranslationProfile = ();
        type Error = FakeError;

        async fn execute(
            &self,
            invocation: LuaInvocation<Self::TranslationProfile>,
        ) -> Result<OperationCompletion<TrustedLuaExecutionOutcome>, Self::Error> {
            let LuaInvocation::WriteBack {
                script_path,
                project,
                calls: _,
            } = invocation
            else {
                panic!("Lua 写回服务只应提交 WriteBack 调用")
            };
            *self.invocation.lock().expect("调用记录锁不应中毒") = Some(RecordedInvocation {
                phase: LuaPhase::WriteBack,
                script_path,
                project,
            });
            if self.fail {
                Err(FakeError)
            } else if self.cancelled {
                Ok(OperationCompletion::Cancelled)
            } else if self.unexpected_outcome {
                Ok(OperationCompletion::Completed(
                    TrustedLuaExecutionOutcome::ExtractIntent(
                        crate::att_mz::lua::runtime::TrustedLuaExtractIntent::Deactivate,
                    ),
                ))
            } else {
                Ok(OperationCompletion::Completed(
                    TrustedLuaExecutionOutcome::Empty,
                ))
            }
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

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeEditorError;

    impl fmt::Display for FakeEditorError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("editor failed")
        }
    }

    impl Error for FakeEditorError {}

    #[derive(Clone, Default)]
    struct FakeEditor {
        bind_fail: bool,
    }

    impl ScopedDirectoryEditor for FakeEditor {
        type CandidateState = ();
        type ScopeState = ();
        type Error = FakeEditorError;

        fn bind_scoped_directory(
            &self,
            candidate: &StagedDirectory<Self::CandidateState>,
            scope: crate::storage::file_system::ScopedDirectoryScope,
        ) -> impl std::future::Future<
            Output = Result<
                BoundScopedDirectory<Self::ScopeState>,
                ScopedDirectoryBindError<Self::Error>,
            >,
        > + Send
        + use<> {
            let root = candidate.staging_root().to_path_buf();
            std::future::ready(if self.bind_fail {
                Err(ScopedDirectoryBindError::CandidateIdentityChanged { root })
            } else {
                Ok(BoundScopedDirectory::new(root, scope, ()))
            })
        }

        fn validate_scoped_directory(
            &self,
            _scope: &BoundScopedDirectory<Self::ScopeState>,
        ) -> impl std::future::Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>>
        + Send
        + use<> {
            std::future::ready(Ok(()))
        }

        fn read_scoped_file(
            &self,
            _scope: &BoundScopedDirectory<Self::ScopeState>,
            _path: ScopedDirectoryPath,
        ) -> impl std::future::Future<
            Output = Result<Vec<u8>, ScopedDirectoryEditError<Self::Error>>,
        > + Send {
            std::future::ready(Ok(Vec::new()))
        }

        fn list_scoped_directory(
            &self,
            _scope: &BoundScopedDirectory<Self::ScopeState>,
            _path: ScopedDirectoryPath,
        ) -> impl std::future::Future<
            Output = Result<
                Vec<crate::storage::file_system::ScopedDirectoryEntry>,
                ScopedDirectoryEditError<Self::Error>,
            >,
        > + Send {
            std::future::ready(Ok(Vec::new()))
        }

        fn list_scoped_root(
            &self,
            _scope: &BoundScopedDirectory<Self::ScopeState>,
        ) -> impl std::future::Future<
            Output = Result<
                Vec<crate::storage::file_system::ScopedDirectoryEntry>,
                ScopedDirectoryEditError<Self::Error>,
            >,
        > + Send {
            std::future::ready(Ok(Vec::new()))
        }

        fn create_scoped_directory(
            &self,
            _scope: &BoundScopedDirectory<Self::ScopeState>,
            _path: ScopedDirectoryPath,
        ) -> impl std::future::Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send
        {
            std::future::ready(Ok(()))
        }

        fn write_scoped_file(
            &self,
            _scope: &BoundScopedDirectory<Self::ScopeState>,
            _path: ScopedDirectoryPath,
            _bytes: Vec<u8>,
        ) -> impl std::future::Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send
        {
            std::future::ready(Ok(()))
        }

        fn remove_scoped_path(
            &self,
            _scope: &BoundScopedDirectory<Self::ScopeState>,
            _path: ScopedDirectoryPath,
        ) -> impl std::future::Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send
        {
            std::future::ready(Ok(()))
        }
    }

    struct FakeCandidate {
        project_name: ProjectName,
        workspace_root: PathBuf,
        output_root: PathBuf,
        staged: StagedDirectory<()>,
    }

    impl PreparedWriteBackCandidate for FakeCandidate {
        fn belongs_to(&self, project: &OpenedProject) -> bool {
            self.project_name == *project.name()
                && self.workspace_root == project.workspace_root()
                && self.output_root == project.write_back_root()
        }

        fn candidate_root(&self) -> &Path {
            self.staged.staging_root()
        }
    }

    impl ScopedPreparedWriteBackCandidate<()> for FakeCandidate {
        fn staged_directory(&self) -> &StagedDirectory<()> {
            &self.staged
        }
    }

    fn project(name: &str) -> OpenedProject {
        OpenedProject::new(
            name.parse::<ProjectName>().expect("项目名应合法"),
            PathBuf::from("C:/projects").join(name),
            PathBuf::from("C:/projects").join(name).join("project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::att_mz::project::test_layout_profile(),
        )
    }

    fn candidate(project: &OpenedProject) -> FakeCandidate {
        FakeCandidate {
            project_name: project.name().clone(),
            workspace_root: project.workspace_root().to_path_buf(),
            output_root: project.write_back_root().to_path_buf(),
            staged: StagedDirectory::new(
                project.write_back_root().to_path_buf(),
                project.workspace_root().join(".write_back-stage"),
                crate::storage::file_system::DirectoryPublishIntent::ReplaceExisting,
                (),
            ),
        }
    }

    #[tokio::test]
    async fn passes_write_back_phase_and_only_this_phase_receives_output_root() {
        let recorded = Arc::new(Mutex::new(None));
        let service = LuaWriteBackService::new(
            FakeHost {
                invocation: Arc::clone(&recorded),
                fail: false,
                cancelled: false,
                unexpected_outcome: false,
            },
            FakeEditor::default(),
        );
        let project = project("alice");
        let candidate = candidate(&project);

        service
            .run(&project, &candidate, PathBuf::from("scripts/write.lua"))
            .await
            .expect("Lua 写回应该成功");

        let invocation = recorded
            .lock()
            .expect("调用记录锁不应中毒")
            .clone()
            .expect("Host 应收到一次调用");
        assert_eq!(invocation.phase, LuaPhase::WriteBack);
        assert_eq!(invocation.script_path, PathBuf::from("scripts/write.lua"));
        assert_eq!(
            invocation.project.source_root(),
            Path::new("C:/projects/alice/source")
        );
        assert_eq!(
            invocation.project.output_root(),
            Some(Path::new("C:/projects/alice/.write_back-stage"))
        );
        assert_eq!(
            invocation.project.database_path(),
            Path::new("C:/projects/alice/project.db")
        );
    }

    #[tokio::test]
    async fn cancellation_is_propagated_as_a_normal_write_back_result() {
        let service = LuaWriteBackService::new(
            FakeHost {
                invocation: Arc::new(Mutex::new(None)),
                fail: false,
                cancelled: true,
                unexpected_outcome: false,
            },
            FakeEditor::default(),
        );
        let project = project("alice");

        let completion = service
            .run(&project, &candidate(&project), PathBuf::from("write.lua"))
            .await
            .expect("Lua 取消应是正常结果");

        assert_eq!(completion, OperationCompletion::Cancelled);
    }

    #[tokio::test]
    async fn rejects_a_candidate_from_another_project_before_host() {
        let recorded = Arc::new(Mutex::new(None));
        let service = LuaWriteBackService::new(
            FakeHost {
                invocation: Arc::clone(&recorded),
                fail: false,
                cancelled: false,
                unexpected_outcome: false,
            },
            FakeEditor::default(),
        );
        let current_project = project("alice");
        let other = project("bob");

        let error = service
            .run(
                &current_project,
                &candidate(&other),
                PathBuf::from("write.lua"),
            )
            .await
            .expect_err("跨项目候选 token 必须拒绝");

        assert!(matches!(
            error,
            LuaWriteBackServiceError::CandidateProjectMismatch { .. }
        ));
        assert!(recorded.lock().expect("调用记录锁不应中毒").is_none());
    }

    #[tokio::test]
    async fn preserves_script_output_and_host_source() {
        let service = LuaWriteBackService::new(
            FakeHost {
                invocation: Arc::new(Mutex::new(None)),
                fail: true,
                cancelled: false,
                unexpected_outcome: false,
            },
            FakeEditor::default(),
        );
        let project = project("alice");
        let error = service
            .run(
                &project,
                &candidate(&project),
                PathBuf::from("broken write.lua"),
            )
            .await
            .expect_err("Host 失败应该传播");

        assert!(matches!(
            &error,
            LuaWriteBackServiceError::ExecuteHost {
                script_path,
                candidate_root,
                source: FakeError,
            } if script_path == &PathBuf::from("broken write.lua")
                && candidate_root == &PathBuf::from("C:/projects/alice/.write_back-stage")
        ));
        assert_eq!(
            error.source().and_then(|source| source.downcast_ref()),
            Some(&FakeError)
        );
    }

    #[test]
    fn execution_future_is_send() {
        fn assert_send<T: Send>(_: T) {}

        let service = LuaWriteBackService::new(
            FakeHost {
                invocation: Arc::new(Mutex::new(None)),
                fail: false,
                cancelled: false,
                unexpected_outcome: false,
            },
            FakeEditor::default(),
        );
        let project = project("alice");
        let candidate = candidate(&project);
        assert_send(service.run(&project, &candidate, PathBuf::from("write.lua")));
    }

    #[tokio::test]
    async fn rejects_an_extract_outcome_from_the_write_back_host() {
        let service = LuaWriteBackService::new(
            FakeHost {
                invocation: Arc::new(Mutex::new(None)),
                fail: false,
                cancelled: false,
                unexpected_outcome: true,
            },
            FakeEditor::default(),
        );
        let project = project("alice");

        let error = service
            .run(&project, &candidate(&project), PathBuf::from("write.lua"))
            .await
            .expect_err("WriteBack 只能接收空阶段结果");

        assert!(matches!(
            error,
            LuaWriteBackServiceError::UnexpectedOutcome { .. }
        ));
    }

    #[tokio::test]
    async fn candidate_binding_failure_stops_before_starting_the_host() {
        let recorded = Arc::new(Mutex::new(None));
        let service = LuaWriteBackService::new(
            FakeHost {
                invocation: Arc::clone(&recorded),
                fail: false,
                cancelled: false,
                unexpected_outcome: false,
            },
            FakeEditor { bind_fail: true },
        );
        let project = project("alice");

        let error = service
            .run(&project, &candidate(&project), PathBuf::from("write.lua"))
            .await
            .expect_err("无法绑定物理候选时不得启动 Lua Host");

        assert!(matches!(
            error,
            LuaWriteBackServiceError::BindCandidate { .. }
        ));
        assert!(recorded.lock().expect("调用记录锁不应中毒").is_none());
    }

    #[test]
    fn lua_layout_facade_uses_the_actual_region_width_and_preserves_alignment() {
        let calls = ScopedLuaWriteBackHostCalls {
            editor: Arc::new(FakeEditor::default()),
            scope: Arc::new(BoundScopedDirectory::new(
                PathBuf::from("C:/projects/alice/.write_back-stage"),
                super::super::mz_output_scope(),
                (),
            )),
            layout_profile: MzWriteBackLayoutProfile::new(width(3), width(2), width(2)),
        };
        let pairs = vec![
            TrustedLuaWriteBackLayoutPair::new("原文".to_owned(), Some("甲乙丙".to_owned())),
            TrustedLuaWriteBackLayoutPair::new("冻结原文".to_owned(), None),
        ];

        let dialogue = calls
            .layout(TrustedLuaWriteBackLayoutRegion::DialogueBody, pairs.clone())
            .expect("对话实际宽度足以容纳译文");
        assert_eq!(dialogue.status(), TrustedLuaWriteBackLayoutStatus::Applied);
        assert_eq!(dialogue.texts(), ["甲乙丙", "冻结原文"]);

        let scrolling = calls
            .layout(TrustedLuaWriteBackLayoutRegion::ScrollingText, pairs)
            .expect("人工布局是正常内容结果");
        assert_eq!(scrolling.status(), TrustedLuaWriteBackLayoutStatus::Manual);
        assert_eq!(scrolling.texts(), ["甲乙丙", "冻结原文"]);
        assert_eq!(scrolling.inserted_line_breaks(), 0);
        assert_eq!(scrolling.inserted_fullwidth_indents(), 0);
    }

    fn width(value: u32) -> MaxFullwidthChars {
        MaxFullwidthChars::new(value).expect("测试布局宽度应该合法")
    }
}
