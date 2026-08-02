use std::error::Error;
#[cfg(test)]
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::asset::RpgMakerAssetOwner;
use super::project::{MaxFullwidthChars, RpgMakerWriteBackLayoutProfile};
use crate::execution::{CooperativeCancellation, OperationCompletion};
use crate::language::{LanguageId, LanguagePair};
use crate::progress::{NoopProgressObserver, ProgressObserver, ProgressSnapshot};
use crate::project_lease::{ProjectCommandLeaseError, ProjectCommandLeaseProvider};
use crate::project_name::ProjectName;
use crate::rpg_maker::RpgMakerLayout;
use crate::rpg_maker::project_database::{
    NewProject, ProjectDatabaseCreator, ProjectDatabaseStateReconciler, ProjectWorkspaceLayout,
    SourceSnapshotFingerprint,
};
use crate::storage::file_system::{
    BoundScopedDirectory, DirectChildDirectoryEnsurer, DirectoryDiscardError, DirectoryEntry,
    DirectoryEntryKind, DirectoryLister, DirectoryPrepareError, DirectoryPublishError,
    DirectoryPublishIntent, DirectoryRecoveryError, DirectorySourceMapping, DirectoryStageRequest,
    DirectoryStageRequestError, DirectoryTreeFingerprintError, DirectoryTreeFingerprintRequest,
    DirectoryTreeFingerprinter, DirectoryTreeRoot, ExistingDirectoryResolver, FileReader,
    ListDirectoryError, ReadFileError, RecoverableDirectoryPublisher, ResolveDirectoryError,
    ScopedDirectoryBindError, ScopedDirectoryEditError, ScopedDirectoryEditor, ScopedDirectoryPath,
    ScopedDirectoryScope, StagedDirectory,
};
use crate::storage::scoped_path::ScopedDirectoryPathError;
use crate::storage::sqlite::{SnapshotDatabaseError, SqliteDatabaseSnapshotter};

/// 初始化 RPG Maker 游戏所需的输入。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitInput {
    pub name: ProjectName,
    pub game_root: PathBuf,
    pub source_language: Option<LanguageId>,
    pub target_language: Option<LanguageId>,
    pub dialogue_max_fullwidth_chars: Option<MaxFullwidthChars>,
    pub scrolling_text_max_fullwidth_chars: Option<MaxFullwidthChars>,
    pub help_description_max_fullwidth_chars: Option<MaxFullwidthChars>,
}

/// Init 更新后可能需要重新提取的 RPG Maker 资产 owner。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitStaleOwner {
    Builtin,
    Rules,
}

/// 本次 Init 把项目工作区收敛到的终态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InitOutcome {
    Created,
    Unchanged,
    Updated { stale_owners: Vec<InitStaleOwner> },
}

/// 初始化成功后交还给 CLI 的项目与收敛结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitOutput {
    pub name: ProjectName,
    pub outcome: InitOutcome,
}

/// Init 收敛过程中能够被真实观测、但没有稳定数量分母的阶段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InitProgressPhase {
    CheckingProject,
    ScanningSource,
    PreparingCandidate,
    UpdatingDatabase,
    Publishing,
}

/// 完成一次工作区状态收敛所需的全部受信事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectWorkspaceConvergenceRequest {
    source_game_root: PathBuf,
    name: ProjectName,
    source_language: Option<LanguageId>,
    target_language: Option<LanguageId>,
    dialogue_max_fullwidth_chars: Option<MaxFullwidthChars>,
    scrolling_text_max_fullwidth_chars: Option<MaxFullwidthChars>,
    help_description_max_fullwidth_chars: Option<MaxFullwidthChars>,
}

