//! 把尚未发布的完整写回候选交给共享可信 Lua Host。

use std::error::Error;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, FailureReport, RecoveryFact, ReportedFailure,
    SafeDiagnostic, SafeDiagnosticSource,
};
use crate::execution::OperationCompletion;
use crate::rpg_maker::lua::directory_cache::SuccessfulDirectoryListCache;
use crate::rpg_maker::lua::hosting::TrustedLuaExecutionHostingError;
use crate::rpg_maker::lua::runtime::{
    OwnedLuaProgram, TrustedLuaHostCallError, TrustedLuaOutputEntry, TrustedLuaOutputEntryKind,
    TrustedLuaWriteBackHostCalls, TrustedLuaWriteBackLayoutPair, TrustedLuaWriteBackLayoutRegion,
    TrustedLuaWriteBackLayoutResult, TrustedLuaWriteBackLayoutStatus,
};
use crate::rpg_maker::lua::{
    LuaInvocation, LuaProjectContext, TrustedLuaExecutionHost, TrustedLuaExecutionOutcome,
};
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::project::RpgMakerWriteBackLayoutProfile;
use crate::storage::file_system::{
    BoundScopedDirectory, ScopedDirectoryBindError, ScopedDirectoryEditError,
    ScopedDirectoryEditor, ScopedDirectoryEntry, ScopedDirectoryEntryKind, ScopedDirectoryPath,
    StagedDirectory,
};
use crate::storage::scoped_path::{
    ExactDirectoryEntryResolutionError, resolve_exact_directory_entry,
};
use crate::windows_path::WindowsOrdinalCaseKeyError;

use super::standard::{
    RpgMakerLayoutTextPair, RpgMakerTextLayoutOutcome, RpgMakerWriteBackLayoutRegion,
};
use super::{LuaWriteBack, PreparedWriteBackCandidate, WriteBackLuaDiagnostic};

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
        program: OwnedLuaProgram,
    ) -> impl std::future::Future<Output = Result<OperationCompletion<()>, Self::Error>> + Send
    {
        let prepared = if !candidate.belongs_to(project) {
            Err(LuaWriteBackServiceError::CandidateProjectMismatch {
                project_root: project.workspace_root().to_path_buf(),
                candidate_root: candidate.candidate_root().to_path_buf(),
            })
        } else {
            let error_path = program.main_script_path().to_path_buf();
            let candidate_root = candidate.candidate_root().to_path_buf();
            let bind = self.editor.bind_scoped_directory(
                candidate.staged_directory(),
                super::rpg_maker_output_scope(project.layout().rpg_maker_layout()),
            );
            let editor = Arc::clone(&self.editor);
            let layout_profile = *project.layout_profile();
            let rpg_maker_layout = project.layout().rpg_maker_layout();
            Ok((
                program,
                LuaProjectContext::for_write_back_candidate(
                    project.name().as_str(),
                    project.layout().rpg_maker_layout().engine(),
                    project.source_content_root(),
                    project.database_path().to_path_buf(),
                    project.language_pair().clone(),
                    candidate_root.clone(),
                ),
                error_path,
                candidate_root,
                bind,
                editor,
                layout_profile,
                rpg_maker_layout,
            ))
        };

        async move {
            let (
                program,
                project,
                error_path,
                candidate_root,
                bind,
                editor,
                layout_profile,
                rpg_maker_layout,
            ) = prepared?;
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
                    rpg_maker_layout,
                    output_directories: Arc::default(),
                });
            let invocation = LuaInvocation::write_back(program, project, calls);
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

fn map_output_path(
    layout: crate::rpg_maker::RpgMakerLayout,
    path: ScopedDirectoryPath,
) -> Result<ScopedDirectoryPath, TrustedLuaHostCallError> {
    if path.first_component() != "data" && path.first_component() != "js" {
        return Err(TrustedLuaHostCallError::new(
            "output",
            "outside_content_roots",
            format!(
                "候选逻辑路径只允许小写 data/** 或 js/**：{}",
                path.as_path().display()
            ),
            None,
            None,
        ));
    }
    ScopedDirectoryPath::from_internal_path(layout.map_content_relative(path.as_path())).map_err(
        |error| {
            let message = error.to_string();
            TrustedLuaHostCallError::new(
                "output",
                "invalid_path",
                message,
                None,
                Some(Arc::new(error)),
            )
        },
    )
}

/// 修改操作必须按 Lua 看见的逻辑内容根判断，而不能按 MV 映射后的 `www/data`
/// 或 `www/js` 判断，否则固定布局前缀会把内容根伪装成普通后代。
fn map_output_mutation_path(
    layout: crate::rpg_maker::RpgMakerLayout,
    path: ScopedDirectoryPath,
) -> Result<ScopedDirectoryPath, TrustedLuaHostCallError> {
    if (path.first_component() == "data" || path.first_component() == "js") && path.is_top_level() {
        return Err(output_edit_error::<std::convert::Infallible>(
            ScopedDirectoryEditError::ScopeRootMutation {
                path: path.as_path().to_path_buf(),
            },
        ));
    }
    map_output_path(layout, path)
}

struct ScopedLuaWriteBackHostCalls<E>
where
    E: ScopedDirectoryEditor,
{
    editor: Arc<E>,
    scope: Arc<BoundScopedDirectory<E::ScopeState>>,
    layout_profile: RpgMakerWriteBackLayoutProfile,
    rpg_maker_layout: crate::rpg_maker::RpgMakerLayout,
    output_directories: Arc<SuccessfulDirectoryListCache<ScopedDirectoryEntry>>,
}

