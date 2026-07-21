//! 把尚未发布的完整写回候选交给共享可信 Lua Host。

use std::error::Error;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use crate::execution::OperationCompletion;
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
    ScopedDirectoryEditor, ScopedDirectoryEntryKind, ScopedDirectoryPath, StagedDirectory,
};
use crate::storage::scoped_path::{ExactPathCaseMismatch, resolve_exact_directory_entry};

use super::standard::{
    RpgMakerLayoutTextPair, RpgMakerTextLayoutOutcome, RpgMakerWriteBackLayoutRegion,
};
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
}

/// 把候选中的每个现存路径段解析为目录实际列出的逐字身份。
///
/// 缺失段仍交给具体编辑操作解释：读、列举和删除会沿用它们已有的 `not_found`
/// 语义，而写文件和建目录可以按各自契约创建新末段。仅大小写不同的现存别名则
/// 必须在任何操作之前失败，避免 Windows 把脚本请求静默重定向到另一个物理身份。
async fn resolve_exact_output_path<E>(
    editor: &E,
    scope: &BoundScopedDirectory<E::ScopeState>,
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
        let entries = match &exact_parent {
            Some(parent) => editor
                .list_scoped_directory(scope, parent.clone())
                .await
                .map_err(output_edit_error)?,
            None => editor
                .list_scoped_root(scope)
                .await
                .map_err(output_edit_error)?,
        };
        let parent = exact_parent
            .as_ref()
            .map_or_else(|| Path::new(""), ScopedDirectoryPath::as_path);
        let resolved = resolve_exact_directory_entry(
            parent,
            expected_name,
            entries.iter().map(|entry| parent.join(entry.name())),
        )
        .map_err(output_case_mismatch)?;
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
        Box::pin(async move {
            let path = path?;
            let path = resolve_exact_output_path(editor.as_ref(), scope.as_ref(), path).await?;
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
        Box::pin(async move {
            let path = path?;
            let path = resolve_exact_output_path(editor.as_ref(), scope.as_ref(), path).await?;
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
        let path = map_output_mutation_path(self.rpg_maker_layout, path);
        let editor = Arc::clone(&self.editor);
        let scope = Arc::clone(&self.scope);
        Box::pin(async move {
            let path = path?;
            let path = resolve_exact_output_path(editor.as_ref(), scope.as_ref(), path).await?;
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
        let path = map_output_mutation_path(self.rpg_maker_layout, path);
        let editor = Arc::clone(&self.editor);
        let scope = Arc::clone(&self.scope);
        Box::pin(async move {
            let path = path?;
            let path = resolve_exact_output_path(editor.as_ref(), scope.as_ref(), path).await?;
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
        let path = map_output_mutation_path(self.rpg_maker_layout, path);
        let editor = Arc::clone(&self.editor);
        let scope = Arc::clone(&self.scope);
        Box::pin(async move {
            let path = path?;
            let path = resolve_exact_output_path(editor.as_ref(), scope.as_ref(), path).await?;
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

fn output_case_mismatch(error: ExactPathCaseMismatch) -> TrustedLuaHostCallError {
    let message = error.to_string();
    TrustedLuaHostCallError::new(
        "filesystem",
        "case_mismatch",
        message,
        None,
        Some(Arc::new(error)),
    )
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
        use std::fs;
        use std::time::Duration;

        use crate::runtime::filesystem::{
            DirectoryPublisherConfig, ExclusiveFileLeaseConfig, SystemFileSystem,
            SystemFileSystemConfig, TreeBudget,
        };
        use crate::storage::file_system::{
            DirectoryPublishIntent, DirectorySourceMapping, DirectoryStageRequest,
            RecoverableDirectoryPublisher,
        };

        let workspace = tempfile::tempdir().expect("应建立 Windows 候选测试目录");
        let source = workspace.path().join("source");
        let source_data = source.join("data");
        let source_js = source.join("js");
        fs::create_dir_all(&source_data).expect("应建立来源 data");
        fs::create_dir_all(&source_js).expect("应建立来源 js");
        fs::write(source_data.join("Items.json"), b"original").expect("应建立大小写精确的来源文件");

        let file_system = SystemFileSystem::new(
            SystemFileSystemConfig::new(
                2,
                8,
                1024 * 1024,
                128,
                TreeBudget::new(128, 16, 1024 * 1024, 512 * 1024).expect("测试目录预算应合法"),
                ExclusiveFileLeaseConfig::new(Duration::from_secs(1)).expect("测试租约应合法"),
            )
            .expect("测试文件系统配置应合法"),
        )
        .expect("应建立生产文件系统根");
        let publisher = file_system.directory_publisher(
            DirectoryPublisherConfig::new(
                workspace.path().join("locks"),
                8,
                Duration::from_secs(1),
            )
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
        use std::fs;
        use std::time::Duration;

        use crate::runtime::filesystem::{
            DirectoryPublisherConfig, ExclusiveFileLeaseConfig, SystemFileSystem,
            SystemFileSystemConfig, TreeBudget,
        };
        use crate::storage::file_system::{
            DirectoryPublishIntent, DirectorySourceMapping, DirectoryStageRequest,
            RecoverableDirectoryPublisher,
        };

        let workspace = tempfile::tempdir().expect("应建立候选根修改测试目录");
        let source_data = workspace.path().join("source-data");
        let source_js = workspace.path().join("source-js");
        fs::create_dir_all(&source_data).expect("应建立来源 data");
        fs::create_dir_all(&source_js).expect("应建立来源 js");
        fs::write(source_data.join("Items.json"), b"{}").expect("应建立来源 data 文件");
        fs::write(source_js.join("plugins.js"), b"var $plugins = [];").expect("应建立来源 js 文件");

        let file_system = SystemFileSystem::new(
            SystemFileSystemConfig::new(
                2,
                8,
                1024 * 1024,
                128,
                TreeBudget::new(128, 16, 1024 * 1024, 512 * 1024).expect("测试目录预算应合法"),
                ExclusiveFileLeaseConfig::new(Duration::from_secs(1)).expect("测试租约应合法"),
            )
            .expect("测试文件系统配置应合法"),
        )
        .expect("应建立生产文件系统根");
        let publisher = file_system.directory_publisher(
            DirectoryPublisherConfig::new(
                workspace.path().join("locks"),
                8,
                Duration::from_secs(1),
            )
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
        entries: Arc<BTreeMap<PathBuf, Vec<crate::storage::file_system::ScopedDirectoryEntry>>>,
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
                entries: Arc::new(entries.into_iter().collect()),
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
                .get(path)
                .cloned()
                .ok_or_else(|| ScopedDirectoryEditError::NotFound {
                    path: path.to_path_buf(),
                })
        }

        fn record(&self, operation: &str, path: &ScopedDirectoryPath) {
            self.terminal_operations
                .lock()
                .expect("候选操作记录锁不应中毒")
                .push(format!("{operation}:{}", path.as_path().display()));
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
            std::future::ready(if self.entries.is_empty() {
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
            std::future::ready(if self.entries.is_empty() {
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
            std::future::ready(Ok(()))
        }

        fn write_scoped_file(
            &self,
            _scope: &BoundScopedDirectory<Self::ScopeState>,
            path: ScopedDirectoryPath,
            _bytes: Vec<u8>,
        ) -> impl std::future::Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send
        {
            self.record("write", &path);
            std::future::ready(Ok(()))
        }

        fn remove_scoped_path(
            &self,
            _scope: &BoundScopedDirectory<Self::ScopeState>,
            path: ScopedDirectoryPath,
        ) -> impl std::future::Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send
        {
            self.record("remove", &path);
            std::future::ready(Ok(()))
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