impl ProjectWorkspaceConvergenceRequest {
    pub(crate) fn new(
        source_game_root: PathBuf,
        name: ProjectName,
        source_language: Option<LanguageId>,
        target_language: Option<LanguageId>,
        dialogue_max_fullwidth_chars: Option<MaxFullwidthChars>,
        scrolling_text_max_fullwidth_chars: Option<MaxFullwidthChars>,
        help_description_max_fullwidth_chars: Option<MaxFullwidthChars>,
    ) -> Self {
        Self {
            source_game_root,
            name,
            source_language,
            target_language,
            dialogue_max_fullwidth_chars,
            scrolling_text_max_fullwidth_chars,
            help_description_max_fullwidth_chars,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ResolvedProjectSettings {
    language_pair: LanguagePair,
    layout_profile: RpgMakerWriteBackLayoutProfile,
}

/// 首次建立项目时必须由用户明确给出的设置。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MissingInitialProjectSetting {
    SourceLanguage,
    TargetLanguage,
    DialogueMaxFullwidthChars,
    ScrollingTextMaxFullwidthChars,
    HelpDescriptionMaxFullwidthChars,
}

impl MissingInitialProjectSetting {
    const fn cli_flag(self) -> &'static str {
        match self {
            Self::SourceLanguage => "--source-language",
            Self::TargetLanguage => "--target-language",
            Self::DialogueMaxFullwidthChars => "--dialogue-max-fullwidth-chars",
            Self::ScrollingTextMaxFullwidthChars => "--scrolling-text-max-fullwidth-chars",
            Self::HelpDescriptionMaxFullwidthChars => "--help-description-max-fullwidth-chars",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProjectWorkspaceConvergence {
    Created,
    Unchanged,
    Updated {
        stale_owners: Vec<RpgMakerAssetOwner>,
    },
}

/// 把项目工作区收敛到本次请求的唯一当前状态。
pub(crate) trait ProjectWorkspaceConverger: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn converge(
        &self,
        request: ProjectWorkspaceConvergenceRequest,
    ) -> impl std::future::Future<
        Output = Result<OperationCompletion<ProjectWorkspaceConvergence>, Self::Error>,
    > + Send;
}

/// 项目工作区收敛服务；持有项目租约直到候选被发布或明确丢弃。
pub(crate) struct ProjectWorkspaceConvergenceService<D, S, R, F, A> {
    projects_root: PathBuf,
    rpg_maker_layout: RpgMakerLayout,
    database_creator: D,
    database_snapshotter: S,
    database_reconciler: R,
    file_system: F,
    directories: A,
    cancellation: CooperativeCancellation,
    progress: Arc<dyn ProgressObserver<InitProgressPhase>>,
}

impl<D, S, R, F, A> ProjectWorkspaceConvergenceService<D, S, R, F, A> {
    #[allow(
        clippy::too_many_arguments,
        reason = "每项参数都是本职责的直接依赖或构造事实"
    )]
    pub(crate) fn new(
        projects_root: PathBuf,
        rpg_maker_layout: RpgMakerLayout,
        database_creator: D,
        database_snapshotter: S,
        database_reconciler: R,
        file_system: F,
        directories: A,
        cancellation: CooperativeCancellation,
    ) -> Self {
        Self {
            projects_root,
            rpg_maker_layout,
            database_creator,
            database_snapshotter,
            database_reconciler,
            file_system,
            directories,
            cancellation,
            progress: Arc::new(NoopProgressObserver),
        }
    }

    /// 为本次 Init 绑定同步、不可失败的业务进度观察者。
    pub(crate) fn with_progress<Q>(mut self, progress: Q) -> Self
    where
        Q: ProgressObserver<InitProgressPhase> + 'static,
    {
        self.progress = Arc::new(progress);
        self
    }

    fn observe(&self, phase: InitProgressPhase) {
        self.progress
            .observe(ProgressSnapshot::indeterminate(phase));
    }
}

impl<D, S, R, F, A> ProjectWorkspaceConverger for ProjectWorkspaceConvergenceService<D, S, R, F, A>
where
    D: ProjectDatabaseCreator,
    S: SqliteDatabaseSnapshotter,
    R: ProjectDatabaseStateReconciler,
    F: ExistingDirectoryResolver
        + DirectChildDirectoryEnsurer<Error = <F as ExistingDirectoryResolver>::Error>
        + DirectoryLister<Error = <F as ExistingDirectoryResolver>::Error>
        + FileReader<Error = <F as ExistingDirectoryResolver>::Error>
        + DirectoryTreeFingerprinter,
    A: RecoverableDirectoryPublisher
        + ScopedDirectoryEditor<
            CandidateState = <A as RecoverableDirectoryPublisher>::StagingState,
            Error = <A as RecoverableDirectoryPublisher>::Error,
        >,
{
    type Error = ProjectWorkspaceConvergenceError<
        D::Error,
        S::Error,
        R::InspectionError,
        R::ReconciliationError,
        <F as ExistingDirectoryResolver>::Error,
        <F as DirectoryTreeFingerprinter>::Error,
        <A as RecoverableDirectoryPublisher>::Error,
    >;

    async fn converge(
        &self,
        request: ProjectWorkspaceConvergenceRequest,
    ) -> Result<OperationCompletion<ProjectWorkspaceConvergence>, Self::Error> {
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        self.observe(InitProgressPhase::CheckingProject);
        let final_layout = ProjectWorkspaceLayout::for_project(
            &self.projects_root,
            self.rpg_maker_layout,
            &request.name,
        );
        let engine_workspace_root = final_layout
            .workspace_root()
            .parent()
            .expect("固定项目工作区必有引擎父目录")
            .to_path_buf();
        match self
            .file_system
            .resolve_existing_directory(engine_workspace_root)
            .await
        {
            Ok(_) => {
                let _ = self
                    .directories
                    .recover(final_layout.workspace_root().to_path_buf())
                    .await
                    .map_err(ProjectWorkspaceConvergenceError::Recover)?;
            }
            Err(ResolveDirectoryError::NotFound { .. }) => {}
            Err(error) => {
                return Err(ProjectWorkspaceConvergenceError::ObserveEngineWorkspaceRoot(error));
            }
        }
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        let target_exists = match self
            .file_system
            .resolve_existing_directory(final_layout.workspace_root().to_path_buf())
            .await
        {
            Ok(_) => true,
            Err(ResolveDirectoryError::NotFound { .. }) => false,
            Err(error) => return Err(ProjectWorkspaceConvergenceError::WorkspaceRoot(error)),
        };

        let current_state = if target_exists {
            Some(
                self.database_reconciler
                    .inspect(
                        final_layout.database_path().to_path_buf(),
                        request.name.clone(),
                    )
                    .await
                    .map_err(ProjectWorkspaceConvergenceError::InspectExistingDatabase)?,
            )
        } else {
            None
        };
        let settings = resolve_project_settings(&request, current_state.as_ref())
            .map_err(ProjectWorkspaceConvergenceError::MissingInitialSettings)?;
        self.observe(InitProgressPhase::ScanningSource);
        let source_game_root = self
            .file_system
            .resolve_existing_directory(request.source_game_root)
            .await
            .map_err(ProjectWorkspaceConvergenceError::SourceGameRoot)?;
        let valid_game_layout = validate_game_source_layout(
            &self.file_system,
            self.rpg_maker_layout,
            &source_game_root,
        )
        .await
        .map_err(ProjectWorkspaceConvergenceError::ObserveGameLayout)?;
        if !valid_game_layout {
            return Err(ProjectWorkspaceConvergenceError::InvalidGameLayout {
                game_root: source_game_root,
                engine: self.rpg_maker_layout.engine().storage_name(),
                data_relative: self.rpg_maker_layout.data_relative(),
                js_relative: self.rpg_maker_layout.js_relative(),
                core_script: self.rpg_maker_layout.core_script(),
            });
        }

        // 这两个名称一旦存在就必须是目录。它们承载运行历史和人工补译材料，
        // 即使项目其余状态完全一致，也不能把同名普通文件当成“无需保留”。
        let preserved_directories = if target_exists {
            observe_preserved_observability_directories(
                &self.file_system,
                final_layout.workspace_root(),
            )
            .await
            .map_err(ProjectWorkspaceConvergenceError::ObservePreservedDirectory)?
        } else {
            Vec::new()
        };

        if let Some(state) = current_state.as_ref() {
            let structure_matches =
                observe_required_workspace_structure(&self.file_system, &final_layout)
                    .await
                    .map_err(ProjectWorkspaceConvergenceError::ObserveWorkspaceStructure)?;
            let source_matches = if structure_matches {
                match fingerprint_source(&self.file_system, &final_layout, true)
                    .await
                    .map_err(ProjectWorkspaceConvergenceError::ObserveExistingSource)?
                {
                    Some(actual) => actual == state.source_snapshot_fingerprint(),
                    None => false,
                }
            } else {
                false
            };
            let workspace_complete = structure_matches && source_matches;
            let settings_match = state.language_pair() == &settings.language_pair
                && state.layout_profile() == &settings.layout_profile;
            if workspace_complete && settings_match {
                let input_fingerprint = fingerprint_game_source(
                    &self.file_system,
                    self.rpg_maker_layout,
                    &source_game_root,
                )
                .await
                .map_err(ProjectWorkspaceConvergenceError::ObserveInputSource)?;
                if self.cancellation.is_requested() {
                    return Ok(OperationCompletion::Cancelled);
                }
                if input_fingerprint == state.source_snapshot_fingerprint() {
                    return Ok(OperationCompletion::Completed(
                        ProjectWorkspaceConvergence::Unchanged,
                    ));
                }
            }
        }

        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        self.observe(InitProgressPhase::PreparingCandidate);
        self.file_system
            .ensure_direct_child_directory(
                self.projects_root.clone(),
                OsString::from(self.rpg_maker_layout.engine().storage_name()),
            )
            .await
            .map_err(ProjectWorkspaceConvergenceError::EngineWorkspaceRoot)?;
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        let publish_intent = if target_exists {
            DirectoryPublishIntent::ReplaceExisting
        } else {
            DirectoryPublishIntent::CreateNew
        };
        // 现存工作区的 logs/ 与 task-records/ 是既有运行历史与人工补译审计材料，
        // 重建发布必须原样带入候选。
        let mut empty_directories = vec![
            self.rpg_maker_layout.write_back_data_relative(),
            self.rpg_maker_layout.write_back_js_relative(),
        ];
        empty_directories.extend(preserved_directories.iter().map(PathBuf::from));
        let stage_request = DirectoryStageRequest::new(
            final_layout.workspace_root().to_path_buf(),
            publish_intent,
            vec![
                DirectorySourceMapping::new(
                    source_game_root.join(self.rpg_maker_layout.data_relative()),
                    self.rpg_maker_layout.source_data_relative(),
                )?,
                DirectorySourceMapping::new(
                    source_game_root.join(self.rpg_maker_layout.js_relative()),
                    self.rpg_maker_layout.source_js_relative(),
                )?,
            ],
            Vec::new(),
            empty_directories,
        )?;
        let staged = self
            .directories
            .prepare(stage_request)
            .await
            .map_err(ProjectWorkspaceConvergenceError::Prepare)?;
        let staged_layout = ProjectWorkspaceLayout::from_workspace_root(
            staged.staging_root().to_path_buf(),
            self.rpg_maker_layout,
        );

        if self.cancellation.is_requested() {
            return discard_cancelled_candidate(&self.directories, staged).await;
        }
        if !preserved_directories.is_empty() {
            // bind 只借用 candidate 建立范围令牌;后续复制只携带 BoundScopedDirectory,
            // 不跨 await 持有 StagingState 借用。
            let scope = ScopedDirectoryScope::new(preserved_directories.iter().map(OsString::from))
                .expect("保留目录名是固定的合法范围根");
            let bound = match self.directories.bind_scoped_directory(&staged, scope).await {
                Ok(bound) => bound,
                Err(source) => {
                    let discard = self.directories.discard(staged).await.err();
                    return Err(ProjectWorkspaceConvergenceError::PreserveObservability {
                        failure: PreserveObservabilityFailure::Bind(source),
                        discard,
                    });
                }
            };
            if let Err(failure) = preserve_observability_directories(
                &self.file_system,
                &self.directories,
                final_layout.workspace_root(),
                &bound,
                &preserved_directories,
            )
            .await
            {
                let discard = self.directories.discard(staged).await.err();
                return Err(ProjectWorkspaceConvergenceError::PreserveObservability {
                    failure,
                    discard,
                });
            }
        }
        let candidate_fingerprint =
            match fingerprint_source(&self.file_system, &staged_layout, false).await {
                Ok(Some(fingerprint)) => fingerprint,
                Ok(None) => unreachable!("候选来源缺失必须由指纹根返回错误"),
                Err(source) => {
                    return Err(discard_candidate_failure(
                        &self.directories,
                        staged,
                        ProjectWorkspaceCandidateFailure::FingerprintCandidate(source),
                    )
                    .await);
                }
            };
        let requested_project = NewProject::new(
            request.name,
            settings.language_pair,
            candidate_fingerprint,
            settings.layout_profile,
        );

        self.observe(InitProgressPhase::UpdatingDatabase);
        if target_exists {
            if let Err(source) = self
                .database_snapshotter
                .snapshot_database(
                    final_layout.database_path().to_path_buf(),
                    staged_layout.database_path().to_path_buf(),
                )
                .await
            {
                return Err(discard_candidate_failure(
                    &self.directories,
                    staged,
                    ProjectWorkspaceCandidateFailure::SnapshotDatabase(source),
                )
                .await);
            }
        } else if let Err(source) = self
            .database_creator
            .create(
                staged_layout.database_path().to_path_buf(),
                requested_project.clone(),
            )
            .await
        {
            return Err(discard_candidate_failure(
                &self.directories,
                staged,
                ProjectWorkspaceCandidateFailure::CreateDatabase(source),
            )
            .await);
        }

        let reconciliation = match self
            .database_reconciler
            .reconcile(
                staged_layout.database_path().to_path_buf(),
                requested_project,
            )
            .await
        {
            Ok(value) => value,
            Err(source) => {
                return Err(discard_candidate_failure(
                    &self.directories,
                    staged,
                    ProjectWorkspaceCandidateFailure::ReconcileDatabase(source),
                )
                .await);
            }
        };

        if self.cancellation.is_requested() {
            return discard_cancelled_candidate(&self.directories, staged).await;
        }
        self.observe(InitProgressPhase::Publishing);
        self.directories
            .publish(staged)
            .await
            .map_err(ProjectWorkspaceConvergenceError::Publish)?;
        if current_state.is_some() {
            Ok(OperationCompletion::Completed(
                ProjectWorkspaceConvergence::Updated {
                    stale_owners: reconciliation.stale_owners(),
                },
            ))
        } else {
            Ok(OperationCompletion::Completed(
                ProjectWorkspaceConvergence::Created,
            ))
        }
    }
}

fn resolve_project_settings(
    request: &ProjectWorkspaceConvergenceRequest,
    current: Option<&crate::rpg_maker::project_database::ProjectDatabaseState>,
) -> Result<ResolvedProjectSettings, Vec<MissingInitialProjectSetting>> {
    let mut missing = Vec::new();

    let source_language = request
        .source_language
        .clone()
        .or_else(|| current.map(|state| state.source_language().to_owned()));
    if source_language.is_none() {
        missing.push(MissingInitialProjectSetting::SourceLanguage);
    }
    let target_language = request
        .target_language
        .clone()
        .or_else(|| current.map(|state| state.target_language().to_owned()));
    if target_language.is_none() {
        missing.push(MissingInitialProjectSetting::TargetLanguage);
    }
    let dialogue_body = request
        .dialogue_max_fullwidth_chars
        .or_else(|| current.map(|state| state.layout_profile().dialogue_body()));
    if dialogue_body.is_none() {
        missing.push(MissingInitialProjectSetting::DialogueMaxFullwidthChars);
    }
    let scrolling_text = request
        .scrolling_text_max_fullwidth_chars
        .or_else(|| current.map(|state| state.layout_profile().scrolling_text()));
    if scrolling_text.is_none() {
        missing.push(MissingInitialProjectSetting::ScrollingTextMaxFullwidthChars);
    }
    let help_description = request
        .help_description_max_fullwidth_chars
        .or_else(|| current.map(|state| state.layout_profile().help_description()));
    if help_description.is_none() {
        missing.push(MissingInitialProjectSetting::HelpDescriptionMaxFullwidthChars);
    }

    if !missing.is_empty() {
        return Err(missing);
    }

    Ok(ResolvedProjectSettings {
        language_pair: LanguagePair::new(
            source_language.expect("已确认源语言存在"),
            target_language.expect("已确认目标语言存在"),
        ),
        layout_profile: RpgMakerWriteBackLayoutProfile::new(
            dialogue_body.expect("已确认对话宽度存在"),
            scrolling_text.expect("已确认滚动文本宽度存在"),
            help_description.expect("已确认帮助描述宽度存在"),
        ),
    })
}

async fn observe_required_workspace_structure<F>(
    file_system: &F,
    layout: &ProjectWorkspaceLayout,
) -> Result<bool, ListDirectoryError<F::Error>>
where
    F: DirectoryLister,
{
    let data_and_js = vec![
        ("data", DirectoryEntryKind::Directory),
        ("js", DirectoryEntryKind::Directory),
    ];
    let mut expectations = vec![(
        layout.workspace_root().to_path_buf(),
        vec![
            ("project.db", DirectoryEntryKind::RegularFile),
            ("source", DirectoryEntryKind::Directory),
            ("write_back", DirectoryEntryKind::Directory),
        ],
    )];
    if let Some(content_directory) = layout.rpg_maker_layout().content_directory() {
        for root in [layout.source_root(), layout.write_back_root()] {
            expectations.push((
                root.to_path_buf(),
                vec![(content_directory, DirectoryEntryKind::Directory)],
            ));
            expectations.push((root.join(content_directory), data_and_js.clone()));
        }
    } else {
        expectations.extend([
            (layout.source_root().to_path_buf(), data_and_js.clone()),
            (layout.write_back_root().to_path_buf(), data_and_js),
        ]);
    }

    for (root, required_children) in expectations {
        let children = match file_system.list_directory(root.clone()).await {
            Ok(children) => children,
            Err(ListDirectoryError::NotFound { .. } | ListDirectoryError::NotDirectory { .. }) => {
                return Ok(false);
            }
            Err(error @ ListDirectoryError::Io { .. }) => return Err(error),
        };
        if !has_required_child_names(&children, &required_children) {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn fingerprint_game_source<F>(
    file_system: &F,
    layout: RpgMakerLayout,
    game_root: &std::path::Path,
) -> Result<SourceSnapshotFingerprint, DirectoryTreeFingerprintError<F::Error>>
where
    F: DirectoryTreeFingerprinter,
{
    fingerprint_roots(
        file_system,
        game_root.join(layout.data_relative()),
        game_root.join(layout.js_relative()),
    )
    .await
}

async fn validate_game_source_layout<F>(
    file_system: &F,
    layout: RpgMakerLayout,
    game_root: &std::path::Path,
) -> Result<bool, ListDirectoryError<F::Error>>
where
    F: DirectoryLister,
{
    let content_root = layout.game_content_root(game_root);
    let children = match file_system.list_directory(content_root).await {
        Ok(children) => children,
        Err(ListDirectoryError::NotFound { .. } | ListDirectoryError::NotDirectory { .. }) => {
            return Ok(false);
        }
        Err(error @ ListDirectoryError::Io { .. }) => return Err(error),
    };
    if count_child(&children, ("data", DirectoryEntryKind::Directory)) != 1
        || count_child(&children, ("js", DirectoryEntryKind::Directory)) != 1
    {
        return Ok(false);
    }

    let js_root = game_root.join(layout.js_relative());
    let js_children = match file_system.list_directory(js_root).await {
        Ok(children) => children,
        Err(ListDirectoryError::NotFound { .. } | ListDirectoryError::NotDirectory { .. }) => {
            return Ok(false);
        }
        Err(error @ ListDirectoryError::Io { .. }) => return Err(error),
    };
    Ok(count_child(
        &js_children,
        (layout.core_script(), DirectoryEntryKind::RegularFile),
    ) == 1)
}

fn has_required_child_names(
    children: &[DirectoryEntry],
    required: &[(&str, DirectoryEntryKind)],
) -> bool {
    required
        .iter()
        .all(|expected| count_child(children, *expected) == 1)
}

fn count_child(children: &[DirectoryEntry], expected: (&str, DirectoryEntryKind)) -> usize {
    children
        .iter()
        .filter(|child| {
            child
                .resolved_path()
                .file_name()
                .is_some_and(|name| name == expected.0)
                && child.kind() == expected.1
        })
        .count()
}

async fn fingerprint_source<F>(
    file_system: &F,
    layout: &ProjectWorkspaceLayout,
    absence_is_repairable: bool,
) -> Result<Option<SourceSnapshotFingerprint>, DirectoryTreeFingerprintError<F::Error>>
where
    F: DirectoryTreeFingerprinter,
{
    match fingerprint_roots(
        file_system,
        layout.source_data().to_path_buf(),
        layout.source_js().to_path_buf(),
    )
    .await
    {
        Ok(value) => Ok(Some(value)),
        Err(
            DirectoryTreeFingerprintError::NotFound { .. }
            | DirectoryTreeFingerprintError::NotDirectory { .. },
        ) if absence_is_repairable => Ok(None),
        Err(error) => Err(error),
    }
}

async fn fingerprint_roots<F>(
    file_system: &F,
    data_root: PathBuf,
    js_root: PathBuf,
) -> Result<SourceSnapshotFingerprint, DirectoryTreeFingerprintError<F::Error>>
where
    F: DirectoryTreeFingerprinter,
{
    let request = DirectoryTreeFingerprintRequest::new(vec![
        DirectoryTreeRoot::new(data_root, PathBuf::from("data")).expect("固定 data 逻辑根必须合法"),
        DirectoryTreeRoot::new(js_root, PathBuf::from("js")).expect("固定 js 逻辑根必须合法"),
    ])
    .expect("固定 data 与 js 逻辑根必须互不重叠");
    file_system
        .fingerprint_directory_tree(request)
        .await
        .map(|value| SourceSnapshotFingerprint::from_bytes(value.into_bytes()))
}

/// 工作区重建时按当前契约保留的非权威可观测性目录。
const PRESERVED_OBSERVABILITY_DIRECTORIES: [&str; 2] = ["logs", "task-records"];

async fn observe_preserved_observability_directories<F>(
    file_system: &F,
    workspace_root: &Path,
) -> Result<Vec<&'static str>, ResolveDirectoryError<F::Error>>
where
    F: ExistingDirectoryResolver,
{
    let mut preserved = Vec::new();
    for name in PRESERVED_OBSERVABILITY_DIRECTORIES {
        match file_system
            .resolve_existing_directory(workspace_root.join(name))
            .await
        {
            Ok(_) => preserved.push(name),
            Err(ResolveDirectoryError::NotFound { .. }) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(preserved)
}

/// 把旧工作区的可观测性目录逐文件搬入已准备的候选。
///
/// 来源树在项目租约下不会并发变化;显式栈遍历不建立深度上限。
async fn preserve_observability_directories<F, A>(
    file_system: &F,
    directories: &A,
    workspace_root: &Path,
    bound: &BoundScopedDirectory<<A as ScopedDirectoryEditor>::ScopeState>,
    preserved: &[&'static str],
) -> Result<
    (),
    PreserveObservabilityFailure<
        <F as ExistingDirectoryResolver>::Error,
        <A as ScopedDirectoryEditor>::Error,
    >,
>
where
    F: ExistingDirectoryResolver
        + DirectoryLister<Error = <F as ExistingDirectoryResolver>::Error>
        + FileReader<Error = <F as ExistingDirectoryResolver>::Error>,
    A: ScopedDirectoryEditor,
{
    let mut pending = preserved
        .iter()
        .map(|name| (workspace_root.join(name), PathBuf::from(name)))
        .collect::<Vec<_>>();
    while let Some((absolute, relative)) = pending.pop() {
        let entries = file_system
            .list_directory(absolute.clone())
            .await
            .map_err(|source| PreserveObservabilityFailure::List {
                path: absolute,
                source,
            })?;
        for entry in entries {
            let name = entry
                .resolved_path()
                .file_name()
                .expect("DirectoryLister 返回的直接子项必须包含名称");
            let child_relative = relative.join(name);
            let scoped = ScopedDirectoryPath::from_internal_path(child_relative.clone()).map_err(
                |source| PreserveObservabilityFailure::InvalidCandidatePath {
                    path: entry.resolved_path().to_path_buf(),
                    source,
                },
            )?;
            match entry.kind() {
                DirectoryEntryKind::Directory => {
                    directories
                        .create_scoped_directory(bound, scoped)
                        .await
                        .map_err(|source| PreserveObservabilityFailure::Edit {
                            path: child_relative.clone(),
                            source,
                        })?;
                    pending.push((entry.resolved_path().to_path_buf(), child_relative));
                }
                DirectoryEntryKind::RegularFile => {
                    let file = file_system
                        .read_file(entry.resolved_path().to_path_buf())
                        .await
                        .map_err(|source| PreserveObservabilityFailure::Read {
                            path: entry.resolved_path().to_path_buf(),
                            source,
                        })?;
                    directories
                        .write_scoped_file(bound, scoped, file.into_bytes())
                        .await
                        .map_err(|source| PreserveObservabilityFailure::Edit {
                            path: child_relative,
                            source,
                        })?;
                }
            }
        }
    }
    Ok(())
}

async fn discard_candidate_failure<A, D, S, I, R, E, P>(
    directories: &A,
    staged: StagedDirectory<A::StagingState>,
    failure: ProjectWorkspaceCandidateFailure<D, S, R, P>,
) -> ProjectWorkspaceConvergenceError<D, S, I, R, E, P, A::Error>
where
    A: RecoverableDirectoryPublisher,
{
    let discard = directories.discard(staged).await.err();
    ProjectWorkspaceConvergenceError::CandidateFailure { failure, discard }
}

async fn discard_cancelled_candidate<A, D, S, I, R, E, P>(
    directories: &A,
    staged: StagedDirectory<A::StagingState>,
) -> Result<
    OperationCompletion<ProjectWorkspaceConvergence>,
    ProjectWorkspaceConvergenceError<D, S, I, R, E, P, A::Error>,
>
where
    A: RecoverableDirectoryPublisher,
{
    match directories.discard(staged).await {
        Ok(()) => Ok(OperationCompletion::Cancelled),
        Err(source) => Err(ProjectWorkspaceConvergenceError::CancellationCleanup(
            source,
        )),
    }
}

#[derive(Debug)]
pub(crate) enum ProjectWorkspaceCandidateFailure<D, S, R, P> {
    FingerprintCandidate(DirectoryTreeFingerprintError<P>),
    CreateDatabase(D),
    SnapshotDatabase(SnapshotDatabaseError<S>),
    ReconcileDatabase(R),
}

/// 工作区重建时无法把现存 `logs/`、`task-records/` 搬入候选。
///
/// 这些目录是既有运行历史与人工补译审计材料;重建路径必须原样保留它们,
/// 不得随 ReplaceExisting 静默丢弃。
#[derive(Debug)]
pub(crate) enum PreserveObservabilityFailure<E, A> {
    Bind(ScopedDirectoryBindError<A>),
    List {
        path: PathBuf,
        source: ListDirectoryError<E>,
    },
    Read {
        path: PathBuf,
        source: ReadFileError<E>,
    },
    InvalidCandidatePath {
        path: PathBuf,
        source: ScopedDirectoryPathError,
    },
    Edit {
        path: PathBuf,
        source: ScopedDirectoryEditError<A>,
    },
}

impl<E, A> fmt::Display for PreserveObservabilityFailure<E, A>
where
    E: fmt::Display,
    A: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bind(error) => write!(formatter, "无法绑定候选保留范围：{error}"),
            Self::List { path, source } => {
                write!(formatter, "无法列举保留目录 {}：{source}", path.display())
            }
            Self::Read { path, source } => {
                write!(formatter, "无法读取保留文件 {}：{source}", path.display())
            }
            Self::InvalidCandidatePath { path, source } => write!(
                formatter,
                "保留条目 {} 无法映射为候选路径：{source}",
                path.display()
            ),
            Self::Edit { path, source } => {
                write!(
                    formatter,
                    "无法写入候选保留条目 {}：{source}",
                    path.display()
                )
            }
        }
    }
}

impl<E, A> Error for PreserveObservabilityFailure<E, A>
where
    E: Error + 'static,
    A: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Bind(source) => Some(source),
            Self::List { source, .. } => Some(source),
            Self::Read { source, .. } => Some(source),
            Self::InvalidCandidatePath { source, .. } => Some(source),
            Self::Edit { source, .. } => Some(source),
        }
    }
}

#[derive(Debug)]
pub(crate) enum ProjectWorkspaceConvergenceError<D, S, I, R, E, P, A> {
    SourceGameRoot(ResolveDirectoryError<E>),
    ObserveGameLayout(ListDirectoryError<E>),
    InvalidGameLayout {
        game_root: PathBuf,
        engine: &'static str,
        data_relative: PathBuf,
        js_relative: PathBuf,
        core_script: &'static str,
    },
    EngineWorkspaceRoot(E),
    ObserveEngineWorkspaceRoot(ResolveDirectoryError<E>),
    WorkspaceRoot(ResolveDirectoryError<E>),
    InspectExistingDatabase(I),
    MissingInitialSettings(Vec<MissingInitialProjectSetting>),
    ObserveWorkspaceStructure(ListDirectoryError<E>),
    ObserveExistingSource(DirectoryTreeFingerprintError<P>),
    ObserveInputSource(DirectoryTreeFingerprintError<P>),
    InvalidStageRequest(DirectoryStageRequestError),
    Recover(DirectoryRecoveryError<A>),
    Prepare(DirectoryPrepareError<A>),
    ObservePreservedDirectory(ResolveDirectoryError<E>),
    PreserveObservability {
        failure: PreserveObservabilityFailure<E, A>,
        discard: Option<DirectoryDiscardError<A>>,
    },
    CandidateFailure {
        failure: ProjectWorkspaceCandidateFailure<D, S, R, P>,
        discard: Option<DirectoryDiscardError<A>>,
    },
    CancellationCleanup(DirectoryDiscardError<A>),
    Publish(DirectoryPublishError<A>),
}

/// 工作区收敛失败已经造成的最高层用户影响。
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectWorkspaceConvergenceFailureImpact {
    ConfigurationOrInput,
    ProjectState,
    StateAppliedButFinalizationFailed,
    RecoveryRequired,
    OutcomeUnknown,
    Internal,
}

#[cfg(test)]
impl<D, S, I, R, E, P, A> ProjectWorkspaceConvergenceError<D, S, I, R, E, P, A> {
    /// 将工作区内部阶段和目录发布终态归并为命令边界可以准确呈现的用户影响。
    pub(crate) fn failure_impact(&self) -> ProjectWorkspaceConvergenceFailureImpact {
        use ProjectWorkspaceConvergenceFailureImpact as Impact;

        match self {
            Self::SourceGameRoot(_)
            | Self::ObserveGameLayout(_)
            | Self::InvalidGameLayout { .. }
            | Self::EngineWorkspaceRoot(_)
            | Self::MissingInitialSettings(_)
            | Self::ObserveInputSource(_) => Impact::ConfigurationOrInput,
            Self::InvalidStageRequest(_) => Impact::Internal,
            Self::ObserveEngineWorkspaceRoot(_)
            | Self::WorkspaceRoot(_)
            | Self::InspectExistingDatabase(_)
            | Self::ObserveWorkspaceStructure(_)
            | Self::ObserveExistingSource(_)
            | Self::ObservePreservedDirectory(_) => Impact::ProjectState,
            Self::Recover(_) => Impact::ProjectState,
            Self::Prepare(DirectoryPrepareError::NotPrepared {
                cleanup_failure, ..
            }) => {
                if cleanup_failure.is_some() {
                    Impact::RecoveryRequired
                } else {
                    Impact::ProjectState
                }
            }
            Self::PreserveObservability { discard, .. }
            | Self::CandidateFailure { discard, .. } => {
                if discard.is_some() {
                    Impact::RecoveryRequired
                } else {
                    Impact::ProjectState
                }
            }
            Self::CancellationCleanup(_) => Impact::RecoveryRequired,
            Self::Publish(error) => match error {
                DirectoryPublishError::TargetAlreadyExists {
                    cleanup_failure, ..
                }
                | DirectoryPublishError::TargetMissing {
                    cleanup_failure, ..
                }
                | DirectoryPublishError::TargetNotDirectory {
                    cleanup_failure, ..
                }
                | DirectoryPublishError::NotAttempted {
                    cleanup_failure, ..
                }
                | DirectoryPublishError::NotPublished {
                    cleanup_failure, ..
                } => {
                    if cleanup_failure.is_some() {
                        Impact::RecoveryRequired
                    } else {
                        Impact::ProjectState
                    }
                }
                DirectoryPublishError::PublishedWithResiduals { .. } => {
                    Impact::StateAppliedButFinalizationFailed
                }
                DirectoryPublishError::RecoveryRequired { .. } => Impact::RecoveryRequired,
                DirectoryPublishError::OutcomeUnknown { .. } => Impact::OutcomeUnknown,
            },
        }
    }
}

impl<D, S, I, R, E, P, A> From<DirectoryStageRequestError>
    for ProjectWorkspaceConvergenceError<D, S, I, R, E, P, A>
{
    fn from(error: DirectoryStageRequestError) -> Self {
        Self::InvalidStageRequest(error)
    }
}

impl<D, S, I, R, E, P, A> fmt::Display for ProjectWorkspaceConvergenceError<D, S, I, R, E, P, A>
where
    D: fmt::Display,
    S: fmt::Display,
    I: fmt::Display,
    R: fmt::Display,
    E: fmt::Display,
    P: fmt::Display,
    A: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceGameRoot(error) => write!(formatter, "无法使用游戏根目录：{error}"),
            Self::ObserveGameLayout(error) => write!(formatter, "无法检查游戏目录结构：{error}"),
            Self::InvalidGameLayout {
                game_root,
                engine,
                data_relative,
                js_relative,
                core_script,
            } => write!(
                formatter,
                "{} 不是有效的 RPG Maker {} 游戏根目录：必须包含目录 {}、{} 和核心脚本 {}/{}",
                game_root.display(),
                engine.to_uppercase(),
                data_relative.display(),
                js_relative.display(),
                js_relative.display(),
                core_script,
            ),
            Self::EngineWorkspaceRoot(error) => {
                write!(formatter, "无法建立 RPG Maker 项目集合目录：{error}")
            }
            Self::ObserveEngineWorkspaceRoot(error) => {
                write!(formatter, "无法检查 RPG Maker 项目集合目录：{error}")
            }
            Self::WorkspaceRoot(error) => write!(formatter, "项目工作区根无效：{error}"),
            Self::InspectExistingDatabase(error) => {
                write!(formatter, "现存项目数据库无效：{error}")
            }
            Self::MissingInitialSettings(settings) => {
                formatter.write_str("首次初始化还需要明确提供：")?;
                for (index, setting) in settings.iter().enumerate() {
                    if index != 0 {
                        formatter.write_str("、")?;
                    }
                    formatter.write_str(setting.cli_flag())?;
                }
                Ok(())
            }
            Self::ObserveWorkspaceStructure(error) => {
                write!(formatter, "无法检查现存工作区结构：{error}")
            }
            Self::ObserveExistingSource(error) => {
                write!(formatter, "无法检查现存冻结来源：{error}")
            }
            Self::ObserveInputSource(error) => {
                write!(formatter, "无法检查本次游戏来源：{error}")
            }
            Self::InvalidStageRequest(error) => write!(formatter, "工作区候选请求无效：{error}"),
            Self::Recover(error) => error.fmt(formatter),
            Self::Prepare(error) => write!(formatter, "无法准备工作区候选：{error}"),
            Self::ObservePreservedDirectory(error) => {
                write!(formatter, "无法检查现存可观测性目录：{error}")
            }
            Self::PreserveObservability { failure, discard } => {
                write!(formatter, "无法保留现存可观测性目录：{failure}")?;
                if let Some(discard) = discard {
                    write!(formatter, "；且候选清理失败：{discard}")?;
                }
                Ok(())
            }
            Self::CandidateFailure { failure, discard } => {
                write!(formatter, "工作区候选处理失败：{failure}")?;
                if let Some(discard) = discard {
                    write!(formatter, "；且候选清理失败：{discard}")?;
                }
                Ok(())
            }
            Self::CancellationCleanup(error) => {
                write!(formatter, "取消后无法清理工作区候选：{error}")
            }
            Self::Publish(error) => write!(formatter, "无法发布完整工作区：{error}"),
        }
    }
}