/// 把候选中的每个现存路径段解析为目录实际列出的逐字身份。
///
/// 缺失段仍交给具体编辑操作解释：读、列举和删除会沿用它们已有的 `not_found`
/// 语义，而写文件和建目录可以按各自契约创建新末段。仅大小写不同的现存别名则
/// 必须在任何操作之前失败，避免 Windows 把脚本请求静默重定向到另一个物理身份。
async fn resolve_exact_output_path<E>(
    editor: &E,
    scope: &BoundScopedDirectory<E::ScopeState>,
    output_directories: &SuccessfulDirectoryListCache<ScopedDirectoryEntry>,
    path: ScopedDirectoryPath,
) -> Result<ScopedDirectoryPath, TrustedLuaHostCallError>
where
    E: ScopedDirectoryEditor,
{
    let mut exact_parent: Option<ScopedDirectoryPath> = None;
    for component in path.as_path().components() {
        let Component::Normal(expected_name) = component else {
            unreachable!("ScopedDirectoryPath 已建立普通相对路径段不变量")
        };
        let expected_name = expected_name
            .to_str()
            .expect("ScopedDirectoryPath 已建立 UTF-8 路径段不变量");
        let entries =
            list_output_directory(editor, scope, output_directories, exact_parent.as_ref()).await?;
        let parent = exact_parent
            .as_ref()
            .map_or_else(|| Path::new(""), ScopedDirectoryPath::as_path);
        let resolved = resolve_exact_directory_entry(
            parent,
            expected_name,
            entries.iter().map(|entry| parent.join(entry.name())),
        )
        .map_err(output_resolution_error)?;
        let Some(resolved) = resolved else {
            return Ok(path);
        };
        exact_parent = Some(
            ScopedDirectoryPath::from_internal_path(resolved)
                .expect("实际目录项与受检父路径组合后仍应是安全内部路径"),
        );
    }
    Ok(path)
}

async fn list_output_directory<E>(
    editor: &E,
    scope: &BoundScopedDirectory<E::ScopeState>,
    output_directories: &SuccessfulDirectoryListCache<ScopedDirectoryEntry>,
    path: Option<&ScopedDirectoryPath>,
) -> Result<Arc<[ScopedDirectoryEntry]>, TrustedLuaHostCallError>
where
    E: ScopedDirectoryEditor,
{
    let cache_key = path.map_or_else(PathBuf::new, |path| path.as_path().to_path_buf());
    let (cached, observed_epoch) = output_directories.lookup(&cache_key);
    if let Some(entries) = cached {
        return Ok(entries);
    }
    let entries = match path {
        Some(path) => editor
            .list_scoped_directory(scope, path.clone())
            .await
            .map_err(output_edit_error)?,
        None => editor
            .list_scoped_root(scope)
            .await
            .map_err(output_edit_error)?,
    };
    Ok(output_directories.insert_if_unchanged(cache_key, observed_epoch, entries))
}

fn invalidate_output_parent_and_target(
    output_directories: &SuccessfulDirectoryListCache<ScopedDirectoryEntry>,
    path: &ScopedDirectoryPath,
) {
    if let Some(parent) = path.as_path().parent() {
        output_directories.invalidate(parent);
    }
    output_directories.invalidate(path.as_path());
}

fn invalidate_created_output_path(
    output_directories: &SuccessfulDirectoryListCache<ScopedDirectoryEntry>,
    path: &ScopedDirectoryPath,
) {
    output_directories.invalidate(Path::new(""));
    let mut prefix = PathBuf::new();
    for component in path.as_path().components() {
        let Component::Normal(name) = component else {
            unreachable!("ScopedDirectoryPath 已建立普通相对路径段不变量")
        };
        prefix.push(name);
        output_directories.invalidate(&prefix);
    }
}

fn invalidate_removed_output_subtree(
    output_directories: &SuccessfulDirectoryListCache<ScopedDirectoryEntry>,
    path: &ScopedDirectoryPath,
) {
    if let Some(parent) = path.as_path().parent() {
        output_directories.invalidate(parent);
    }
    output_directories.invalidate_subtree(path.as_path());
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
        let path = map_output_path(self.rpg_maker_layout, path);
        let editor = Arc::clone(&self.editor);
        let scope = Arc::clone(&self.scope);
        let output_directories = Arc::clone(&self.output_directories);
        Box::pin(async move {
            let path = path?;
            let path = resolve_exact_output_path(
                editor.as_ref(),
                scope.as_ref(),
                output_directories.as_ref(),
                path,
            )
            .await?;
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
        let path = map_output_path(self.rpg_maker_layout, path);
        let editor = Arc::clone(&self.editor);
        let scope = Arc::clone(&self.scope);
        let output_directories = Arc::clone(&self.output_directories);
        Box::pin(async move {
            let path = path?;
            let path = resolve_exact_output_path(
                editor.as_ref(),
                scope.as_ref(),
                output_directories.as_ref(),
                path,
            )
            .await?;
            list_output_directory(
                editor.as_ref(),
                scope.as_ref(),
                output_directories.as_ref(),
                Some(&path),
            )
            .await?
            .iter()
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
        let path = map_output_mutation_path(self.rpg_maker_layout, path);
        let editor = Arc::clone(&self.editor);
        let scope = Arc::clone(&self.scope);
        let output_directories = Arc::clone(&self.output_directories);
        Box::pin(async move {
            let path = path?;
            let path = resolve_exact_output_path(
                editor.as_ref(),
                scope.as_ref(),
                output_directories.as_ref(),
                path,
            )
            .await?;
            let invalidation_path = path.clone();
            let result = editor.create_scoped_directory(&scope, path).await;
            invalidate_created_output_path(output_directories.as_ref(), &invalidation_path);
            result.map_err(output_edit_error)
        })
    }

    fn write_output(
        &self,
        path: ScopedDirectoryPath,
        bytes: Vec<u8>,
    ) -> Pin<
        Box<dyn std::future::Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>,
    > {
        let path = map_output_mutation_path(self.rpg_maker_layout, path);
        let editor = Arc::clone(&self.editor);
        let scope = Arc::clone(&self.scope);
        let output_directories = Arc::clone(&self.output_directories);
        Box::pin(async move {
            let path = path?;
            let path = resolve_exact_output_path(
                editor.as_ref(),
                scope.as_ref(),
                output_directories.as_ref(),
                path,
            )
            .await?;
            let invalidation_path = path.clone();
            let result = editor.write_scoped_file(&scope, path, bytes).await;
            invalidate_output_parent_and_target(output_directories.as_ref(), &invalidation_path);
            result.map_err(output_edit_error)
        })
    }

    fn remove_output(
        &self,
        path: ScopedDirectoryPath,
    ) -> Pin<
        Box<dyn std::future::Future<Output = Result<(), TrustedLuaHostCallError>> + Send + 'static>,
    > {
        let path = map_output_mutation_path(self.rpg_maker_layout, path);
        let editor = Arc::clone(&self.editor);
        let scope = Arc::clone(&self.scope);
        let output_directories = Arc::clone(&self.output_directories);
        Box::pin(async move {
            let path = path?;
            let path = resolve_exact_output_path(
                editor.as_ref(),
                scope.as_ref(),
                output_directories.as_ref(),
                path,
            )
            .await?;
            let invalidation_path = path.clone();
            let result = editor.remove_scoped_path(&scope, path).await;
            invalidate_removed_output_subtree(output_directories.as_ref(), &invalidation_path);
            result.map_err(output_edit_error)
        })
    }

    fn layout(
        &self,
        region: TrustedLuaWriteBackLayoutRegion,
        pairs: Vec<TrustedLuaWriteBackLayoutPair>,
    ) -> Result<TrustedLuaWriteBackLayoutResult, TrustedLuaHostCallError> {
        let region = match region {
            TrustedLuaWriteBackLayoutRegion::DialogueBody => {
                RpgMakerWriteBackLayoutRegion::DialogueBody
            }
            TrustedLuaWriteBackLayoutRegion::ScrollingText => {
                RpgMakerWriteBackLayoutRegion::ScrollingText
            }
            TrustedLuaWriteBackLayoutRegion::HelpDescription => {
                RpgMakerWriteBackLayoutRegion::HelpDescription
            }
        };
        let pairs = pairs
            .into_iter()
            .map(|pair| {
                RpgMakerLayoutTextPair::new(
                    pair.original().to_owned(),
                    pair.translation().map(str::to_owned),
                )
            })
            .collect::<Vec<_>>();
        let (status, applied) =
            match super::standard::layout::layout(region, &pairs, &self.layout_profile) {
                RpgMakerTextLayoutOutcome::Applied(applied) => {
                    (TrustedLuaWriteBackLayoutStatus::Applied, applied)
                }
                RpgMakerTextLayoutOutcome::Manual(manual) => {
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

fn output_resolution_error(error: ExactDirectoryEntryResolutionError) -> TrustedLuaHostCallError {
    let message = error.to_string();
    let (kind, diagnostic) = match &error {
        ExactDirectoryEntryResolutionError::CaseMismatch(error) => (
            "case_mismatch",
            SafeDiagnostic::new(
                DiagnosticCode::FileSystemOperation,
                DiagnosticStage::WriteBack,
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
                        DiagnosticStage::WriteBack,
                        DiagnosticSubject::path(path),
                        DiagnosticReason::Resource {
                            resource: "Windows 文件名 UTF-16 单元数".to_owned(),
                            actual: *observed,
                            maximum: Some(*maximum),
                        },
                        DiagnosticImpact::Unchanged,
                        DiagnosticAction::ReportBug,
                    )
                }
                WindowsOrdinalCaseKeyError::WindowsApi { phase, source } => SafeDiagnostic::io(
                    DiagnosticCode::FileSystemOperation,
                    DiagnosticStage::WriteBack,
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
            ("io", diagnostic)
        }
    };
    TrustedLuaHostCallError::new("filesystem", kind, message, None, Some(Arc::new(error)))
        .with_safe_diagnostic(diagnostic)
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

impl<O, R, E> LuaWriteBackServiceError<TrustedLuaExecutionHostingError<O, R>, E>
where
    O: SafeDiagnosticSource,
    R: SafeDiagnosticSource,
    E: SafeDiagnosticSource,
{
    pub(crate) fn safe_diagnostic(&self) -> SafeDiagnostic {
        match self {
            Self::CandidateProjectMismatch {
                project_root,
                candidate_root,
            } => SafeDiagnostic::new(
                DiagnosticCode::WriteBackCandidate,
                DiagnosticStage::WriteBack,
                DiagnosticSubject::path(candidate_root),
                DiagnosticReason::failure(DiagnosticFailureKind::WriteBackCandidateProjectMismatch),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::ReportBug,
            )
            .with_recovery(RecoveryFact::path(project_root)),
            Self::ExecuteHost {
                script_path,
                candidate_root,
                source,
            } => source
                .safe_diagnostic(
                    DiagnosticStage::WriteBack,
                    script_path,
                    DiagnosticImpact::Unchanged,
                )
                .with_recovery(RecoveryFact::path(candidate_root)),
            Self::BindCandidate {
                candidate_root,
                source,
            } => scoped_bind_diagnostic(source, candidate_root),
            Self::UnexpectedOutcome {
                script_path,
                candidate_root,
            } => SafeDiagnostic::new(
                DiagnosticCode::WriteBackCandidate,
                DiagnosticStage::WriteBack,
                DiagnosticSubject::path(script_path),
                DiagnosticReason::failure(DiagnosticFailureKind::WriteBackUnexpectedLuaOutcome),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::ReportBug,
            )
            .with_recovery(RecoveryFact::path(candidate_root)),
        }
    }
}

impl<O, R, E> WriteBackLuaDiagnostic
    for LuaWriteBackServiceError<TrustedLuaExecutionHostingError<O, R>, E>
where
    O: Error + SafeDiagnosticSource + Send + Sync + 'static,
    R: Error + SafeDiagnosticSource + Send + Sync + 'static,
    E: Error + SafeDiagnosticSource + Send + Sync + 'static,
{
    fn into_write_back_failure_report(self) -> FailureReport {
        match self {
            Self::ExecuteHost {
                script_path,
                candidate_root,
                source,
            } => source
                .into_failure_report(
                    DiagnosticStage::WriteBack,
                    &script_path,
                    DiagnosticImpact::Unchanged,
                )
                .with_primary_recovery(RecoveryFact::path(candidate_root)),
            source => {
                let diagnostic = source.safe_diagnostic();
                FailureReport::new(ReportedFailure::new(diagnostic, source))
            }
        }
    }
}

fn scoped_bind_diagnostic<E>(
    source: &ScopedDirectoryBindError<E>,
    candidate_root: &Path,
) -> SafeDiagnostic
where
    E: SafeDiagnosticSource,
{
    match source {
        ScopedDirectoryBindError::WrongEditorInstance => SafeDiagnostic::new(
            DiagnosticCode::WriteBackCandidate,
            DiagnosticStage::WriteBack,
            DiagnosticSubject::path(candidate_root),
            DiagnosticReason::failure(DiagnosticFailureKind::WrongPublisherInstance),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::ReportBug,
        ),
        ScopedDirectoryBindError::CandidateFinalized { root } => SafeDiagnostic::new(
            DiagnosticCode::WriteBackCandidate,
            DiagnosticStage::WriteBack,
            DiagnosticSubject::path(root),
            DiagnosticReason::failure(DiagnosticFailureKind::StateMismatch),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::ReportBug,
        ),
        ScopedDirectoryBindError::CandidateIdentityChanged { root } => SafeDiagnostic::new(
            DiagnosticCode::WriteBackCandidate,
            DiagnosticStage::WriteBack,
            DiagnosticSubject::path(root),
            DiagnosticReason::failure(DiagnosticFailureKind::FileIdentityChanged),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckPathAndPermissions,
        ),
        ScopedDirectoryBindError::Failed { root, source } => source
            .safe_diagnostic_source(
                DiagnosticStage::WriteBack,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckPathAndPermissions,
            )
            .with_recovery(RecoveryFact::path(root)),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::rpg_maker::ProjectName;
    use crate::rpg_maker::lua::LuaPhase;
    use crate::rpg_maker::project::MaxFullwidthChars;

    #[test]
    fn output_paths_keep_logical_roots_and_hide_mv_www_mapping() {
        let data = ScopedDirectoryPath::new(PathBuf::from("data/Actors.json")).unwrap();
        assert_eq!(
            map_output_path(crate::rpg_maker::RpgMakerLayout::MZ, data.clone())
                .unwrap()
                .as_path(),
            Path::new("data/Actors.json")
        );
        assert_eq!(
            map_output_path(crate::rpg_maker::RpgMakerLayout::MV, data)
                .unwrap()
                .as_path(),
            Path::new("www/data/Actors.json")
        );

        for path in [
            "Data/Actors.json",
            "www/data/Actors.json",
            "assets/file.json",
        ] {
            let error = map_output_path(
                crate::rpg_maker::RpgMakerLayout::MV,
                ScopedDirectoryPath::new(PathBuf::from(path)).unwrap(),
            )
            .expect_err("Lua 逻辑输出只允许小写 data/js 根");
            assert_eq!(error.kind(), "outside_content_roots");
        }
    }

    #[tokio::test]
    async fn logical_output_roots_are_read_only_before_mv_or_mz_layout_mapping() {
        for (layout, entries) in [
            (
                crate::rpg_maker::RpgMakerLayout::MZ,
                vec![
                    (
                        PathBuf::new(),
                        vec![
                            entry("data", ScopedDirectoryEntryKind::Directory),
                            entry("js", ScopedDirectoryEntryKind::Directory),
                        ],
                    ),
                    (PathBuf::from("data"), Vec::new()),
                    (PathBuf::from("js"), Vec::new()),
                ],
            ),
            (
                crate::rpg_maker::RpgMakerLayout::MV,
                vec![
                    (
                        PathBuf::new(),
                        vec![entry("www", ScopedDirectoryEntryKind::Directory)],
                    ),
                    (
                        PathBuf::from("www"),
                        vec![
                            entry("data", ScopedDirectoryEntryKind::Directory),
                            entry("js", ScopedDirectoryEntryKind::Directory),
                        ],
                    ),
                    (PathBuf::from("www/data"), Vec::new()),
                    (PathBuf::from("www/js"), Vec::new()),
                ],
            ),
        ] {
            let editor = Arc::new(FakeEditor::with_entries(entries));
            let calls = output_calls(layout, Arc::clone(&editor));

            for root in ["data", "js"] {
                calls
                    .list_output(scoped_path(root))
                    .await
                    .expect("逻辑内容根允许列举");
                calls
                    .read_output(scoped_path(root))
                    .await
                    .expect("Host 不应把读取误判为根修改");

                let mutations = [
                    calls.create_output_directory(scoped_path(root)).await,
                    calls
                        .write_output(scoped_path(root), b"changed".to_vec())
                        .await,
                    calls.remove_output(scoped_path(root)).await,
                ];
                for error in
                    mutations.map(|result| result.expect_err("逻辑 data/js 根禁止任何修改"))
                {
                    assert_eq!(error.domain(), "output");
                    assert_eq!(error.kind(), "scope_root_mutation");
                }
            }

            let expected_reads = ["data", "js"]
                .map(|root| {
                    format!(
                        "read:{}",
                        layout.map_content_relative(Path::new(root)).display()
                    )
                })
                .to_vec();
            assert_eq!(
                *editor
                    .terminal_operations
                    .lock()
                    .expect("候选操作记录锁不应中毒"),
                expected_reads
            );
        }
    }

    #[tokio::test]
    async fn output_operations_reject_case_aliases_before_reaching_the_terminal_edit() {
        let editor = Arc::new(FakeEditor::with_entries([
            (
                PathBuf::new(),
                vec![entry("data", ScopedDirectoryEntryKind::Directory)],
            ),
            (
                PathBuf::from("data"),
                vec![
                    entry("Items.json", ScopedDirectoryEntryKind::File),
                    entry("Generated", ScopedDirectoryEntryKind::Directory),
                ],
            ),
            (PathBuf::from("data/Generated"), Vec::new()),
        ]));
        let calls = output_calls(crate::rpg_maker::RpgMakerLayout::MZ, Arc::clone(&editor));

        let aliases = [
            calls
                .read_output(scoped_path("data/items.json"))
                .await
                .map(|_| ()),
            calls
                .list_output(scoped_path("data/items.json"))
                .await
                .map(|_| ()),
            calls
                .create_output_directory(scoped_path("data/generated/child"))
                .await,
            calls
                .write_output(scoped_path("data/items.json"), b"changed".to_vec())
                .await,
            calls.remove_output(scoped_path("data/items.json")).await,
        ];
        for error in aliases.map(|result| result.expect_err("仅大小写不同的别名必须失败"))
        {
            assert_eq!(error.domain(), "filesystem");
            assert_eq!(error.kind(), "case_mismatch");
        }
        assert!(
            editor
                .terminal_operations
                .lock()
                .expect("候选操作记录锁不应中毒")
                .is_empty(),
            "别名请求不得到达读写或删除终态操作"
        );

        calls
            .read_output(scoped_path("data/Items.json"))
            .await
            .expect("逐字现存路径应能读取");
        calls
            .write_output(scoped_path("data/New.json"), b"new".to_vec())
            .await
            .expect("写文件允许创建不存在的末段");
        calls
            .create_output_directory(scoped_path("data/NewDirectory"))
            .await
            .expect("建目录允许创建不存在的末段");
        assert_eq!(
            *editor
                .terminal_operations
                .lock()
                .expect("候选操作记录锁不应中毒"),
            [
                "read:data/Items.json",
                "write:data/New.json",
                "create:data/NewDirectory",
            ]
        );
    }

    #[tokio::test]
    async fn output_directory_cache_is_limited_to_one_lua_execution() {
        let editor = Arc::new(FakeEditor::with_entries([
            (
                PathBuf::new(),
                vec![entry("data", ScopedDirectoryEntryKind::Directory)],
            ),
            (
                PathBuf::from("data"),
                vec![entry("Generated", ScopedDirectoryEntryKind::Directory)],
            ),
            (
                PathBuf::from("data/Generated"),
                vec![entry("Item.json", ScopedDirectoryEntryKind::File)],
            ),
        ]));
        let path = scoped_path("data/Generated/Item.json");
        let first_execution =
            output_calls(crate::rpg_maker::RpgMakerLayout::MZ, Arc::clone(&editor));

        for _ in 0..8 {
            first_execution
                .read_output(path.clone())
                .await
                .expect("同一次执行应能重复读取候选");
        }
        first_execution
            .list_output(scoped_path("data/Generated"))
            .await
            .expect("已经观测的候选目录应能列举");
        assert_eq!(
            *editor
                .directory_lists
                .lock()
                .expect("候选目录列举记录锁不应中毒"),
            [
                PathBuf::new(),
                PathBuf::from("data"),
                PathBuf::from("data/Generated"),
            ],
            "重复文件数不应再乘以每个父目录的列举次数"
        );

        let next_execution =
            output_calls(crate::rpg_maker::RpgMakerLayout::MZ, Arc::clone(&editor));
        next_execution
            .read_output(path)
            .await
            .expect("下一次执行仍应重新观测候选");
        assert_eq!(
            editor
                .directory_lists
                .lock()
                .expect("候选目录列举记录锁不应中毒")
                .len(),
            6
        );
    }

    #[tokio::test]
    async fn failed_output_directory_lists_are_not_cached() {
        let editor = Arc::new(FakeEditor::with_entries([
            (
                PathBuf::new(),
                vec![entry("data", ScopedDirectoryEntryKind::Directory)],
            ),
            (PathBuf::from("data"), Vec::new()),
        ]));
        let calls = output_calls(crate::rpg_maker::RpgMakerLayout::MZ, Arc::clone(&editor));

        for _ in 0..2 {
            let error = calls
                .list_output(scoped_path("data/Missing"))
                .await
                .expect_err("失败的候选目录列举不得进入缓存");
            assert_eq!(error.domain(), "output");
            assert_eq!(error.kind(), "not_found");
        }
        assert_eq!(
            *editor
                .directory_lists
                .lock()
                .expect("候选目录列举记录锁不应中毒"),
            [
                PathBuf::new(),
                PathBuf::from("data"),
                PathBuf::from("data/Missing"),
                PathBuf::from("data/Missing"),
            ]
        );
    }

    #[tokio::test]
    async fn recursive_directory_creation_invalidates_all_cached_ancestors() {
        for fail_mutations in [false, true] {
            let editor = Arc::new(FakeEditor {
                fail_mutations,
                ..FakeEditor::with_entries([
                    (
                        PathBuf::new(),
                        vec![entry("data", ScopedDirectoryEntryKind::Directory)],
                    ),
                    (PathBuf::from("data"), Vec::new()),
                ])
            });
            let calls = output_calls(crate::rpg_maker::RpgMakerLayout::MZ, Arc::clone(&editor));

            calls
                .list_output(scoped_path("data"))
                .await
                .expect("应先缓存候选根和现存 data 目录");
            let create = calls
                .create_output_directory(scoped_path("data/New/Nested"))
                .await;
            if fail_mutations {
                let error = create.expect_err("逐段建目录失败仍可能留下已经创建的前缀");
                assert_eq!(error.domain(), "output");
                assert_eq!(error.kind(), "io");
            } else {
                create.expect("应允许具体编辑器逐段建立缺失目录");
            }
            let data_entries = calls
                .list_output(scoped_path("data"))
                .await
                .expect("建目录尝试后必须重新观测所有祖先目录");
            assert_eq!(
                data_entries
                    .iter()
                    .map(TrustedLuaOutputEntry::name)
                    .collect::<Vec<_>>(),
                ["New"]
            );
            let new_entries = calls
                .list_output(scoped_path("data/New"))
                .await
                .expect("已经创建的前缀必须能够按逐字身份重新列举");
            let expected_new_entries: &[&str] = if fail_mutations { &[] } else { &["Nested"] };
            assert_eq!(
                new_entries
                    .iter()
                    .map(TrustedLuaOutputEntry::name)
                    .collect::<Vec<_>>(),
                expected_new_entries
            );

            let terminal_count = editor
                .terminal_operations
                .lock()
                .expect("候选操作记录锁不应中毒")
                .len();
            let alias_error = calls
                .create_output_directory(scoped_path("data/new/Other"))
                .await
                .expect_err("创建后的大小写别名必须由最新祖先列举拒绝");
            assert_eq!(alias_error.domain(), "filesystem");
            assert_eq!(alias_error.kind(), "case_mismatch");
            assert_eq!(
                editor
                    .terminal_operations
                    .lock()
                    .expect("候选操作记录锁不应中毒")
                    .len(),
                terminal_count,
                "大小写别名不得到达具体编辑操作"
            );
        }
    }

    #[tokio::test]
    async fn output_mutations_invalidate_affected_lists_after_success_or_failure() {
        for fail_mutations in [false, true] {
            let editor = Arc::new(FakeEditor {
                fail_mutations,
                ..FakeEditor::with_entries([
                    (
                        PathBuf::new(),
                        vec![entry("data", ScopedDirectoryEntryKind::Directory)],
                    ),
                    (
                        PathBuf::from("data"),
                        vec![
                            entry("Generated", ScopedDirectoryEntryKind::Directory),
                            entry("Items.json", ScopedDirectoryEntryKind::File),
                        ],
                    ),
                    (
                        PathBuf::from("data/Generated"),
                        vec![entry("Nested", ScopedDirectoryEntryKind::Directory)],
                    ),
                    (
                        PathBuf::from("data/Generated/Nested"),
                        vec![entry("Leaf.json", ScopedDirectoryEntryKind::File)],
                    ),
                ])
            });
            let calls = output_calls(crate::rpg_maker::RpgMakerLayout::MZ, Arc::clone(&editor));
            let leaf = scoped_path("data/Generated/Nested/Leaf.json");

            calls
                .read_output(leaf.clone())
                .await
                .expect("应先缓存完整祖先链");

            let create = calls
                .create_output_directory(scoped_path("data/Generated/New"))
                .await;
            if fail_mutations {
                create.expect_err("建目录失败仍须使相关缓存失效");
            } else {
                create.expect("建目录成功应使相关缓存失效");
            }
            calls
                .list_output(scoped_path("data/Generated"))
                .await
                .expect("建目录尝试后应重新列举父目录");

            let write = calls
                .write_output(scoped_path("data/Items.json"), b"changed".to_vec())
                .await;
            if fail_mutations {
                write.expect_err("写文件失败仍须使相关缓存失效");
            } else {
                write.expect("写文件成功应使相关缓存失效");
            }
            calls
                .read_output(leaf.clone())
                .await
                .expect("写文件尝试后应重新列举父目录");

            let remove = calls.remove_output(scoped_path("data/Generated")).await;
            if fail_mutations {
                remove.expect_err("删除失败仍须使相关缓存失效");
            } else {
                remove.expect("删除成功应使相关缓存失效");
            }
            calls
                .read_output(leaf.clone())
                .await
                .expect("删除尝试后应重新列举目标及全部后代");

            assert_eq!(
                *editor
                    .directory_lists
                    .lock()
                    .expect("候选目录列举记录锁不应中毒"),
                [
                    PathBuf::new(),
                    PathBuf::from("data"),
                    PathBuf::from("data/Generated"),
                    PathBuf::from("data/Generated/Nested"),
                    PathBuf::new(),
                    PathBuf::from("data"),
                    PathBuf::from("data/Generated"),
                    PathBuf::from("data"),
                    PathBuf::from("data"),
                    PathBuf::from("data/Generated"),
                    PathBuf::from("data/Generated/Nested"),
                ]
            );
        }
    }

    #[tokio::test]
    async fn mv_output_case_resolution_keeps_www_internal() {
        let editor = Arc::new(FakeEditor::with_entries([
            (
                PathBuf::new(),
                vec![entry("www", ScopedDirectoryEntryKind::Directory)],
            ),
            (
                PathBuf::from("www"),
                vec![entry("data", ScopedDirectoryEntryKind::Directory)],
            ),
            (
                PathBuf::from("www/data"),
                vec![entry("Items.json", ScopedDirectoryEntryKind::File)],
            ),
        ]));
        let calls = output_calls(crate::rpg_maker::RpgMakerLayout::MV, Arc::clone(&editor));

        let error = calls
            .write_output(scoped_path("data/items.json"), b"changed".to_vec())
            .await
            .expect_err("MV 也必须按映射后的真实目录项检查大小写");
        assert_eq!(error.domain(), "filesystem");
        assert_eq!(error.kind(), "case_mismatch");
        assert!(
            editor
                .terminal_operations
                .lock()
                .expect("候选操作记录锁不应中毒")
                .is_empty()
        );
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn windows_candidate_does_not_overwrite_a_case_aliased_output_file() {
        use crate::runtime::filesystem::{
            DirectoryPublisherConfig, SystemFileSystem, SystemFileSystemConfig,
        };
        use crate::storage::file_system::{
            DirectoryPublishIntent, DirectorySourceMapping, DirectoryStageRequest,
            RecoverableDirectoryPublisher,
        };
        use std::fs;

        let workspace = tempfile::tempdir().expect("应建立 Windows 候选测试目录");
        let source = workspace.path().join("source");
        let source_data = source.join("data");
        let source_js = source.join("js");
        fs::create_dir_all(&source_data).expect("应建立来源 data");
        fs::create_dir_all(&source_js).expect("应建立来源 js");
        fs::write(source_data.join("Items.json"), b"original").expect("应建立大小写精确的来源文件");

        let file_system = SystemFileSystem::new(SystemFileSystemConfig::production())
            .expect("应建立生产文件系统根");
        let publisher = file_system.directory_publisher(
            DirectoryPublisherConfig::production(workspace.path().join("locks"))
                .expect("测试发布配置应合法"),
        );
        let request = DirectoryStageRequest::new(
            workspace.path().join("write_back"),
            DirectoryPublishIntent::CreateNew,
            vec![
                DirectorySourceMapping::new(source_data, PathBuf::from("data"))
                    .expect("data 映射应合法"),
                DirectorySourceMapping::new(source_js, PathBuf::from("js")).expect("js 映射应合法"),
            ],
            Vec::new(),
            Vec::new(),
        )
        .expect("候选请求应合法");
        let staged = publisher.prepare(request).await.expect("应准备候选");
        let actual = staged.staging_root().join("data/Items.json");
        let scope = Arc::new(
            publisher
                .bind_scoped_directory(
                    &staged,
                    super::super::rpg_maker_output_scope(crate::rpg_maker::RpgMakerLayout::MZ),
                )
                .await
                .expect("应绑定候选编辑范围"),
        );
        let calls = ScopedLuaWriteBackHostCalls {
            editor: Arc::new(publisher.clone()),
            scope: Arc::clone(&scope),
            layout_profile: RpgMakerWriteBackLayoutProfile::new(width(3), width(2), width(2)),
            rpg_maker_layout: crate::rpg_maker::RpgMakerLayout::MZ,
            output_directories: Arc::default(),
        };

        let error = calls
            .write_output(scoped_path("data/items.json"), b"changed".to_vec())
            .await
            .expect_err("Windows 不得把错误大小写静默解析到 Items.json");
        assert_eq!(error.domain(), "filesystem");
        assert_eq!(error.kind(), "case_mismatch");
        assert_eq!(
            fs::read(&actual).expect("真实候选文件应仍可读"),
            b"original"
        );

        drop(calls);
        drop(scope);
        publisher.discard(staged).await.expect("测试结束应丢弃候选");
        file_system
            .shutdown()
            .await
            .expect("文件系统 worker 应正常关闭");
    }

    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_mv_and_mz_candidates_keep_logical_content_roots_read_only() {
        for layout in [
            crate::rpg_maker::RpgMakerLayout::MZ,
            crate::rpg_maker::RpgMakerLayout::MV,
        ] {
            assert_real_candidate_roots_are_read_only(layout).await;
        }
    }

    #[cfg(windows)]
    async fn assert_real_candidate_roots_are_read_only(layout: crate::rpg_maker::RpgMakerLayout) {
        use crate::runtime::filesystem::{
            DirectoryPublisherConfig, SystemFileSystem, SystemFileSystemConfig,
        };
        use crate::storage::file_system::{
            DirectoryPublishIntent, DirectorySourceMapping, DirectoryStageRequest,
            RecoverableDirectoryPublisher,
        };
        use std::fs;

        let workspace = tempfile::tempdir().expect("应建立候选根修改测试目录");
        let source_data = workspace.path().join("source-data");
        let source_js = workspace.path().join("source-js");
        fs::create_dir_all(&source_data).expect("应建立来源 data");
        fs::create_dir_all(&source_js).expect("应建立来源 js");
        fs::write(source_data.join("Items.json"), b"{}").expect("应建立来源 data 文件");
        fs::write(source_js.join("plugins.js"), b"var $plugins = [];").expect("应建立来源 js 文件");

        let file_system = SystemFileSystem::new(SystemFileSystemConfig::production())
            .expect("应建立生产文件系统根");
        let publisher = file_system.directory_publisher(
            DirectoryPublisherConfig::production(workspace.path().join("locks"))
                .expect("测试发布配置应合法"),
        );
        let request = DirectoryStageRequest::new(
            workspace.path().join("write_back"),
            DirectoryPublishIntent::CreateNew,
            vec![
                DirectorySourceMapping::new(source_data, layout.data_relative())
                    .expect("data 映射应合法"),
                DirectorySourceMapping::new(source_js, layout.js_relative())
                    .expect("js 映射应合法"),
            ],
            Vec::new(),
            Vec::new(),
        )
        .expect("候选请求应合法");
        let staged = publisher.prepare(request).await.expect("应准备候选");
        let scope = Arc::new(
            publisher
                .bind_scoped_directory(&staged, super::super::rpg_maker_output_scope(layout))
                .await
                .expect("应绑定候选编辑范围"),
        );
        let calls = ScopedLuaWriteBackHostCalls {
            editor: Arc::new(publisher.clone()),
            scope: Arc::clone(&scope),
            layout_profile: RpgMakerWriteBackLayoutProfile::new(width(3), width(2), width(2)),
            rpg_maker_layout: layout,
            output_directories: Arc::default(),
        };

        for root in ["data", "js"] {
            calls
                .list_output(scoped_path(root))
                .await
                .expect("真实候选逻辑根应允许列举");
            let read_error = calls
                .read_output(scoped_path(root))
                .await
                .expect_err("内容根是目录，读取应按普通 not_file 失败");
            assert_eq!(read_error.domain(), "output");
            assert_eq!(read_error.kind(), "not_file");

            let mutations = [
                calls.create_output_directory(scoped_path(root)).await,
                calls
                    .write_output(scoped_path(root), b"changed".to_vec())
                    .await,
                calls.remove_output(scoped_path(root)).await,
            ];
            for error in mutations.map(|result| result.expect_err("真实候选逻辑根禁止任何修改"))
            {
                assert_eq!(error.domain(), "output");
                assert_eq!(error.kind(), "scope_root_mutation");
            }

            let physical_root = staged.staging_root().join(match root {
                "data" => layout.data_relative(),
                "js" => layout.js_relative(),
                _ => unreachable!("测试只遍历 data/js"),
            });
            assert!(physical_root.is_dir(), "根修改失败后真实目录必须仍存在");
        }

        drop(calls);
        drop(scope);
        publisher.discard(staged).await.expect("测试结束应丢弃候选");
        file_system
            .shutdown()
            .await
            .expect("文件系统 worker 应正常关闭");
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
        unexpected_outcome: bool,
    }

    impl TrustedLuaExecutionHost for FakeHost {
        type TranslationClient = ();
        type Error = FakeError;

        async fn execute(
            &self,
            invocation: LuaInvocation<Self::TranslationClient>,
        ) -> Result<OperationCompletion<TrustedLuaExecutionOutcome>, Self::Error> {
            let LuaInvocation::WriteBack {
                program,
                project,
                calls: _,
            } = invocation
            else {
                panic!("Lua 写回服务只应提交 WriteBack 调用")
            };
            *self.invocation.lock().expect("调用记录锁不应中毒") = Some(RecordedInvocation {
                phase: LuaPhase::WriteBack,
                script_path: program.main_script_path().to_path_buf(),
                project,
            });
            if self.fail {
                Err(FakeError)
            } else if self.cancelled {
                Ok(OperationCompletion::Cancelled)
            } else if self.unexpected_outcome {
                Ok(OperationCompletion::Completed(
                    TrustedLuaExecutionOutcome::ExtractIntent(
                        crate::rpg_maker::lua::runtime::TrustedLuaExtractIntent::Deactivate,
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

    impl SafeDiagnosticSource for FakeError {
        fn safe_diagnostic_source(
            &self,
            stage: DiagnosticStage,
            impact: DiagnosticImpact,
            action: DiagnosticAction,
        ) -> SafeDiagnostic {
            SafeDiagnostic::new(
                DiagnosticCode::InternalOperation,
                stage,
                DiagnosticSubject::component("fake Lua root"),
                DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
                impact,
                action,
            )
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeEditorError;

    impl fmt::Display for FakeEditorError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("editor failed")
        }
    }

    impl Error for FakeEditorError {}

    impl SafeDiagnosticSource for FakeEditorError {
        fn safe_diagnostic_source(
            &self,
            stage: DiagnosticStage,
            impact: DiagnosticImpact,
            action: DiagnosticAction,
        ) -> SafeDiagnostic {
            SafeDiagnostic::new(
                DiagnosticCode::FileSystemOperation,
                stage,
                DiagnosticSubject::component("fake candidate editor"),
                DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
                impact,
                action,
            )
        }
    }

    #[derive(Clone, Default)]
    struct FakeEditor {
        bind_fail: bool,
        fail_mutations: bool,
        entries:
            Arc<Mutex<BTreeMap<PathBuf, Vec<crate::storage::file_system::ScopedDirectoryEntry>>>>,
        directory_lists: Arc<Mutex<Vec<PathBuf>>>,
        terminal_operations: Arc<Mutex<Vec<String>>>,
    }

    impl FakeEditor {
        fn with_entries(
            entries: impl IntoIterator<
                Item = (
                    PathBuf,
                    Vec<crate::storage::file_system::ScopedDirectoryEntry>,
                ),
            >,
        ) -> Self {
            Self {
                entries: Arc::new(Mutex::new(entries.into_iter().collect())),
                ..Self::default()
            }
        }

        fn entries_at(
            &self,
            path: &Path,
        ) -> Result<
            Vec<crate::storage::file_system::ScopedDirectoryEntry>,
            ScopedDirectoryEditError<FakeEditorError>,
        > {
            self.entries
                .lock()
                .expect("候选目录项测试状态锁不应中毒")
                .get(path)
                .cloned()
                .ok_or_else(|| ScopedDirectoryEditError::NotFound {
                    path: path.to_path_buf(),
                })
        }

        fn entries_are_empty(&self) -> bool {
            self.entries
                .lock()
                .expect("候选目录项测试状态锁不应中毒")
                .is_empty()
        }

        fn record(&self, operation: &str, path: &ScopedDirectoryPath) {
            self.terminal_operations
                .lock()
                .expect("候选操作记录锁不应中毒")
                .push(format!("{operation}:{}", path.as_path().display()));
        }

        fn record_list(&self, path: &Path) {
            self.directory_lists
                .lock()
                .expect("候选目录列举记录锁不应中毒")
                .push(path.to_path_buf());
        }

        fn mutation_result(
            &self,
            path: &ScopedDirectoryPath,
        ) -> Result<(), ScopedDirectoryEditError<FakeEditorError>> {
            if self.fail_mutations {
                Err(ScopedDirectoryEditError::Failed {
                    path: path.as_path().to_path_buf(),
                    source: FakeEditorError,
                })
            } else {
                Ok(())
            }
        }

        fn create_directory_result(
            &self,
            path: &ScopedDirectoryPath,
        ) -> Result<(), ScopedDirectoryEditError<FakeEditorError>> {
            let mut entries = self.entries.lock().expect("候选目录项测试状态锁不应中毒");
            let mut current = PathBuf::new();
            for component in path.as_path().components() {
                let Component::Normal(name) = component else {
                    unreachable!("ScopedDirectoryPath 已建立普通相对路径段不变量")
                };
                let parent_entries = entries.entry(current.clone()).or_default();
                let created = if parent_entries.iter().any(|entry| entry.name() == name) {
                    false
                } else {
                    parent_entries.push(crate::storage::file_system::ScopedDirectoryEntry::new(
                        name.to_os_string(),
                        ScopedDirectoryEntryKind::Directory,
                    ));
                    parent_entries.sort_by(|left, right| left.name().cmp(right.name()));
                    true
                };
                current.push(name);
                entries.entry(current.clone()).or_default();
                if self.fail_mutations && created {
                    return Err(ScopedDirectoryEditError::Failed {
                        path: path.as_path().to_path_buf(),
                        source: FakeEditorError,
                    });
                }
            }
            drop(entries);
            self.mutation_result(path)
        }
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

        fn read_scoped_file(
            &self,
            _scope: &BoundScopedDirectory<Self::ScopeState>,
            path: ScopedDirectoryPath,
        ) -> impl std::future::Future<
            Output = Result<Vec<u8>, ScopedDirectoryEditError<Self::Error>>,
        > + Send {
            self.record("read", &path);
            std::future::ready(Ok(Vec::new()))
        }

        fn list_scoped_directory(
            &self,
            _scope: &BoundScopedDirectory<Self::ScopeState>,
            path: ScopedDirectoryPath,
        ) -> impl std::future::Future<
            Output = Result<
                Vec<crate::storage::file_system::ScopedDirectoryEntry>,
                ScopedDirectoryEditError<Self::Error>,
            >,
        > + Send {
            self.record_list(path.as_path());
            std::future::ready(if self.entries_are_empty() {
                Ok(Vec::new())
            } else {
                self.entries_at(path.as_path())
            })
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
            self.record_list(Path::new(""));
            std::future::ready(if self.entries_are_empty() {
                Ok(Vec::new())
            } else {
                self.entries_at(Path::new(""))
            })
        }

        fn create_scoped_directory(
            &self,
            _scope: &BoundScopedDirectory<Self::ScopeState>,
            path: ScopedDirectoryPath,
        ) -> impl std::future::Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send
        {
            self.record("create", &path);
            std::future::ready(self.create_directory_result(&path))
        }

        fn write_scoped_file(
            &self,
            _scope: &BoundScopedDirectory<Self::ScopeState>,
            path: ScopedDirectoryPath,
            _bytes: Vec<u8>,
        ) -> impl std::future::Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send
        {
            self.record("write", &path);
            std::future::ready(self.mutation_result(&path))
        }

        fn remove_scoped_path(
            &self,
            _scope: &BoundScopedDirectory<Self::ScopeState>,
            path: ScopedDirectoryPath,
        ) -> impl std::future::Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send
        {
            self.record("remove", &path);
            std::future::ready(self.mutation_result(&path))
        }
    }

    fn entry(
        name: &str,
        kind: ScopedDirectoryEntryKind,
    ) -> crate::storage::file_system::ScopedDirectoryEntry {
        crate::storage::file_system::ScopedDirectoryEntry::new(OsString::from(name), kind)
    }

    fn scoped_path(path: &str) -> ScopedDirectoryPath {
        ScopedDirectoryPath::new(PathBuf::from(path)).expect("测试输出路径应合法")
    }

    fn output_calls(
        layout: crate::rpg_maker::RpgMakerLayout,
        editor: Arc<FakeEditor>,
    ) -> ScopedLuaWriteBackHostCalls<FakeEditor> {
        ScopedLuaWriteBackHostCalls {
            editor,
            scope: Arc::new(BoundScopedDirectory::new(
                PathBuf::from("C:/projects/alice/.write_back-stage"),
                super::super::rpg_maker_output_scope(layout),
                (),
            )),
            layout_profile: RpgMakerWriteBackLayoutProfile::new(width(3), width(2), width(2)),
            rpg_maker_layout: layout,
            output_directories: Arc::default(),
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
            crate::rpg_maker::project::test_layout_profile(),
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

    fn program(path: &str) -> OwnedLuaProgram {
        OwnedLuaProgram::new(PathBuf::from(path), b"return nil".to_vec())
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
            .run(&project, &candidate, program("scripts/write.lua"))
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
            .run(&project, &candidate(&project), program("write.lua"))
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
            .run(&current_project, &candidate(&other), program("write.lua"))
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
            .run(&project, &candidate(&project), program("broken write.lua"))
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
        assert_send(service.run(&project, &candidate, program("write.lua")));
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
            .run(&project, &candidate(&project), program("write.lua"))
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
            FakeEditor {
                bind_fail: true,
                ..FakeEditor::default()
            },
        );
        let project = project("alice");

        let error = service
            .run(&project, &candidate(&project), program("write.lua"))
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
                super::super::rpg_maker_output_scope(crate::rpg_maker::RpgMakerLayout::MZ),
                (),
            )),
            layout_profile: RpgMakerWriteBackLayoutProfile::new(width(3), width(2), width(2)),
            rpg_maker_layout: crate::rpg_maker::RpgMakerLayout::MZ,
            output_directories: Arc::default(),
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