impl<D, S, R, P> fmt::Display for ProjectWorkspaceCandidateFailure<D, S, R, P>
where
    D: fmt::Display,
    S: fmt::Display,
    R: fmt::Display,
    P: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FingerprintCandidate(error) => {
                write!(formatter, "无法建立候选来源指纹：{error}")
            }
            Self::CreateDatabase(error) => write!(formatter, "无法创建候选数据库：{error}"),
            Self::SnapshotDatabase(error) => write!(formatter, "无法复制现存数据库：{error}"),
            Self::ReconcileDatabase(error) => write!(formatter, "无法对账候选数据库：{error}"),
        }
    }
}

impl<D, S, R, P> Error for ProjectWorkspaceCandidateFailure<D, S, R, P>
where
    D: Error + 'static,
    S: Error + 'static,
    R: Error + 'static,
    P: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::FingerprintCandidate(source) => Some(source),
            Self::CreateDatabase(source) => Some(source),
            Self::SnapshotDatabase(source) => Some(source),
            Self::ReconcileDatabase(source) => Some(source),
        }
    }
}

impl<D, S, I, R, E, P, A> Error for ProjectWorkspaceConvergenceError<D, S, I, R, E, P, A>
where
    D: Error + 'static,
    S: Error + 'static,
    I: Error + 'static,
    R: Error + 'static,
    E: Error + 'static,
    P: Error + 'static,
    A: Error + 'static,
{
}

/// 只负责验证初始化意图并交给工作区收敛边界。
pub(crate) struct InitService<W, P> {
    workspace_converger: W,
    project_lease: P,
    cancellation: CooperativeCancellation,
}

impl<W, P> InitService<W, P> {
    pub(crate) fn new(
        workspace_converger: W,
        project_lease: P,
        cancellation: CooperativeCancellation,
    ) -> Self {
        Self {
            workspace_converger,
            project_lease,
            cancellation,
        }
    }
}

impl<W, P> InitService<W, P>
where
    W: ProjectWorkspaceConverger,
    P: ProjectCommandLeaseProvider,
{
    pub(crate) async fn execute(
        &self,
        input: InitInput,
    ) -> Result<OperationCompletion<InitOutput>, InitServiceError<W::Error, P::Error>> {
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        let output_name = input.name.clone();
        let _lease = self
            .project_lease
            .acquire(&input.name)
            .await
            .map_err(InitServiceError::ProjectLease)?;
        if self.cancellation.is_requested() {
            return Ok(OperationCompletion::Cancelled);
        }
        let outcome = self
            .workspace_converger
            .converge(ProjectWorkspaceConvergenceRequest::new(
                input.game_root,
                input.name,
                input.source_language,
                input.target_language,
                input.dialogue_max_fullwidth_chars,
                input.scrolling_text_max_fullwidth_chars,
                input.help_description_max_fullwidth_chars,
            ))
            .await
            .map_err(InitServiceError::Workspace)?;
        let OperationCompletion::Completed(outcome) = outcome else {
            return Ok(OperationCompletion::Cancelled);
        };
        let outcome = match outcome {
            ProjectWorkspaceConvergence::Created => InitOutcome::Created,
            ProjectWorkspaceConvergence::Unchanged => InitOutcome::Unchanged,
            ProjectWorkspaceConvergence::Updated { stale_owners } => InitOutcome::Updated {
                stale_owners: stale_owners.into_iter().map(InitStaleOwner::from).collect(),
            },
        };

        Ok(OperationCompletion::Completed(InitOutput {
            name: output_name,
            outcome,
        }))
    }
}

impl From<RpgMakerAssetOwner> for InitStaleOwner {
    fn from(owner: RpgMakerAssetOwner) -> Self {
        match owner {
            RpgMakerAssetOwner::Builtin => Self::Builtin,
            RpgMakerAssetOwner::Rules => Self::Rules,
        }
    }
}

/// 初始化编排在本职责边界内能够产生的错误。
#[derive(Debug)]
pub(crate) enum InitServiceError<W, P> {
    ProjectLease(ProjectCommandLeaseError<P>),
    Workspace(W),
}

impl<W, P> fmt::Display for InitServiceError<W, P>
where
    W: Error,
    P: Error,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProjectLease(error) => error.fmt(formatter),
            Self::Workspace(error) => write!(formatter, "无法收敛项目工作区：{error}"),
        }
    }
}

impl<W, P> Error for InitServiceError<W, P>
where
    W: Error + 'static,
    P: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ProjectLease(error) => Some(error),
            Self::Workspace(error) => Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fs;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    type SnapshotDatabaseResponses = VecDeque<Result<(), SnapshotDatabaseError<FakeError>>>;
    use crate::fingerprint::Sha256Fingerprint;
    use crate::rpg_maker::project_database::{ProjectDatabaseReconciliation, ProjectDatabaseState};
    use crate::runtime::filesystem::{
        DirectoryPublisherConfig, SystemFileSystem, SystemFileSystemConfig,
    };

    fn find_entry_by_windows_name(parent: &Path, expected: &[u16]) -> PathBuf {
        fs::read_dir(parent)
            .unwrap_or_else(|error| panic!("应列举目录 {}：{error}", parent.display()))
            .map(|entry| entry.expect("目录项应可读取"))
            .find(|entry| {
                entry
                    .file_name()
                    .as_os_str()
                    .encode_wide()
                    .eq(expected.iter().copied())
            })
            .map(|entry| entry.path())
            .unwrap_or_else(|| {
                panic!(
                    "目录 {} 应包含 UTF-16 名称 {:?}",
                    parent.display(),
                    expected
                )
            })
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeError {}

    #[derive(Clone, Default)]
    struct Observations {
        events: Arc<Mutex<Vec<&'static str>>>,
        progress: Arc<Mutex<Vec<ProgressSnapshot<InitProgressPhase>>>>,
        stage_requests: Arc<Mutex<Vec<DirectoryStageRequest>>>,
        created_projects: Arc<Mutex<Vec<(PathBuf, NewProject)>>>,
        snapshots: Arc<Mutex<Vec<(PathBuf, PathBuf)>>>,
        reconciled_projects: Arc<Mutex<Vec<(PathBuf, NewProject)>>>,
    }

    impl Observations {
        fn event(&self, event: &'static str) {
            self.events
                .lock()
                .expect("events mutex should not be poisoned")
                .push(event);
        }

        fn events(&self) -> Vec<&'static str> {
            self.events
                .lock()
                .expect("events mutex should not be poisoned")
                .clone()
        }

        fn progress(&self) -> Vec<ProgressSnapshot<InitProgressPhase>> {
            self.progress
                .lock()
                .expect("progress mutex should not be poisoned")
                .clone()
        }
    }

    impl ProgressObserver<InitProgressPhase> for Observations {
        fn observe(&self, snapshot: ProgressSnapshot<InitProgressPhase>) {
            self.progress
                .lock()
                .expect("progress mutex should not be poisoned")
                .push(snapshot);
        }
    }

    #[derive(Clone, Copy)]
    enum ExistingSourceObservation {
        Fingerprint([u8; 32]),
        Missing,
    }

    #[derive(Clone, Copy, Debug)]
    enum WorkspaceStructureObservation {
        Complete,
        ObservabilityDirectories,
        LogsNotDirectory,
        TaskRecordsNotDirectory,
        SqliteSidecars,
        SqliteSidecarNotFile,
        DatabaseNotFile,
        SourceNotDirectory,
        SourceDataNotDirectory,
        WriteBackDataNotDirectory,
        ExtraWorkspaceEntry,
        ExtraSourceEntry,
        ExtraWriteBackEntry,
        MissingSourceEntry,
        WriteBackNotDirectory,
        Io,
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum PreservationFailure {
        List,
        Read,
        Edit,
    }

    #[derive(Clone)]
    struct FakeWorkspaceFileSystem {
        observations: Observations,
        namespace_error: Option<FakeError>,
        target_exists: Arc<AtomicBool>,
        workspace_structure: WorkspaceStructureObservation,
        existing_source: ExistingSourceObservation,
        candidate_fingerprint: [u8; 32],
        preservation_failure: Option<PreservationFailure>,
    }

    impl DirectChildDirectoryEnsurer for FakeWorkspaceFileSystem {
        type Error = FakeError;

        async fn ensure_direct_child_directory(
            &self,
            parent: PathBuf,
            child: OsString,
        ) -> Result<PathBuf, Self::Error> {
            self.observations.event("ensure_engine_root");
            assert_eq!(parent, Path::new("C:/projects"));
            assert_eq!(child, OsStr::new("mz"));
            if let Some(error) = self.namespace_error {
                return Err(error);
            }
            Ok(parent.join(child))
        }
    }

    impl ExistingDirectoryResolver for FakeWorkspaceFileSystem {
        type Error = FakeError;

        async fn resolve_existing_directory(
            &self,
            path: PathBuf,
        ) -> Result<PathBuf, ResolveDirectoryError<Self::Error>> {
            assert_ne!(
                path,
                Path::new("C:/projects/game"),
                "RPG Maker Init 不得探测缺少引擎命名空间的工作区"
            );
            if path == Path::new("C:/games/source") {
                self.observations.event("game_root");
                return Ok(path);
            }
            if path == Path::new("C:/projects/mz/game") {
                self.observations.event("workspace_root");
                if self.target_exists.load(Ordering::Acquire) {
                    return Ok(path);
                }
                return Err(ResolveDirectoryError::NotFound { path });
            }
            if path == Path::new("C:/projects/mz/game/logs")
                || path == Path::new("C:/projects/mz/game/task-records")
            {
                if (path.ends_with("logs")
                    && matches!(
                        self.workspace_structure,
                        WorkspaceStructureObservation::LogsNotDirectory
                    ))
                    || (path.ends_with("task-records")
                        && matches!(
                            self.workspace_structure,
                            WorkspaceStructureObservation::TaskRecordsNotDirectory
                        ))
                {
                    return Err(ResolveDirectoryError::NotDirectory { path });
                }
                // 只有声明了可观测性目录的工作区场景才存在这两个目录。
                if matches!(
                    self.workspace_structure,
                    WorkspaceStructureObservation::ObservabilityDirectories
                ) {
                    self.observations.event("probe_preserved");
                    return Ok(path);
                }
                return Err(ResolveDirectoryError::NotFound { path });
            }
            Ok(path)
        }
    }

    impl DirectoryLister for FakeWorkspaceFileSystem {
        type Error = FakeError;

        async fn list_directory(
            &self,
            path: PathBuf,
        ) -> Result<Vec<DirectoryEntry>, ListDirectoryError<Self::Error>> {
            if matches!(self.workspace_structure, WorkspaceStructureObservation::Io)
                && path.starts_with("C:/projects/mz/game")
            {
                return Err(ListDirectoryError::Io {
                    path,
                    source: FakeError("list workspace"),
                });
            }
            if path == Path::new("C:/games/source") {
                return Ok(vec![
                    DirectoryEntry::new(path.join("data"), DirectoryEntryKind::Directory),
                    DirectoryEntry::new(path.join("js"), DirectoryEntryKind::Directory),
                ]);
            }
            if path == Path::new("C:/games/source/js") {
                return Ok(vec![DirectoryEntry::new(
                    path.join("rmmz_core.js"),
                    DirectoryEntryKind::RegularFile,
                )]);
            }
            if path == Path::new("C:/projects/mz/game") {
                self.observations.event("list_workspace");
                let mut children = vec![
                    DirectoryEntry::new(
                        path.join("project.db"),
                        if matches!(
                            self.workspace_structure,
                            WorkspaceStructureObservation::DatabaseNotFile
                        ) {
                            DirectoryEntryKind::Directory
                        } else {
                            DirectoryEntryKind::RegularFile
                        },
                    ),
                    DirectoryEntry::new(
                        path.join("source"),
                        if matches!(
                            self.workspace_structure,
                            WorkspaceStructureObservation::SourceNotDirectory
                        ) {
                            DirectoryEntryKind::RegularFile
                        } else {
                            DirectoryEntryKind::Directory
                        },
                    ),
                    DirectoryEntry::new(path.join("write_back"), DirectoryEntryKind::Directory),
                ];
                if matches!(
                    self.workspace_structure,
                    WorkspaceStructureObservation::ExtraWorkspaceEntry
                ) {
                    children.push(DirectoryEntry::new(
                        path.join("unexpected"),
                        DirectoryEntryKind::RegularFile,
                    ));
                }
                if matches!(
                    self.workspace_structure,
                    WorkspaceStructureObservation::ObservabilityDirectories
                ) {
                    children.extend([
                        DirectoryEntry::new(path.join("logs"), DirectoryEntryKind::Directory),
                        DirectoryEntry::new(
                            path.join("task-records"),
                            DirectoryEntryKind::Directory,
                        ),
                    ]);
                }
                if matches!(
                    self.workspace_structure,
                    WorkspaceStructureObservation::LogsNotDirectory
                ) {
                    children.push(DirectoryEntry::new(
                        path.join("logs"),
                        DirectoryEntryKind::RegularFile,
                    ));
                }
                if matches!(
                    self.workspace_structure,
                    WorkspaceStructureObservation::TaskRecordsNotDirectory
                ) {
                    children.push(DirectoryEntry::new(
                        path.join("task-records"),
                        DirectoryEntryKind::RegularFile,
                    ));
                }
                if matches!(
                    self.workspace_structure,
                    WorkspaceStructureObservation::SqliteSidecars
                        | WorkspaceStructureObservation::SqliteSidecarNotFile
                ) {
                    children.extend([
                        DirectoryEntry::new(
                            path.join("project.db-journal"),
                            if matches!(
                                self.workspace_structure,
                                WorkspaceStructureObservation::SqliteSidecarNotFile
                            ) {
                                DirectoryEntryKind::Directory
                            } else {
                                DirectoryEntryKind::RegularFile
                            },
                        ),
                        DirectoryEntry::new(
                            path.join("project.db-wal"),
                            DirectoryEntryKind::RegularFile,
                        ),
                        DirectoryEntry::new(
                            path.join("project.db-shm"),
                            DirectoryEntryKind::RegularFile,
                        ),
                    ]);
                }
                return Ok(children);
            }
            if path == Path::new("C:/projects/mz/game/source") {
                self.observations.event("list_source");
                let mut children = vec![
                    DirectoryEntry::new(
                        path.join("data"),
                        if matches!(
                            self.workspace_structure,
                            WorkspaceStructureObservation::SourceDataNotDirectory
                        ) {
                            DirectoryEntryKind::RegularFile
                        } else {
                            DirectoryEntryKind::Directory
                        },
                    ),
                    DirectoryEntry::new(path.join("js"), DirectoryEntryKind::Directory),
                ];
                if matches!(
                    self.workspace_structure,
                    WorkspaceStructureObservation::MissingSourceEntry
                ) {
                    children.pop();
                }
                if matches!(
                    self.workspace_structure,
                    WorkspaceStructureObservation::ExtraSourceEntry
                ) {
                    children.push(DirectoryEntry::new(
                        path.join("unexpected"),
                        DirectoryEntryKind::RegularFile,
                    ));
                }
                return Ok(children);
            }
            if path == Path::new("C:/projects/mz/game/logs")
                || path == Path::new("C:/projects/mz/game/task-records")
            {
                self.observations.event("list_preserved");
                if path.ends_with("logs") {
                    if matches!(self.preservation_failure, Some(PreservationFailure::List)) {
                        return Err(ListDirectoryError::Io {
                            path,
                            source: FakeError("preserve list"),
                        });
                    }
                    if matches!(
                        self.preservation_failure,
                        Some(PreservationFailure::Read | PreservationFailure::Edit)
                    ) {
                        return Ok(vec![DirectoryEntry::new(
                            path.join("run.bin"),
                            DirectoryEntryKind::RegularFile,
                        )]);
                    }
                }
                return Ok(Vec::new());
            }
            if path == Path::new("C:/projects/mz/game/write_back") {
                self.observations.event("list_write_back");
                if matches!(
                    self.workspace_structure,
                    WorkspaceStructureObservation::WriteBackNotDirectory
                ) {
                    return Err(ListDirectoryError::NotDirectory { path });
                }
                let mut children = vec![
                    DirectoryEntry::new(
                        path.join("data"),
                        if matches!(
                            self.workspace_structure,
                            WorkspaceStructureObservation::WriteBackDataNotDirectory
                        ) {
                            DirectoryEntryKind::RegularFile
                        } else {
                            DirectoryEntryKind::Directory
                        },
                    ),
                    DirectoryEntry::new(path.join("js"), DirectoryEntryKind::Directory),
                ];
                if matches!(
                    self.workspace_structure,
                    WorkspaceStructureObservation::ExtraWriteBackEntry
                ) {
                    children.push(DirectoryEntry::new(
                        path.join("unexpected"),
                        DirectoryEntryKind::RegularFile,
                    ));
                }
                return Ok(children);
            }
            panic!("测试未声明目录列举：{}", path.display());
        }
    }

    impl DirectoryTreeFingerprinter for FakeWorkspaceFileSystem {
        type Error = FakeError;

        async fn fingerprint_directory_tree(
            &self,
            request: DirectoryTreeFingerprintRequest,
        ) -> Result<Sha256Fingerprint, DirectoryTreeFingerprintError<Self::Error>> {
            assert_eq!(
                request
                    .roots()
                    .iter()
                    .map(|root| root.logical_root())
                    .collect::<Vec<_>>(),
                vec![Path::new("data"), Path::new("js")]
            );
            let candidate = request
                .roots()
                .iter()
                .all(|root| root.physical_root().starts_with("C:/projects/.game-stage"));
            if candidate {
                self.observations.event("fingerprint_candidate");
                return Ok(Sha256Fingerprint::from_bytes(self.candidate_fingerprint));
            }
            let input = request
                .roots()
                .iter()
                .all(|root| root.physical_root().starts_with("C:/games/source"));
            if input {
                self.observations.event("fingerprint_input");
                return Ok(Sha256Fingerprint::from_bytes(self.candidate_fingerprint));
            }
            self.observations.event("fingerprint_existing");
            match self.existing_source {
                ExistingSourceObservation::Fingerprint(value) => {
                    Ok(Sha256Fingerprint::from_bytes(value))
                }
                ExistingSourceObservation::Missing => {
                    Err(DirectoryTreeFingerprintError::NotFound {
                        path: PathBuf::from("C:/projects/mz/game/source/data"),
                    })
                }
            }
        }
    }

    #[derive(Clone)]
    struct FakeDatabaseCreator {
        observations: Observations,
    }

    impl ProjectDatabaseCreator for FakeDatabaseCreator {
        type Error = FakeError;

        async fn create(
            &self,
            destination_path: PathBuf,
            project: NewProject,
        ) -> Result<(), Self::Error> {
            self.observations.event("create_database");
            self.observations
                .created_projects
                .lock()
                .expect("created projects mutex should not be poisoned")
                .push((destination_path.clone(), project));
            Ok(())
        }
    }

    #[derive(Clone)]
    struct FakeSnapshotter {
        observations: Observations,
        responses: Arc<Mutex<SnapshotDatabaseResponses>>,
    }

    impl SqliteDatabaseSnapshotter for FakeSnapshotter {
        type Error = FakeError;

        async fn snapshot_database(
            &self,
            source: PathBuf,
            destination: PathBuf,
        ) -> Result<(), SnapshotDatabaseError<Self::Error>> {
            self.observations.event("snapshot_database");
            self.observations
                .snapshots
                .lock()
                .expect("snapshots mutex should not be poisoned")
                .push((source, destination));
            self.responses
                .lock()
                .expect("snapshot responses mutex should not be poisoned")
                .pop_front()
                .expect("测试必须提供快照响应")
        }
    }

    #[derive(Clone)]
    struct FakeReconciler {
        observations: Observations,
        inspection: Result<ProjectDatabaseState, FakeError>,
        reconciliation: Result<ProjectDatabaseReconciliation, FakeError>,
    }

    impl ProjectDatabaseStateReconciler for FakeReconciler {
        type InspectionError = FakeError;
        type ReconciliationError = FakeError;

        async fn inspect(
            &self,
            _database_path: PathBuf,
            _expected_name: ProjectName,
        ) -> Result<ProjectDatabaseState, Self::InspectionError> {
            self.observations.event("inspect_database");
            self.inspection.clone()
        }

        async fn reconcile(
            &self,
            database_path: PathBuf,
            requested: NewProject,
        ) -> Result<ProjectDatabaseReconciliation, Self::ReconciliationError> {
            self.observations.event("reconcile_database");
            self.observations
                .reconciled_projects
                .lock()
                .expect("reconciled projects mutex should not be poisoned")
                .push((database_path, requested));
            self.reconciliation.clone()
        }
    }

    #[derive(Clone)]
    struct FakePublisher {
        observations: Observations,
        discard_error: Arc<Mutex<Option<FakeError>>>,
        preservation_failure: Option<PreservationFailure>,
        target_exists: Arc<AtomicBool>,
        recover_target_state: Option<bool>,
    }

    impl FileReader for FakeWorkspaceFileSystem {
        type Error = FakeError;

        async fn read_file(
            &self,
            path: PathBuf,
        ) -> Result<crate::storage::file_system::ReadFile, ReadFileError<Self::Error>> {
            if path == Path::new("C:/projects/mz/game/logs/run.bin") {
                if matches!(self.preservation_failure, Some(PreservationFailure::Read)) {
                    return Err(ReadFileError::Io {
                        path,
                        source: FakeError("preserve read"),
                    });
                }
                return Ok(crate::storage::file_system::ReadFile::new(
                    path,
                    vec![0, 0xff, 0x7f],
                ));
            }
            Err(ReadFileError::NotFound { path })
        }
    }

    impl ScopedDirectoryEditor for FakePublisher {
        type CandidateState = usize;
        type ScopeState = ();
        type Error = FakeError;

        fn bind_scoped_directory(
            &self,
            candidate: &StagedDirectory<Self::CandidateState>,
            scope: ScopedDirectoryScope,
        ) -> impl std::future::Future<
            Output = Result<
                crate::storage::file_system::BoundScopedDirectory<Self::ScopeState>,
                ScopedDirectoryBindError<Self::Error>,
            >,
        > + Send
        + use<> {
            self.observations.event("bind_preserved_scope");
            let root = candidate.staging_root().to_path_buf();
            async move {
                Ok(crate::storage::file_system::BoundScopedDirectory::new(
                    root,
                    scope,
                    (),
                ))
            }
        }

        async fn list_scoped_directory(
            &self,
            _scope: &crate::storage::file_system::BoundScopedDirectory<Self::ScopeState>,
            _path: ScopedDirectoryPath,
        ) -> Result<
            Vec<crate::storage::file_system::ScopedDirectoryEntry>,
            ScopedDirectoryEditError<Self::Error>,
        > {
            Ok(Vec::new())
        }

        async fn list_scoped_root(
            &self,
            _scope: &crate::storage::file_system::BoundScopedDirectory<Self::ScopeState>,
        ) -> Result<
            Vec<crate::storage::file_system::ScopedDirectoryEntry>,
            ScopedDirectoryEditError<Self::Error>,
        > {
            Ok(Vec::new())
        }

        async fn create_scoped_directory(
            &self,
            _scope: &crate::storage::file_system::BoundScopedDirectory<Self::ScopeState>,
            _path: ScopedDirectoryPath,
        ) -> Result<(), ScopedDirectoryEditError<Self::Error>> {
            self.observations.event("preserve_dir");
            Ok(())
        }

        async fn write_scoped_file(
            &self,
            _scope: &crate::storage::file_system::BoundScopedDirectory<Self::ScopeState>,
            path: ScopedDirectoryPath,
            _bytes: Vec<u8>,
        ) -> Result<(), ScopedDirectoryEditError<Self::Error>> {
            self.observations.event("preserve_file");
            if matches!(self.preservation_failure, Some(PreservationFailure::Edit)) {
                return Err(ScopedDirectoryEditError::Failed {
                    path: path.as_path().to_path_buf(),
                    source: FakeError("preserve write"),
                });
            }
            Ok(())
        }
    }

    impl RecoverableDirectoryPublisher for FakePublisher {
        type Error = FakeError;
        type StagingState = usize;

        async fn recover(
            &self,
            _target_root: PathBuf,
        ) -> Result<
            crate::storage::file_system::DirectoryRecoveryOutcome,
            DirectoryRecoveryError<Self::Error>,
        > {
            if let Some(target_exists) = self.recover_target_state {
                self.observations.event("recover");
                self.target_exists.store(target_exists, Ordering::Release);
                return Ok(crate::storage::file_system::DirectoryRecoveryOutcome::Recovered);
            }
            Ok(crate::storage::file_system::DirectoryRecoveryOutcome::Unchanged)
        }

        async fn prepare(
            &self,
            request: DirectoryStageRequest,
        ) -> Result<StagedDirectory<Self::StagingState>, DirectoryPrepareError<Self::Error>>
        {
            self.observations.event("prepare");
            self.observations
                .stage_requests
                .lock()
                .expect("stage requests mutex should not be poisoned")
                .push(request.clone());
            Ok(StagedDirectory::new(
                request.target_root().to_path_buf(),
                PathBuf::from("C:/projects/.game-stage"),
                request.publish_intent(),
                1,
            ))
        }

        async fn publish(
            &self,
            _staged: StagedDirectory<Self::StagingState>,
        ) -> Result<(), DirectoryPublishError<Self::Error>> {
            self.observations.event("publish");
            Ok(())
        }

        async fn discard(
            &self,
            staged: StagedDirectory<Self::StagingState>,
        ) -> Result<(), DirectoryDiscardError<Self::Error>> {
            self.observations.event("discard");
            match self
                .discard_error
                .lock()
                .expect("discard error mutex should not be poisoned")
                .take()
            {
                Some(source) => Err(DirectoryDiscardError::new(
                    staged.staging_root().to_path_buf(),
                    source,
                )),
                None => Ok(()),
            }
        }
    }

    fn width(value: u32) -> super::super::project::MaxFullwidthChars {
        super::super::project::MaxFullwidthChars::new(value).expect("宽度应合法")
    }

    fn language_id(value: &str) -> LanguageId {
        LanguageId::parse(value).expect("测试语言 ID 应合法")
    }

    fn profile() -> RpgMakerWriteBackLayoutProfile {
        RpgMakerWriteBackLayoutProfile::new(width(24), width(30), width(18))
    }

    fn fingerprint(value: u8) -> SourceSnapshotFingerprint {
        SourceSnapshotFingerprint::from_bytes([value; 32])
    }

    fn database_state(
        source_fingerprint: u8,
        owners: Vec<(RpgMakerAssetOwner, SourceSnapshotFingerprint)>,
    ) -> ProjectDatabaseState {
        ProjectDatabaseState::for_test(
            "game".parse().expect("项目名应合法"),
            LanguagePair::new(language_id("ja"), language_id("zh-Hans")),
            fingerprint(source_fingerprint),
            profile(),
            owners,
        )
    }

    fn request() -> ProjectWorkspaceConvergenceRequest {
        ProjectWorkspaceConvergenceRequest::new(
            PathBuf::from("C:/games/source"),
            "game".parse().expect("项目名应合法"),
            Some(language_id("ja")),
            Some(language_id("zh-Hans")),
            Some(profile().dialogue_body()),
            Some(profile().scrolling_text()),
            Some(profile().help_description()),
        )
    }

    fn omitted_settings_request() -> ProjectWorkspaceConvergenceRequest {
        ProjectWorkspaceConvergenceRequest::new(
            PathBuf::from("C:/games/source"),
            "game".parse().expect("项目名应合法"),
            None,
            None,
            None,
            None,
            None,
        )
    }

    fn service(
        target_exists: bool,
        workspace_structure: WorkspaceStructureObservation,
        existing_source: ExistingSourceObservation,
        candidate_fingerprint: u8,
        inspection: Result<ProjectDatabaseState, FakeError>,
        reconciliation: Result<ProjectDatabaseReconciliation, FakeError>,
        snapshot_response: Result<(), SnapshotDatabaseError<FakeError>>,
    ) -> (
        ProjectWorkspaceConvergenceService<
            FakeDatabaseCreator,
            FakeSnapshotter,
            FakeReconciler,
            FakeWorkspaceFileSystem,
            FakePublisher,
        >,
        Observations,
    ) {
        let observations = Observations::default();
        let target_exists = Arc::new(AtomicBool::new(target_exists));
        (
            ProjectWorkspaceConvergenceService::new(
                PathBuf::from("C:/projects"),
                RpgMakerLayout::MZ,
                FakeDatabaseCreator {
                    observations: observations.clone(),
                },
                FakeSnapshotter {
                    observations: observations.clone(),
                    responses: Arc::new(Mutex::new(VecDeque::from([snapshot_response]))),
                },
                FakeReconciler {
                    observations: observations.clone(),
                    inspection,
                    reconciliation,
                },
                FakeWorkspaceFileSystem {
                    observations: observations.clone(),
                    namespace_error: None,
                    target_exists: Arc::clone(&target_exists),
                    workspace_structure,
                    existing_source,
                    candidate_fingerprint: [candidate_fingerprint; 32],
                    preservation_failure: None,
                },
                FakePublisher {
                    observations: observations.clone(),
                    discard_error: Arc::new(Mutex::new(None)),
                    preservation_failure: None,
                    target_exists,
                    recover_target_state: None,
                },
                CooperativeCancellation::default(),
            )
            .with_progress(observations.clone()),
            observations,
        )
    }

    #[tokio::test]
    async fn engine_workspace_root_failure_stops_before_candidate_preparation() {
        let state = database_state(0x22, Vec::new());
        let (mut service, observations) = service(
            false,
            WorkspaceStructureObservation::Complete,
            ExistingSourceObservation::Missing,
            0x22,
            Ok(state.clone()),
            Ok(ProjectDatabaseReconciliation::for_test(state)),
            Ok(()),
        );
        service.file_system.namespace_error = Some(FakeError("cannot create mz root"));

        let error = service
            .converge(request())
            .await
            .expect_err("MZ 项目集合目录失败必须停止 Init");

        assert!(matches!(
            &error,
            ProjectWorkspaceConvergenceError::EngineWorkspaceRoot(FakeError(
                "cannot create mz root"
            ))
        ));
        assert_eq!(
            error.failure_impact(),
            ProjectWorkspaceConvergenceFailureImpact::ConfigurationOrInput
        );
        assert_eq!(
            observations.events(),
            vec!["workspace_root", "game_root", "ensure_engine_root"]
        );
        assert!(
            observations
                .stage_requests
                .lock()
                .expect("stage requests mutex should not be poisoned")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn first_init_reports_all_missing_settings_before_candidate_side_effects() {
        let state = database_state(0x22, Vec::new());
        let (service, observations) = service(
            false,
            WorkspaceStructureObservation::Complete,
            ExistingSourceObservation::Missing,
            0x22,
            Ok(state.clone()),
            Ok(ProjectDatabaseReconciliation::for_test(state)),
            Ok(()),
        );

        let error = service
            .converge(omitted_settings_request())
            .await
            .expect_err("首次 Init 必须一次报告全部缺失设置");

        assert!(matches!(
            error,
            ProjectWorkspaceConvergenceError::MissingInitialSettings(ref missing)
                if missing == &vec![
                    MissingInitialProjectSetting::SourceLanguage,
                    MissingInitialProjectSetting::TargetLanguage,
                    MissingInitialProjectSetting::DialogueMaxFullwidthChars,
                    MissingInitialProjectSetting::ScrollingTextMaxFullwidthChars,
                    MissingInitialProjectSetting::HelpDescriptionMaxFullwidthChars,
                ]
        ));
        assert_eq!(observations.events(), vec!["workspace_root"]);
        assert_eq!(
            observations.progress(),
            vec![ProgressSnapshot::indeterminate(
                InitProgressPhase::CheckingProject
            )]
        );
        assert!(
            observations
                .stage_requests
                .lock()
                .expect("stage requests mutex should not be poisoned")
                .is_empty()
        );
        assert!(
            observations
                .created_projects
                .lock()
                .expect("created projects mutex should not be poisoned")
                .is_empty()
        );
        assert!(
            observations
                .reconciled_projects
                .lock()
                .expect("reconciled projects mutex should not be poisoned")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn first_init_builds_candidate_database_then_publishes_create_new() {
        let candidate_state = database_state(0x22, Vec::new());
        let (service, observations) = service(
            false,
            WorkspaceStructureObservation::Complete,
            ExistingSourceObservation::Missing,
            0x22,
            Ok(candidate_state.clone()),
            Ok(ProjectDatabaseReconciliation::for_test(candidate_state)),
            Ok(()),
        );

        let outcome = service
            .converge(request())
            .await
            .expect("首次 Init 应创建工作区");

        assert_eq!(
            outcome,
            OperationCompletion::Completed(ProjectWorkspaceConvergence::Created)
        );
        assert_eq!(
            observations.events(),
            vec![
                "workspace_root",
                "game_root",
                "ensure_engine_root",
                "prepare",
                "fingerprint_candidate",
                "create_database",
                "reconcile_database",
                "publish",
            ]
        );
        assert_eq!(
            observations.progress(),
            vec![
                ProgressSnapshot::indeterminate(InitProgressPhase::CheckingProject),
                ProgressSnapshot::indeterminate(InitProgressPhase::ScanningSource),
                ProgressSnapshot::indeterminate(InitProgressPhase::PreparingCandidate),
                ProgressSnapshot::indeterminate(InitProgressPhase::UpdatingDatabase),
                ProgressSnapshot::indeterminate(InitProgressPhase::Publishing),
            ]
        );
        let stage_requests = observations
            .stage_requests
            .lock()
            .expect("stage requests mutex should not be poisoned");
        assert_eq!(
            stage_requests[0].publish_intent(),
            DirectoryPublishIntent::CreateNew
        );
        assert_eq!(
            stage_requests[0].target_root(),
            Path::new("C:/projects/mz/game")
        );
        assert_eq!(stage_requests[0].source_mappings().len(), 2);
        assert_eq!(
            stage_requests[0].source_mappings()[0].source_directory(),
            Path::new("C:/games/source/data")
        );
        assert_eq!(
            stage_requests[0].source_mappings()[0].relative_target(),
            Path::new("source/data")
        );
        assert_eq!(
            stage_requests[0].source_mappings()[1].source_directory(),
            Path::new("C:/games/source/js")
        );
        assert_eq!(
            stage_requests[0].source_mappings()[1].relative_target(),
            Path::new("source/js")
        );
        assert_eq!(
            stage_requests[0].empty_directories(),
            &[
                PathBuf::from("write_back/data"),
                PathBuf::from("write_back/js")
            ]
        );
        let created = observations
            .created_projects
            .lock()
            .expect("created projects mutex should not be poisoned");
        assert_eq!(created.len(), 1);
        assert_eq!(
            created[0].0,
            Path::new("C:/projects/.game-stage/project.db")
        );
        assert_eq!(
            created[0].1.source_snapshot_fingerprint(),
            fingerprint(0x22)
        );
        let reconciled = observations
            .reconciled_projects
            .lock()
            .expect("reconciled projects mutex should not be poisoned");
        assert_eq!(reconciled.len(), 1);
        assert_eq!(
            reconciled[0].0,
            Path::new("C:/projects/.game-stage/project.db")
        );
        assert_eq!(
            reconciled[0].1.source_snapshot_fingerprint(),
            fingerprint(0x22)
        );
        assert!(
            observations
                .snapshots
                .lock()
                .expect("snapshots mutex should not be poisoned")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn cancellation_before_convergence_emits_no_unstarted_phase() {
        let candidate_state = database_state(0x22, Vec::new());
        let (service, observations) = service(
            false,
            WorkspaceStructureObservation::Complete,
            ExistingSourceObservation::Missing,
            0x22,
            Ok(candidate_state.clone()),
            Ok(ProjectDatabaseReconciliation::for_test(candidate_state)),
            Ok(()),
        );
        service.cancellation.request();

        let completion = service
            .converge(request())
            .await
            .expect("预先取消应作为正常结果传播");

        assert_eq!(completion, OperationCompletion::Cancelled);
        assert!(observations.progress().is_empty());
        assert!(observations.events().is_empty());
    }

    #[tokio::test]
    async fn existing_project_inherits_each_omitted_setting() {
        let current = database_state(0x33, Vec::new());
        let (service, observations) = service(
            true,
            WorkspaceStructureObservation::Complete,
            ExistingSourceObservation::Fingerprint([0x33; 32]),
            0x33,
            Ok(current.clone()),
            Ok(ProjectDatabaseReconciliation::for_test(current)),
            Ok(()),
        );

        let outcome = service
            .converge(omitted_settings_request())
            .await
            .expect("已有项目应逐项复用数据库设置");

        assert_eq!(
            outcome,
            OperationCompletion::Completed(ProjectWorkspaceConvergence::Unchanged)
        );
        assert_eq!(
            observations.progress(),
            vec![
                ProgressSnapshot::indeterminate(InitProgressPhase::CheckingProject),
                ProgressSnapshot::indeterminate(InitProgressPhase::ScanningSource),
            ]
        );
        assert!(!observations.events().contains(&"prepare"));
        assert!(!observations.events().contains(&"snapshot_database"));
        assert!(!observations.events().contains(&"reconcile_database"));
    }

    #[tokio::test]
    async fn recovery_precedes_state_inheritance_and_unchanged_decision() {
        for target_exists_before_recovery in [true, false] {
            let current = database_state(0x33, Vec::new());
            let (mut service, observations) = service(
                target_exists_before_recovery,
                WorkspaceStructureObservation::Complete,
                ExistingSourceObservation::Fingerprint([0x33; 32]),
                0x33,
                Ok(current.clone()),
                Ok(ProjectDatabaseReconciliation::for_test(current)),
                Ok(()),
            );
            service.directories.recover_target_state = Some(true);

            let outcome = service
                .converge(omitted_settings_request())
                .await
                .expect("恢复后的项目应在同一次 Init 中继承设置并完成判断");

            assert_eq!(
                outcome,
                OperationCompletion::Completed(ProjectWorkspaceConvergence::Unchanged)
            );
            let events = observations.events();
            let recover = events
                .iter()
                .position(|event| *event == "recover")
                .expect("应先调用显式目录恢复");
            let workspace = events
                .iter()
                .position(|event| *event == "workspace_root")
                .expect("恢复后应重新观察项目工作区");
            let inspect = events
                .iter()
                .position(|event| *event == "inspect_database")
                .expect("恢复后应读取现存数据库设置");
            assert!(recover < workspace && workspace < inspect);
            assert!(!events.contains(&"prepare"));
            assert!(!events.contains(&"publish"));
        }
    }

    #[tokio::test]
    async fn recovered_missing_target_uses_replace_existing_for_changed_input() {
        let current = database_state(0x33, Vec::new());
        let updated = database_state(0x44, Vec::new());
        let (mut service, observations) = service(
            false,
            WorkspaceStructureObservation::Complete,
            ExistingSourceObservation::Fingerprint([0x33; 32]),
            0x44,
            Ok(current),
            Ok(ProjectDatabaseReconciliation::for_test(updated)),
            Ok(()),
        );
        service.directories.recover_target_state = Some(true);

        let outcome = service
            .converge(omitted_settings_request())
            .await
            .expect("恢复后的旧项目应在同一次 Init 中按新输入更新");

        assert!(matches!(
            outcome,
            OperationCompletion::Completed(ProjectWorkspaceConvergence::Updated { .. })
        ));
        let stage_requests = observations
            .stage_requests
            .lock()
            .expect("stage requests mutex should not be poisoned");
        assert_eq!(stage_requests.len(), 1);
        assert_eq!(
            stage_requests[0].publish_intent(),
            DirectoryPublishIntent::ReplaceExisting
        );
        assert!(observations.events().contains(&"snapshot_database"));
        assert!(!observations.events().contains(&"create_database"));
    }

    #[tokio::test]
    async fn existing_project_combines_explicit_overrides_with_inherited_settings() {
        let current = database_state(0x33, Vec::new());
        let updated = ProjectDatabaseState::for_test(
            "game".parse().expect("项目名应合法"),
            LanguagePair::new(language_id("ja"), language_id("zh-Hant")),
            fingerprint(0x33),
            RpgMakerWriteBackLayoutProfile::new(width(40), width(30), width(18)),
            Vec::new(),
        );
        let (service, observations) = service(
            true,
            WorkspaceStructureObservation::Complete,
            ExistingSourceObservation::Fingerprint([0x33; 32]),
            0x33,
            Ok(current),
            Ok(ProjectDatabaseReconciliation::for_test(updated)),
            Ok(()),
        );
        let mut requested = omitted_settings_request();
        requested.target_language = Some(language_id("zh-Hant"));
        requested.dialogue_max_fullwidth_chars = Some(width(40));

        let outcome = service
            .converge(requested)
            .await
            .expect("显式项应覆盖，省略项应继承");

        assert!(matches!(
            outcome,
            OperationCompletion::Completed(ProjectWorkspaceConvergence::Updated { .. })
        ));
        let reconciled = observations
            .reconciled_projects
            .lock()
            .expect("reconciled projects mutex should not be poisoned");
        let requested = &reconciled[0].1;
        assert_eq!(requested.source_language().as_str(), "ja");
        assert_eq!(requested.target_language().as_str(), "zh-Hant");
        assert_eq!(requested.layout_profile().dialogue_body(), width(40));
        assert_eq!(requested.layout_profile().scrolling_text(), width(30));
        assert_eq!(requested.layout_profile().help_description(), width(18));
    }

    #[tokio::test]
    async fn identical_existing_project_returns_before_candidate_and_preserves_output() {
        let current = database_state(0x33, Vec::new());
        let (service, observations) = service(
            true,
            WorkspaceStructureObservation::Complete,
            ExistingSourceObservation::Fingerprint([0x33; 32]),
            0x33,
            Ok(current.clone()),
            Ok(ProjectDatabaseReconciliation::for_test(current)),
            Ok(()),
        );

        let outcome = service
            .converge(request())
            .await
            .expect("完全相同的项目应成功 no-op");

        assert_eq!(
            outcome,
            OperationCompletion::Completed(ProjectWorkspaceConvergence::Unchanged)
        );
        assert_eq!(
            observations.events(),
            vec![
                "workspace_root",
                "inspect_database",
                "game_root",
                "list_workspace",
                "list_source",
                "list_write_back",
                "fingerprint_existing",
                "fingerprint_input",
            ]
        );
        assert!(
            observations
                .stage_requests
                .lock()
                .expect("stage requests mutex should not be poisoned")
                .is_empty()
        );
        assert!(
            observations
                .snapshots
                .lock()
                .expect("snapshots mutex should not be poisoned")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn observability_names_that_are_not_directories_are_rejected_before_noop_or_rebuild() {
        for (structure, entry_name) in [
            (WorkspaceStructureObservation::LogsNotDirectory, "logs"),
            (
                WorkspaceStructureObservation::TaskRecordsNotDirectory,
                "task-records",
            ),
        ] {
            // 0x33 原本会走 Unchanged，0x44 原本会整树重建；两条路径都必须先拒绝。
            for candidate_fingerprint in [0x33, 0x44] {
                let current = database_state(0x33, Vec::new());
                let (service, observations) = service(
                    true,
                    structure,
                    ExistingSourceObservation::Fingerprint([0x33; 32]),
                    candidate_fingerprint,
                    Ok(current.clone()),
                    Ok(ProjectDatabaseReconciliation::for_test(current)),
                    Ok(()),
                );

                let error = service
                    .converge(request())
                    .await
                    .expect_err("同名普通文件不能被静默删除或当成缺失目录");

                assert!(matches!(
                    error,
                    ProjectWorkspaceConvergenceError::ObservePreservedDirectory(
                        ResolveDirectoryError::NotDirectory { ref path }
                    ) if path == &Path::new("C:/projects/mz/game").join(entry_name)
                ));
                assert!(!observations.events().contains(&"prepare"));
                assert!(!observations.events().contains(&"publish"));
            }
        }
    }

    #[tokio::test]
    async fn sqlite_sidecars_do_not_make_an_identical_workspace_look_changed() {
        let current = database_state(0x33, Vec::new());
        let (service, observations) = service(
            true,
            WorkspaceStructureObservation::SqliteSidecars,
            ExistingSourceObservation::Fingerprint([0x33; 32]),
            0x33,
            Ok(current.clone()),
            Ok(ProjectDatabaseReconciliation::for_test(current)),
            Ok(()),
        );

        let outcome = service
            .converge(request())
            .await
            .expect("SQLite sidecar 属于已检查数据库的存储语义");

        assert_eq!(
            outcome,
            OperationCompletion::Completed(ProjectWorkspaceConvergence::Unchanged)
        );
        assert!(!observations.events().contains(&"prepare"));
        assert!(!observations.events().contains(&"publish"));
    }

    #[tokio::test]
    async fn observability_directories_do_not_make_an_identical_workspace_look_changed() {
        let current = database_state(0x33, Vec::new());
        let (service, observations) = service(
            true,
            WorkspaceStructureObservation::ObservabilityDirectories,
            ExistingSourceObservation::Fingerprint([0x33; 32]),
            0x33,
            Ok(current.clone()),
            Ok(ProjectDatabaseReconciliation::for_test(current)),
            Ok(()),
        );

        let outcome = service
            .converge(request())
            .await
            .expect("可观测性目录属于合法的项目工作区设施");

        assert_eq!(
            outcome,
            OperationCompletion::Completed(ProjectWorkspaceConvergence::Unchanged)
        );
        assert!(!observations.events().contains(&"prepare"));
        assert!(!observations.events().contains(&"publish"));
    }

    #[tokio::test]
    async fn changed_source_updates_workspace_and_reports_stale_owners() {
        let current = database_state(0x33, vec![(RpgMakerAssetOwner::Builtin, fingerprint(0x33))]);
        let updated = database_state(0x44, vec![(RpgMakerAssetOwner::Builtin, fingerprint(0x33))]);
        let (service, observations) = service(
            true,
            WorkspaceStructureObservation::Complete,
            ExistingSourceObservation::Fingerprint([0x33; 32]),
            0x44,
            Ok(current),
            Ok(ProjectDatabaseReconciliation::for_test(updated)),
            Ok(()),
        );

        let outcome = service
            .converge(request())
            .await
            .expect("来源变化应发布更新");

        assert_eq!(
            outcome,
            OperationCompletion::Completed(ProjectWorkspaceConvergence::Updated {
                stale_owners: vec![RpgMakerAssetOwner::Builtin],
            })
        );
        assert!(observations.events().contains(&"snapshot_database"));
        assert!(observations.events().contains(&"publish"));
        assert!(!observations.events().contains(&"discard"));
    }

    #[tokio::test]
    async fn workspace_rebuild_preserves_existing_observability_directories() {
        // 来源变化触发整树重建时,logs/ 与 task-records/ 是既有运行历史与
        // 人工补译审计材料,必须进入候选而不是随 ReplaceExisting 静默消失。
        let current = database_state(0x33, Vec::new());
        let updated = database_state(0x44, Vec::new());
        let (service, observations) = service(
            true,
            WorkspaceStructureObservation::ObservabilityDirectories,
            ExistingSourceObservation::Fingerprint([0x33; 32]),
            0x44,
            Ok(current),
            Ok(ProjectDatabaseReconciliation::for_test(updated)),
            Ok(()),
        );

        let outcome = service
            .converge(request())
            .await
            .expect("带可观测性目录的来源变化应发布更新");

        assert!(matches!(
            outcome,
            OperationCompletion::Completed(ProjectWorkspaceConvergence::Updated { .. })
        ));
        let events = observations.events();
        assert!(events.contains(&"probe_preserved"));
        assert!(events.contains(&"bind_preserved_scope"));
        assert!(events.contains(&"list_preserved"));
        assert!(events.contains(&"publish"));
        assert!(!events.contains(&"discard"));
        let stage_requests = observations
            .stage_requests
            .lock()
            .expect("stage requests mutex should not be poisoned");
        let request = stage_requests.last().expect("重建应产生候选请求");
        for preserved in PRESERVED_OBSERVABILITY_DIRECTORIES {
            assert!(
                request
                    .empty_directories()
                    .iter()
                    .any(|path| path == Path::new(preserved)),
                "候选必须为保留目录 {preserved} 建立空目录根"
            );
        }
    }

    #[tokio::test]
    async fn real_filesystem_preserves_nested_empty_zero_binary_and_unicode_observability_entries()
    {
        let temporary = tempfile::tempdir().expect("应建立真实文件系统临时目录");
        let workspace = temporary.path().join("existing-workspace");
        let logs = workspace.join("logs");
        let task_records = workspace.join("task-records");
        fs::create_dir_all(logs.join("nested/空目录")).expect("应建立嵌套空日志目录");
        fs::create_dir_all(task_records.join("人工补译")).expect("应建立 Unicode 审计目录");
        fs::write(logs.join("empty.jsonl"), []).expect("应建立零字节日志");
        let binary = [0, 0xff, 0x7f, 0x80, b'\n'];
        fs::write(logs.join("nested/raw.bin"), binary).expect("应建立二进制日志");
        let unicode_bytes = "人工补译记录\n".as_bytes();
        fs::write(task_records.join("人工补译/任务一.md"), unicode_bytes)
            .expect("应建立 Unicode 审计文件");

        let seed = temporary.path().join("seed");
        fs::create_dir(&seed).expect("应建立候选种子目录");
        let target = temporary.path().join("target");
        let file_system =
            SystemFileSystem::new(SystemFileSystemConfig::production()).expect("应建立文件系统根");
        let publisher = file_system.directory_publisher(
            DirectoryPublisherConfig::production(temporary.path().join("locks"))
                .expect("锁目录配置应合法"),
        );
        let request = DirectoryStageRequest::new(
            target.clone(),
            DirectoryPublishIntent::CreateNew,
            vec![
                DirectorySourceMapping::new(seed, PathBuf::from("seed"))
                    .expect("候选种子映射应合法"),
            ],
            Vec::new(),
            PRESERVED_OBSERVABILITY_DIRECTORIES
                .iter()
                .map(PathBuf::from)
                .collect(),
        )
        .expect("候选请求应合法");
        let staged = publisher.prepare(request).await.expect("应准备真实候选");
        let staging_root = staged.staging_root().to_path_buf();
        let scope = ScopedDirectoryScope::new(
            PRESERVED_OBSERVABILITY_DIRECTORIES
                .iter()
                .map(OsString::from),
        )
        .expect("保留范围应合法");
        let bound = publisher
            .bind_scoped_directory(&staged, scope)
            .await
            .expect("应绑定真实候选范围");

        preserve_observability_directories(
            &file_system,
            &publisher,
            &workspace,
            &bound,
            &PRESERVED_OBSERVABILITY_DIRECTORIES,
        )
        .await
        .expect("真实可观测性目录应逐字节搬入候选");

        assert!(staging_root.join("logs/nested/空目录").is_dir());
        assert!(staging_root.join("task-records/人工补译").is_dir());
        assert_eq!(
            fs::read(staging_root.join("logs/empty.jsonl")).expect("候选零字节日志应可读"),
            Vec::<u8>::new()
        );
        assert_eq!(
            fs::read(staging_root.join("logs/nested/raw.bin")).expect("候选二进制日志应可读"),
            binary
        );
        assert_eq!(
            fs::read(staging_root.join("task-records/人工补译/任务一.md"))
                .expect("候选 Unicode 审计文件应可读"),
            unicode_bytes
        );
        assert_eq!(
            fs::read(logs.join("nested/raw.bin")).expect("原工作区二进制日志应保持可读"),
            binary,
            "保留步骤只能复制，不能修改原工作区"
        );

        publisher.discard(staged).await.expect("应丢弃测试候选");
        assert!(!staging_root.exists(), "discard 应移除候选");
        assert!(!target.exists(), "未 publish 不得建立目标");
        assert_eq!(
            fs::read(task_records.join("人工补译/任务一.md"))
                .expect("discard 后原审计文件仍应可读"),
            unicode_bytes
        );
        file_system.shutdown().await.expect("文件系统根应终结");
    }

    #[tokio::test]
    async fn real_replace_ignores_then_discards_unknown_entries_and_preserves_observability() {
        let temporary = tempfile::tempdir().expect("应建立真实文件系统临时目录");
        let workspace = temporary.path().join("workspace");
        for directory in [
            "source/data",
            "source/js",
            "write_back/data",
            "write_back/js",
            "source/unknown-directory",
            "logs/nested/空目录",
            "task-records/人工补译",
        ] {
            fs::create_dir_all(workspace.join(directory)).expect("应建立现有工作区目录");
        }
        fs::write(workspace.join("project.db"), b"old database").expect("应建立项目数据库");
        fs::write(workspace.join("unknown-root.bin"), b"discard me").expect("应建立未知根文件");
        fs::write(
            workspace.join("source/unknown-directory/value.bin"),
            b"discard nested",
        )
        .expect("应建立未知 source 条目");
        fs::write(workspace.join("write_back/obsolete.bin"), b"discard output")
            .expect("应建立未知 write_back 条目");
        fs::write(workspace.join("logs/zero.jsonl"), []).expect("应建立零字节日志");
        let log_bytes = [0, 0xff, 0x80, b'\n'];
        fs::write(workspace.join("logs/nested/raw.bin"), log_bytes).expect("应建立二进制日志");
        let task_bytes = "人工补译记录\n".as_bytes();
        fs::write(
            workspace.join("task-records/人工补译/任务一.md"),
            task_bytes,
        )
        .expect("应建立 Unicode 任务记录");
        let raw_directory_units = [
            u16::from(b'h'),
            u16::from(b'i'),
            u16::from(b'g'),
            u16::from(b'h'),
            0xd800,
        ];
        let raw_file_units = [
            u16::from(b'l'),
            u16::from(b'o'),
            u16::from(b'w'),
            0xdc00,
            u16::from(b'.'),
            u16::from(b'b'),
            u16::from(b'i'),
            u16::from(b'n'),
        ];
        let raw_directory = workspace
            .join("logs")
            .join(OsString::from_wide(&raw_directory_units));
        fs::create_dir(&raw_directory).expect("应建立含孤立高 surrogate 的日志目录");
        let raw_windows_bytes = [0, 0xff, 0x81, b'\r', b'\n'];
        fs::write(
            raw_directory.join(OsString::from_wide(&raw_file_units)),
            raw_windows_bytes,
        )
        .expect("应建立含孤立低 surrogate 的二进制日志");

        let replacement_data = temporary.path().join("replacement-data");
        let replacement_js = temporary.path().join("replacement-js");
        fs::create_dir(&replacement_data).expect("应建立替换 data");
        fs::create_dir(&replacement_js).expect("应建立替换 js");
        fs::write(replacement_data.join("Actors.json"), b"[]").expect("应建立替换数据");
        fs::write(replacement_js.join("rmmz_core.js"), b"new core").expect("应建立替换脚本");

        let file_system =
            SystemFileSystem::new(SystemFileSystemConfig::production()).expect("应建立文件系统根");
        let layout =
            ProjectWorkspaceLayout::from_workspace_root(workspace.clone(), RpgMakerLayout::MZ);
        assert!(
            observe_required_workspace_structure(&file_system, &layout)
                .await
                .expect("应列举真实工作区"),
            "未知条目不得使必需结构失效"
        );

        let publisher = file_system.directory_publisher(
            DirectoryPublisherConfig::production(temporary.path().join("locks"))
                .expect("锁目录配置应合法"),
        );
        let request = DirectoryStageRequest::new(
            workspace.clone(),
            DirectoryPublishIntent::ReplaceExisting,
            vec![
                DirectorySourceMapping::new(replacement_data, PathBuf::from("source/data"))
                    .expect("data 映射应合法"),
                DirectorySourceMapping::new(replacement_js, PathBuf::from("source/js"))
                    .expect("js 映射应合法"),
            ],
            Vec::new(),
            ["write_back/data", "write_back/js", "logs", "task-records"]
                .into_iter()
                .map(PathBuf::from)
                .collect(),
        )
        .expect("替换候选请求应合法");
        let staged = publisher.prepare(request).await.expect("应准备替换候选");
        let scope = ScopedDirectoryScope::new(
            PRESERVED_OBSERVABILITY_DIRECTORIES
                .iter()
                .map(OsString::from),
        )
        .expect("保留范围应合法");
        let bound = publisher
            .bind_scoped_directory(&staged, scope)
            .await
            .expect("应绑定保留目录");
        preserve_observability_directories(
            &file_system,
            &publisher,
            &workspace,
            &bound,
            &PRESERVED_OBSERVABILITY_DIRECTORIES,
        )
        .await
        .expect("可观测性目录应进入替换候选");
        fs::write(staged.staging_root().join("project.db"), b"new database")
            .expect("应建立候选项目数据库");

        publisher.publish(staged).await.expect("真实替换应发布");

        assert!(!workspace.join("unknown-root.bin").exists());
        assert!(!workspace.join("source/unknown-directory").exists());
        assert!(!workspace.join("write_back/obsolete.bin").exists());
        assert_eq!(
            fs::read(workspace.join("source/data/Actors.json")).expect("替换数据应可读"),
            b"[]"
        );
        assert!(workspace.join("logs/nested/空目录").is_dir());
        assert_eq!(
            fs::read(workspace.join("logs/zero.jsonl")).expect("零字节日志应可读"),
            Vec::<u8>::new()
        );
        assert_eq!(
            fs::read(workspace.join("logs/nested/raw.bin")).expect("二进制日志应可读"),
            log_bytes
        );
        assert_eq!(
            fs::read(workspace.join("task-records/人工补译/任务一.md"))
                .expect("Unicode 任务记录应可读"),
            task_bytes
        );
        let preserved_raw_directory =
            find_entry_by_windows_name(&workspace.join("logs"), &raw_directory_units);
        assert_eq!(
            preserved_raw_directory
                .file_name()
                .expect("保留目录必须包含名称")
                .encode_wide()
                .collect::<Vec<_>>(),
            raw_directory_units
        );
        let preserved_raw_file =
            find_entry_by_windows_name(&preserved_raw_directory, &raw_file_units);
        assert_eq!(
            preserved_raw_file
                .file_name()
                .expect("保留文件必须包含名称")
                .encode_wide()
                .collect::<Vec<_>>(),
            raw_file_units
        );
        assert_eq!(
            fs::read(preserved_raw_file).expect("原始 UTF-16 二进制日志应可读"),
            raw_windows_bytes
        );
        file_system.shutdown().await.expect("文件系统根应终结");
    }

    #[tokio::test]
    async fn observability_list_read_and_write_failures_discard_once_without_publish() {
        for failure in [
            PreservationFailure::List,
            PreservationFailure::Read,
            PreservationFailure::Edit,
        ] {
            let current = database_state(0x33, Vec::new());
            let updated = database_state(0x44, Vec::new());
            let (mut service, observations) = service(
                true,
                WorkspaceStructureObservation::ObservabilityDirectories,
                ExistingSourceObservation::Fingerprint([0x33; 32]),
                0x44,
                Ok(current),
                Ok(ProjectDatabaseReconciliation::for_test(updated)),
                Ok(()),
            );
            service.file_system.preservation_failure = Some(failure);
            service.directories.preservation_failure = Some(failure);

            let error = match service.converge(request()).await {
                Err(error) => error,
                Ok(_) => panic!("{failure:?} 保留失败不得 publish"),
            };

            let ProjectWorkspaceConvergenceError::PreserveObservability {
                failure: primary,
                discard,
            } = error
            else {
                panic!("{failure:?} 应保留为可观测性搬运失败")
            };
            assert!(discard.is_none(), "候选清理成功时不得伪造相关失败");
            match failure {
                PreservationFailure::List => assert!(matches!(
                    primary,
                    PreserveObservabilityFailure::List {
                        source: ListDirectoryError::Io {
                            source: FakeError("preserve list"),
                            ..
                        },
                        ..
                    }
                )),
                PreservationFailure::Read => assert!(matches!(
                    primary,
                    PreserveObservabilityFailure::Read {
                        source: ReadFileError::Io {
                            source: FakeError("preserve read"),
                            ..
                        },
                        ..
                    }
                )),
                PreservationFailure::Edit => assert!(matches!(
                    primary,
                    PreserveObservabilityFailure::Edit {
                        source: ScopedDirectoryEditError::Failed {
                            source: FakeError("preserve write"),
                            ..
                        },
                        ..
                    }
                )),
            }
            let events = observations.events();
            assert_eq!(
                events.iter().filter(|event| **event == "discard").count(),
                1,
                "{failure:?} 后候选必须且只能 discard 一次"
            );
            assert!(!events.contains(&"publish"), "{failure:?} 后不得 publish");
            assert!(
                !events.contains(&"snapshot_database"),
                "{failure:?} 后不得继续修改候选数据库"
            );
        }
    }

    #[tokio::test]
    async fn observability_primary_and_discard_failures_are_both_preserved() {
        let current = database_state(0x33, Vec::new());
        let updated = database_state(0x44, Vec::new());
        let (mut service, observations) = service(
            true,
            WorkspaceStructureObservation::ObservabilityDirectories,
            ExistingSourceObservation::Fingerprint([0x33; 32]),
            0x44,
            Ok(current),
            Ok(ProjectDatabaseReconciliation::for_test(updated)),
            Ok(()),
        );
        service.file_system.preservation_failure = Some(PreservationFailure::Read);
        *service
            .directories
            .discard_error
            .lock()
            .expect("discard error mutex should not be poisoned") = Some(FakeError("discard"));

        let error = service
            .converge(request())
            .await
            .expect_err("保留与候选清理双重失败必须同时返回");

        assert!(matches!(
            error,
            ProjectWorkspaceConvergenceError::PreserveObservability {
                failure: PreserveObservabilityFailure::Read {
                    source: ReadFileError::Io {
                        source: FakeError("preserve read"),
                        ..
                    },
                    ..
                },
                discard: Some(ref discard),
            } if discard.source() == &FakeError("discard")
                && discard.staging_root() == Path::new("C:/projects/.game-stage")
        ));
        let events = observations.events();
        assert_eq!(
            events.iter().filter(|event| **event == "discard").count(),
            1
        );
        assert!(!events.contains(&"publish"));
    }

    #[tokio::test]
    async fn missing_or_wrong_required_workspace_structure_is_repaired() {
        for structure in [
            WorkspaceStructureObservation::DatabaseNotFile,
            WorkspaceStructureObservation::SourceNotDirectory,
            WorkspaceStructureObservation::SourceDataNotDirectory,
            WorkspaceStructureObservation::WriteBackDataNotDirectory,
            WorkspaceStructureObservation::MissingSourceEntry,
            WorkspaceStructureObservation::WriteBackNotDirectory,
        ] {
            let current = database_state(0x55, Vec::new());
            let (service, observations) = service(
                true,
                structure,
                ExistingSourceObservation::Fingerprint([0x55; 32]),
                0x55,
                Ok(current.clone()),
                Ok(ProjectDatabaseReconciliation::for_test(current)),
                Ok(()),
            );

            let outcome = service
                .converge(request())
                .await
                .unwrap_or_else(|error| panic!("{structure:?} 应执行 repair：{error}"));

            assert_eq!(
                outcome,
                OperationCompletion::Completed(ProjectWorkspaceConvergence::Updated {
                    stale_owners: Vec::new(),
                }),
                "{structure:?}"
            );
            assert!(observations.events().contains(&"publish"), "{structure:?}");
            assert!(!observations.events().contains(&"discard"), "{structure:?}");
            assert!(
                !observations.events().contains(&"fingerprint_existing"),
                "结构不完整时不应把其内容身份当作可复用事实：{structure:?}"
            );
        }
    }

    #[tokio::test]
    async fn unknown_workspace_entries_do_not_trigger_rebuild() {
        for structure in [
            WorkspaceStructureObservation::SqliteSidecarNotFile,
            WorkspaceStructureObservation::ExtraWorkspaceEntry,
            WorkspaceStructureObservation::ExtraSourceEntry,
            WorkspaceStructureObservation::ExtraWriteBackEntry,
        ] {
            let current = database_state(0x55, Vec::new());
            let (service, observations) = service(
                true,
                structure,
                ExistingSourceObservation::Fingerprint([0x55; 32]),
                0x55,
                Ok(current.clone()),
                Ok(ProjectDatabaseReconciliation::for_test(current)),
                Ok(()),
            );

            let outcome = service
                .converge(request())
                .await
                .unwrap_or_else(|error| panic!("{structure:?} 应被忽略：{error}"));

            assert_eq!(
                outcome,
                OperationCompletion::Completed(ProjectWorkspaceConvergence::Unchanged),
                "{structure:?}"
            );
            let events = observations.events();
            assert!(events.contains(&"fingerprint_existing"), "{structure:?}");
            assert!(!events.contains(&"prepare"), "{structure:?}");
            assert!(!events.contains(&"publish"), "{structure:?}");
            assert!(!events.contains(&"discard"), "{structure:?}");
        }
    }

    #[tokio::test]
    async fn workspace_structure_listing_io_failure_is_technical_error() {
        let current = database_state(0x55, Vec::new());
        let (service, observations) = service(
            true,
            WorkspaceStructureObservation::Io,
            ExistingSourceObservation::Fingerprint([0x55; 32]),
            0x55,
            Ok(current.clone()),
            Ok(ProjectDatabaseReconciliation::for_test(current)),
            Ok(()),
        );

        let error = service
            .converge(request())
            .await
            .expect_err("真实列举 I/O 失败不得被误判为可修复结构偏差");

        assert!(matches!(
            error,
            ProjectWorkspaceConvergenceError::ObserveWorkspaceStructure(ListDirectoryError::Io {
                source: FakeError("list workspace"),
                ..
            })
        ));
        assert!(!observations.events().contains(&"prepare"));
    }

    #[tokio::test]
    async fn invalid_existing_database_stops_before_candidate_preparation() {
        let current = database_state(0x33, Vec::new());
        let (service, observations) = service(
            true,
            WorkspaceStructureObservation::Complete,
            ExistingSourceObservation::Fingerprint([0x33; 32]),
            0x33,
            Err(FakeError("invalid database")),
            Ok(ProjectDatabaseReconciliation::for_test(current)),
            Ok(()),
        );

        let error = service
            .converge(request())
            .await
            .expect_err("无效数据库绝不能被覆盖");

        assert!(matches!(
            error,
            ProjectWorkspaceConvergenceError::InspectExistingDatabase(FakeError(
                "invalid database"
            ))
        ));
        assert_eq!(
            observations.events(),
            vec!["workspace_root", "inspect_database"]
        );
    }

    #[tokio::test]
    async fn snapshot_failure_discards_candidate_once_and_preserves_primary_error() {
        let current = database_state(0x33, Vec::new());
        let (service, observations) = service(
            true,
            WorkspaceStructureObservation::Complete,
            ExistingSourceObservation::Fingerprint([0x33; 32]),
            0x44,
            Ok(current.clone()),
            Ok(ProjectDatabaseReconciliation::for_test(current)),
            Err(SnapshotDatabaseError::NotCreated(FakeError("backup"))),
        );

        let error = service
            .converge(request())
            .await
            .expect_err("快照失败必须拒绝候选");

        assert!(matches!(
            error,
            ProjectWorkspaceConvergenceError::CandidateFailure {
                failure: ProjectWorkspaceCandidateFailure::SnapshotDatabase(
                    SnapshotDatabaseError::NotCreated(FakeError("backup"))
                ),
                discard: None,
            }
        ));
        assert_eq!(
            observations
                .events()
                .into_iter()
                .filter(|event| *event == "discard")
                .count(),
            1
        );
        assert!(!observations.events().contains(&"reconcile_database"));
    }

    #[tokio::test]
    async fn candidate_failure_preserves_primary_and_discard_errors() {
        let current = database_state(0x33, Vec::new());
        let (service, _) = service(
            true,
            WorkspaceStructureObservation::Complete,
            ExistingSourceObservation::Fingerprint([0x33; 32]),
            0x44,
            Ok(current.clone()),
            Ok(ProjectDatabaseReconciliation::for_test(current)),
            Err(SnapshotDatabaseError::NotCreated(FakeError("backup"))),
        );
        *service
            .directories
            .discard_error
            .lock()
            .expect("discard error mutex should not be poisoned") = Some(FakeError("discard"));

        let error = service
            .converge(request())
            .await
            .expect_err("快照与候选清理双重失败必须同时保留");

        assert!(matches!(
            error,
            ProjectWorkspaceConvergenceError::CandidateFailure {
                failure: ProjectWorkspaceCandidateFailure::SnapshotDatabase(
                    SnapshotDatabaseError::NotCreated(FakeError("backup"))
                ),
                discard: Some(ref discard),
            } if discard.source() == &FakeError("discard")
                && discard.staging_root() == Path::new("C:/projects/.game-stage")
        ));
    }

    type FakeWorkspaceResponse =
        Result<OperationCompletion<ProjectWorkspaceConvergence>, FakeError>;

    #[derive(Clone)]
    struct FakeWorkspaceConverger {
        requests: Arc<Mutex<Vec<ProjectWorkspaceConvergenceRequest>>>,
        responses: Arc<Mutex<VecDeque<FakeWorkspaceResponse>>>,
    }

    impl ProjectWorkspaceConverger for FakeWorkspaceConverger {
        type Error = FakeError;

        async fn converge(
            &self,
            request: ProjectWorkspaceConvergenceRequest,
        ) -> Result<OperationCompletion<ProjectWorkspaceConvergence>, Self::Error> {
            self.requests
                .lock()
                .expect("workspace requests mutex should not be poisoned")
                .push(request);
            self.responses
                .lock()
                .expect("workspace responses mutex should not be poisoned")
                .pop_front()
                .expect("测试必须提供响应")
        }
    }

    #[derive(Clone, Copy)]
    struct FakeProjectLease;

    impl ProjectCommandLeaseProvider for FakeProjectLease {
        type Error = FakeError;
        type LeaseState = ();

        async fn acquire(
            &self,
            _: &ProjectName,
        ) -> Result<
            crate::project_lease::ProjectCommandLease<Self::LeaseState>,
            ProjectCommandLeaseError<Self::Error>,
        > {
            Ok(crate::project_lease::ProjectCommandLease::for_test(()))
        }
    }

    fn init_input() -> InitInput {
        InitInput {
            name: "game".parse().expect("项目名应合法"),
            game_root: PathBuf::from("./Game"),
            source_language: Some(language_id("JA")),
            target_language: Some(language_id("zh-hans")),
            dialogue_max_fullwidth_chars: Some(profile().dialogue_body()),
            scrolling_text_max_fullwidth_chars: Some(profile().scrolling_text()),
            help_description_max_fullwidth_chars: Some(profile().help_description()),
        }
    }

    #[tokio::test]
    async fn init_service_forwards_trusted_languages_and_maps_updated_owner_result() {
        let converger = FakeWorkspaceConverger {
            requests: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(VecDeque::from([Ok(
                OperationCompletion::Completed(ProjectWorkspaceConvergence::Updated {
                    stale_owners: vec![RpgMakerAssetOwner::Builtin, RpgMakerAssetOwner::Rules],
                }),
            )]))),
        };
        let service = InitService::new(
            converger,
            FakeProjectLease,
            CooperativeCancellation::default(),
        );

        let output = service
            .execute(init_input())
            .await
            .expect("Init 编排应成功");

        let OperationCompletion::Completed(output) = output else {
            panic!("Init 应正常完成")
        };
        assert_eq!(output.name.as_str(), "game");
        assert_eq!(
            output.outcome,
            InitOutcome::Updated {
                stale_owners: vec![InitStaleOwner::Builtin, InitStaleOwner::Rules],
            }
        );
        let requests = service
            .workspace_converger
            .requests
            .lock()
            .expect("workspace requests mutex should not be poisoned");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].source_game_root, Path::new("./Game"));
        assert_eq!(
            requests[0].source_language.as_ref().map(LanguageId::as_str),
            Some("ja")
        );
        assert_eq!(
            requests[0].target_language.as_ref().map(LanguageId::as_str),
            Some("zh-Hans")
        );
    }

    #[test]
    fn init_and_workspace_futures_are_send() {
        fn assert_send(_: impl Send) {}

        let state = database_state(0x11, Vec::new());
        let (workspace, _) = service(
            false,
            WorkspaceStructureObservation::Complete,
            ExistingSourceObservation::Missing,
            0x11,
            Ok(state.clone()),
            Ok(ProjectDatabaseReconciliation::for_test(state)),
            Ok(()),
        );
        assert_send(workspace.converge(request()));

        let init = InitService::new(
            FakeWorkspaceConverger {
                requests: Arc::new(Mutex::new(Vec::new())),
                responses: Arc::new(Mutex::new(VecDeque::from([Ok(
                    OperationCompletion::Completed(ProjectWorkspaceConvergence::Created),
                )]))),
            },
            FakeProjectLease,
            CooperativeCancellation::default(),
        );
        assert_send(init.execute(init_input()));
    }
}
