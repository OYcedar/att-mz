//! 候选 token 所有权、准备及目录交换终态。

use super::candidate::{build_candidate, validate_complete_candidate};
use super::error::{SystemFileSystemError, io_error};
use super::journal::{JournalPhase, JournalRecord, append_journal};
use super::lease::target_lock_path;
use super::path::validate_windows_name;
use super::recovery::recover_target;
#[cfg(test)]
use super::test_faults::{TestPublishFaultPoint, hit_test_publish_fault, injected_publish_error};
use super::workspace::{
    PUBLICATION_BACKUP_NAME, PUBLICATION_JOURNAL_NAME, PUBLICATION_STAGE_NAME, StageCleanupGuard,
    ensure_publication_workspace, identity_at, publication_workspace_root,
    remove_directory_tree_if_identity, remove_file_if_exists,
};
use super::{DirectoryPublisherConfig, SystemDirectoryPublisher};
use crate::diagnostic::FileSystemPathViolation;
use crate::runtime::performance::RunPerformanceCounters;
use crate::runtime::windows::{
    ExclusiveFileLock, FileIdentity, WindowsFsError, open_directory,
    rename_without_replace_if_identity, validate_local_case_insensitive_ntfs_directory,
};
use crate::storage::file_system::{
    DirectoryDiscardError, DirectoryPrepareError, DirectoryPublishError, DirectoryPublishIntent,
    DirectoryRecoveryError, DirectoryRecoveryOutcome, DirectoryStageRequest,
    RecoverableDirectoryPublisher, StagedDirectory, StagingCleanupFailure,
};
use std::fs::{self, File};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::{io, thread};
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

pub(crate) struct SystemStagingState {
    pub(super) publisher_identity: Arc<()>,
    parent_root: PathBuf,
    parent_identity: FileIdentity,
    pub(super) stage_identity: FileIdentity,
    target_lock: Option<ExclusiveFileLock>,
    parent_handle: Option<File>,
    stage_handle: Option<File>,
    journal_path: PathBuf,
    backup_path: PathBuf,
    cleanup: StageCleanupGuard,
    abandoned_before_delivery: bool,
    pub(super) finalized: bool,
}

impl SystemStagingState {
    fn mark_abandoned_before_delivery(&mut self) {
        self.abandoned_before_delivery = true;
    }
}

impl Drop for SystemStagingState {
    fn drop(&mut self) {
        if !self.finalized && !self.abandoned_before_delivery && !thread::panicking() {
            assert!(
                !self.cleanup.armed,
                "已准备目录 token 未经 publish/discard 直接丢弃"
            );
        }
    }
}

impl RecoverableDirectoryPublisher for SystemDirectoryPublisher {
    type Error = Box<SystemFileSystemError>;
    type StagingState = SystemStagingState;

    async fn recover(
        &self,
        target_root: PathBuf,
    ) -> Result<DirectoryRecoveryOutcome, DirectoryRecoveryError<Self::Error>> {
        let publisher_config = self.config.clone();
        let cancellation = self.inner.pool.cancellation();
        let error_target = target_root.clone();
        let result = self
            .inner
            .pool
            .execute("recover_directory_target", &error_target, move || {
                recover_directory_target_sync(target_root, &publisher_config, &cancellation)
            })
            .await
            .map_err(|source| {
                DirectoryRecoveryError::new(error_target.clone(), Box::new(source))
            })?;
        result.map_err(|source| DirectoryRecoveryError::new(error_target, Box::new(source)))
    }

    async fn prepare(
        &self,
        request: DirectoryStageRequest,
    ) -> Result<StagedDirectory<Self::StagingState>, DirectoryPrepareError<Self::Error>> {
        let target_root = request.target_root().to_path_buf();
        let publisher_config = self.config.clone();
        let publisher_identity = Arc::clone(&self.publisher_identity);
        let cancellation = self.inner.pool.cancellation();
        let materialization_width = self.inner.pool.width();
        let error_target = target_root.clone();
        self.inner
            .pool
            .execute_with_abandon(
                "prepare_directory_candidate",
                &error_target,
                move || {
                    prepare_directory_sync(
                        request,
                        publisher_config,
                        publisher_identity,
                        &cancellation,
                        materialization_width,
                    )
                },
                |result| {
                    if let Ok(staged) = result {
                        staged.state_mut().mark_abandoned_before_delivery();
                    }
                },
            )
            .await
            .map_err(|source| DirectoryPrepareError::NotPrepared {
                target_root: error_target,
                source: Box::new(source),
                cleanup_failure: None,
            })?
    }

    async fn publish(
        &self,
        staged: StagedDirectory<Self::StagingState>,
    ) -> Result<(), DirectoryPublishError<Self::Error>> {
        let expected_identity = Arc::clone(&self.publisher_identity);
        let performance = Arc::clone(&self.inner.performance);
        let target_root = staged.target_root().to_path_buf();
        // publish 按值接管已准备目录 token，必须运行至终态；取消只阻止新工作进入，
        // 不阻止已交付 token 的收尾，否则取消窗口内的发布会以 token 弃置断言告终。
        self.inner
            .pool
            .execute_terminal(move || {
                publish_directory_sync(staged, &expected_identity, &performance)
            })
            .await
            .map_err(|source| DirectoryPublishError::OutcomeUnknown {
                target_root,
                recovery_artifacts: Vec::new(),
                source: Box::new(source),
            })?
    }

    async fn discard(
        &self,
        staged: StagedDirectory<Self::StagingState>,
    ) -> Result<(), DirectoryDiscardError<Self::Error>> {
        let expected_identity = Arc::clone(&self.publisher_identity);
        let staging_root = staged.staging_root().to_path_buf();
        // discard 与 publish 同理：token 已交付，取消后仍必须完成候选清理。
        self.inner
            .pool
            .execute_terminal(move || discard_directory_sync(staged, &expected_identity))
            .await
            .map_err(|source| DirectoryDiscardError::new(staging_root.clone(), Box::new(source)))?
            .map_err(|source| DirectoryDiscardError::new(staging_root, Box::new(source)))
    }
}

fn recover_directory_target_sync(
    target_root: PathBuf,
    publisher_config: &DirectoryPublisherConfig,
    cancellation: &AtomicBool,
) -> Result<DirectoryRecoveryOutcome, SystemFileSystemError> {
    if !target_root.is_absolute() {
        return Err(SystemFileSystemError::InvalidPath {
            path: target_root,
            violation: FileSystemPathViolation::NotAbsolute,
        });
    }
    let parent = target_root
        .parent()
        .ok_or_else(|| SystemFileSystemError::InvalidPath {
            path: target_root.clone(),
            violation: FileSystemPathViolation::MissingParent,
        })?;
    let parent_root = validate_local_case_insensitive_ntfs_directory(parent)?;
    let parent_handle = open_directory(&parent_root, false)?;
    let target_name =
        target_root
            .file_name()
            .ok_or_else(|| SystemFileSystemError::InvalidPath {
                path: target_root.clone(),
                violation: FileSystemPathViolation::MissingFileName,
            })?;
    validate_windows_name(target_name, &target_root)?;
    let target_root = parent_root.join(target_name);
    let lock_path = target_lock_path(&publisher_config.lock_directory, &target_root)?;
    let target_lock = ExclusiveFileLock::acquire(&lock_path, cancellation)?;
    let outcome = recover_target(&target_root)?;
    drop(target_lock);
    drop(parent_handle);
    Ok(outcome)
}

fn prepare_directory_sync(
    request: DirectoryStageRequest,
    publisher_config: DirectoryPublisherConfig,
    publisher_identity: Arc<()>,
    cancellation: &AtomicBool,
    materialization_width: usize,
) -> Result<StagedDirectory<SystemStagingState>, DirectoryPrepareError<Box<SystemFileSystemError>>>
{
    let target_root = request.target_root().to_path_buf();
    let result: Result<_, PrepareSyncFailure> = (|| {
        if !target_root.is_absolute() {
            return Err(SystemFileSystemError::InvalidPath {
                path: target_root.clone(),
                violation: FileSystemPathViolation::NotAbsolute,
            }
            .into());
        }
        let parent = target_root
            .parent()
            .ok_or_else(|| SystemFileSystemError::InvalidPath {
                path: target_root.clone(),
                violation: FileSystemPathViolation::MissingParent,
            })?;
        let parent_root = validate_local_case_insensitive_ntfs_directory(parent)?;
        let parent_handle = open_directory(&parent_root, false)?;
        let parent_identity = FileIdentity::of(&parent_handle, &parent_root)?;
        let target_name =
            target_root
                .file_name()
                .ok_or_else(|| SystemFileSystemError::InvalidPath {
                    path: target_root.clone(),
                    violation: FileSystemPathViolation::MissingFileName,
                })?;
        validate_windows_name(target_name, &target_root)?;
        let target_root = parent_root.join(target_name);
        let lock_path = target_lock_path(&publisher_config.lock_directory, &target_root)?;
        let target_lock = ExclusiveFileLock::acquire(&lock_path, cancellation)?;
        let _ = recover_target(&target_root)?;

        let workspace_root = publication_workspace_root(&parent_root, target_name);
        ensure_publication_workspace(&workspace_root)?;
        let stage_root = workspace_root.join(PUBLICATION_STAGE_NAME);
        let backup_path = workspace_root.join(PUBLICATION_BACKUP_NAME);
        let journal_path = workspace_root.join(PUBLICATION_JOURNAL_NAME);
        fs::create_dir(&stage_root)
            .map_err(|source| io_error("建立目录候选", &stage_root, source))?;
        let stage_handle = open_directory(&stage_root, true).map_err(|source| {
            PrepareSyncFailure::Terminal(DirectoryPrepareError::NotPrepared {
                target_root: target_root.clone(),
                source: Box::new(source.into()),
                cleanup_failure: Some(StagingCleanupFailure::new(
                    stage_root.clone(),
                    Box::new(SystemFileSystemError::InvalidStagedIdentity {
                        path: stage_root.clone(),
                    }),
                )),
            })
        })?;
        let stage_identity = FileIdentity::of(&stage_handle, &stage_root).map_err(|source| {
            PrepareSyncFailure::Terminal(DirectoryPrepareError::NotPrepared {
                target_root: target_root.clone(),
                source: Box::new(source.into()),
                cleanup_failure: Some(StagingCleanupFailure::new(
                    stage_root.clone(),
                    Box::new(SystemFileSystemError::InvalidStagedIdentity {
                        path: stage_root.clone(),
                    }),
                )),
            })
        })?;
        let mut cleanup = StageCleanupGuard::new(stage_root.clone(), stage_identity);
        let build_result = build_candidate(
            &stage_root,
            &target_root,
            request.source_mappings(),
            request.overlays(),
            request.empty_directories(),
            cancellation,
            materialization_width,
        );
        if let Err(source) = build_result {
            return Err(prepare_terminal_failure(
                target_root,
                &stage_root,
                &mut cleanup,
                source,
            ));
        }
        Ok(StagedDirectory::new(
            target_root,
            stage_root,
            request.publish_intent(),
            SystemStagingState {
                publisher_identity,
                parent_root,
                parent_identity,
                stage_identity,
                target_lock: Some(target_lock),
                parent_handle: Some(parent_handle),
                stage_handle: Some(stage_handle),
                journal_path,
                backup_path,
                cleanup,
                abandoned_before_delivery: false,
                finalized: false,
            },
        ))
    })();

    match result {
        Ok(staged) => Ok(staged),
        Err(PrepareSyncFailure::Root(source)) => Err(DirectoryPrepareError::NotPrepared {
            target_root,
            source: Box::new(source),
            cleanup_failure: None,
        }),
        Err(PrepareSyncFailure::Terminal(error)) => Err(error),
    }
}

enum PrepareSyncFailure {
    Root(SystemFileSystemError),
    Terminal(DirectoryPrepareError<Box<SystemFileSystemError>>),
}

impl From<SystemFileSystemError> for PrepareSyncFailure {
    fn from(source: SystemFileSystemError) -> Self {
        Self::Root(source)
    }
}

impl From<WindowsFsError> for PrepareSyncFailure {
    fn from(source: WindowsFsError) -> Self {
        Self::Root(source.into())
    }
}

fn prepare_terminal_failure(
    target_root: PathBuf,
    stage_root: &Path,
    cleanup: &mut StageCleanupGuard,
    source: SystemFileSystemError,
) -> PrepareSyncFailure {
    let cleanup_failure = cleanup.cleanup().err().map(|cleanup_source| {
        StagingCleanupFailure::new(stage_root.to_path_buf(), Box::new(cleanup_source))
    });
    PrepareSyncFailure::Terminal(DirectoryPrepareError::NotPrepared {
        target_root,
        source: Box::new(source),
        cleanup_failure,
    })
}

fn publish_directory_sync(
    staged: StagedDirectory<SystemStagingState>,
    expected_identity: &Arc<()>,
    performance: &RunPerformanceCounters,
) -> Result<(), DirectoryPublishError<Box<SystemFileSystemError>>> {
    let (target_root, stage_root, intent, mut state) = staged.into_parts();
    state.finalized = true;
    if !Arc::ptr_eq(&state.publisher_identity, expected_identity) {
        let cleanup_failure = cleanup_state(&mut state, &stage_root);
        return Err(DirectoryPublishError::NotAttempted {
            target_root,
            source: Box::new(SystemFileSystemError::WrongPublisherInstance),
            cleanup_failure,
        });
    }
    if state.target_lock.is_none() {
        let cleanup_failure = cleanup_state(&mut state, &stage_root);
        return Err(DirectoryPublishError::NotAttempted {
            target_root,
            source: Box::new(SystemFileSystemError::InvalidStagedIdentity {
                path: state.parent_root.clone(),
            }),
            cleanup_failure,
        });
    }
    if let Err(source) = verify_staged_state(&state, &stage_root) {
        let artifacts = vec![stage_root.clone()];
        state.cleanup.disarm();
        return Err(DirectoryPublishError::OutcomeUnknown {
            target_root,
            recovery_artifacts: artifacts,
            source: Box::new(source),
        });
    }
    if let Err(source) = validate_complete_candidate(&stage_root, state.stage_identity, performance)
    {
        if matches!(&source, SystemFileSystemError::InvalidStagedIdentity { .. }) {
            state.cleanup.disarm();
            return Err(DirectoryPublishError::OutcomeUnknown {
                target_root,
                recovery_artifacts: vec![stage_root],
                source: Box::new(source),
            });
        }
        let cleanup_failure = cleanup_state(&mut state, &stage_root);
        return Err(DirectoryPublishError::NotAttempted {
            target_root,
            source: Box::new(source),
            cleanup_failure,
        });
    }
    match intent {
        DirectoryPublishIntent::CreateNew => {
            publish_create_new(target_root, stage_root, &mut state)
        }
        DirectoryPublishIntent::ReplaceExisting => {
            publish_replace(target_root, stage_root, &mut state)
        }
    }
}

fn verify_staged_state(
    state: &SystemStagingState,
    stage_root: &Path,
) -> Result<(), SystemFileSystemError> {
    let parent_handle = state.parent_handle.as_ref().ok_or_else(|| {
        SystemFileSystemError::InvalidStagedIdentity {
            path: state.parent_root.clone(),
        }
    })?;
    if FileIdentity::of(parent_handle, &state.parent_root)? != state.parent_identity {
        return Err(SystemFileSystemError::InvalidStagedIdentity {
            path: state.parent_root.clone(),
        });
    }
    let stage_handle = state.stage_handle.as_ref().ok_or_else(|| {
        SystemFileSystemError::InvalidStagedIdentity {
            path: stage_root.to_path_buf(),
        }
    })?;
    if FileIdentity::of(stage_handle, stage_root)? != state.stage_identity {
        return Err(SystemFileSystemError::InvalidStagedIdentity {
            path: stage_root.to_path_buf(),
        });
    }
    let current = open_directory(stage_root, true)?;
    if FileIdentity::of(&current, stage_root)? != state.stage_identity {
        return Err(SystemFileSystemError::InvalidStagedIdentity {
            path: stage_root.to_path_buf(),
        });
    }
    Ok(())
}

fn publish_create_new(
    target_root: PathBuf,
    stage_root: PathBuf,
    state: &mut SystemStagingState,
) -> Result<(), DirectoryPublishError<Box<SystemFileSystemError>>> {
    match fs::symlink_metadata(&target_root) {
        Ok(_) => {
            let cleanup_failure = cleanup_state(state, &stage_root);
            return Err(DirectoryPublishError::TargetAlreadyExists {
                target_root,
                cleanup_failure,
            });
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            let cleanup_failure = cleanup_state(state, &stage_root);
            let source = io_error("读取新建发布目标元数据", &target_root, source);
            return Err(DirectoryPublishError::NotAttempted {
                target_root,
                source: Box::new(source),
                cleanup_failure,
            });
        }
    }
    state.stage_handle.take();
    match rename_without_replace_if_identity(&stage_root, &target_root, state.stage_identity) {
        Ok(()) => {
            state.cleanup.disarm();
            match identity_at(&target_root) {
                Ok(Some(identity)) if identity == state.stage_identity => Ok(()),
                _ => Err(DirectoryPublishError::OutcomeUnknown {
                    target_root,
                    recovery_artifacts: Vec::new(),
                    source: Box::new(SystemFileSystemError::InvalidStagedIdentity {
                        path: stage_root,
                    }),
                }),
            }
        }
        Err(WindowsFsError::RenameTargetExists { .. }) => {
            let cleanup_failure = cleanup_state(state, &stage_root);
            Err(DirectoryPublishError::TargetAlreadyExists {
                target_root,
                cleanup_failure,
            })
        }
        Err(
            WindowsFsError::FileIdentityChanged { .. }
            | WindowsFsError::RenameTargetUnconfirmed { .. },
        ) => {
            state.cleanup.disarm();
            Err(DirectoryPublishError::OutcomeUnknown {
                target_root,
                recovery_artifacts: vec![stage_root.clone()],
                source: Box::new(SystemFileSystemError::InvalidStagedIdentity { path: stage_root }),
            })
        }
        Err(source) => {
            let cleanup_failure = cleanup_state(state, &stage_root);
            Err(DirectoryPublishError::NotPublished {
                target_root,
                source: Box::new(source.into()),
                cleanup_failure,
            })
        }
    }
}

fn publish_replace(
    target_root: PathBuf,
    stage_root: PathBuf,
    state: &mut SystemStagingState,
) -> Result<(), DirectoryPublishError<Box<SystemFileSystemError>>> {
    let target_metadata = match fs::symlink_metadata(&target_root) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            let cleanup_failure = cleanup_state(state, &stage_root);
            return Err(DirectoryPublishError::TargetMissing {
                target_root,
                cleanup_failure,
            });
        }
        Err(source) => {
            let cleanup_failure = cleanup_state(state, &stage_root);
            let source = io_error("读取替换目标元数据", &target_root, source);
            return Err(DirectoryPublishError::NotAttempted {
                target_root,
                source: Box::new(source),
                cleanup_failure,
            });
        }
    };
    if !target_metadata.is_dir()
        || target_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        let cleanup_failure = cleanup_state(state, &stage_root);
        return Err(DirectoryPublishError::TargetNotDirectory {
            target_root,
            cleanup_failure,
        });
    }
    let target_handle = match open_directory(&target_root, true) {
        Ok(handle) => handle,
        Err(source) => {
            let cleanup_failure = cleanup_state(state, &stage_root);
            return Err(DirectoryPublishError::NotAttempted {
                target_root,
                source: Box::new(source.into()),
                cleanup_failure,
            });
        }
    };
    let original_identity = match FileIdentity::of(&target_handle, &target_root) {
        Ok(identity) => identity,
        Err(source) => {
            let cleanup_failure = cleanup_state(state, &stage_root);
            return Err(DirectoryPublishError::NotAttempted {
                target_root,
                source: Box::new(source.into()),
                cleanup_failure,
            });
        }
    };
    drop(target_handle);
    let mut record = JournalRecord {
        target_name: target_root
            .file_name()
            .expect("受信目标必有名称")
            .encode_wide()
            .collect(),
        original_identity,
        candidate_identity: state.stage_identity,
        phase: JournalPhase::OriginalMoveIntent,
    };
    if let Err(source) = append_journal(&state.journal_path, &record, true) {
        let cleanup_failure = cleanup_state(state, &stage_root);
        let cleanup_failure = include_file_cleanup_failure(
            cleanup_failure,
            &state.journal_path,
            remove_file_if_exists(&state.journal_path),
        );
        return Err(DirectoryPublishError::NotPublished {
            target_root,
            source: Box::new(source),
            cleanup_failure,
        });
    }
    #[cfg(test)]
    {
        let _ = hit_test_publish_fault(&target_root, TestPublishFaultPoint::AfterOriginalJournal);
    }
    #[cfg(test)]
    if hit_test_publish_fault(&target_root, TestPublishFaultPoint::BeforeOriginalMove) {
        let source = injected_publish_error("移动旧目标", &target_root);
        let cleanup_failure = cleanup_state(state, &stage_root);
        let cleanup_failure = include_file_cleanup_failure(
            cleanup_failure,
            &state.journal_path,
            remove_file_if_exists(&state.journal_path),
        );
        return Err(DirectoryPublishError::NotPublished {
            target_root,
            source: Box::new(source),
            cleanup_failure,
        });
    }
    if let Err(source) =
        rename_without_replace_if_identity(&target_root, &state.backup_path, original_identity)
    {
        if matches!(
            source,
            WindowsFsError::FileIdentityChanged { .. }
                | WindowsFsError::RenameTargetUnconfirmed { .. }
        ) {
            state.cleanup.disarm();
            return Err(DirectoryPublishError::OutcomeUnknown {
                target_root: target_root.clone(),
                recovery_artifacts: vec![target_root, stage_root, state.journal_path.clone()],
                source: Box::new(source.into()),
            });
        }
        let cleanup_failure = cleanup_state(state, &stage_root);
        let cleanup_failure = include_file_cleanup_failure(
            cleanup_failure,
            &state.journal_path,
            remove_file_if_exists(&state.journal_path),
        );
        return Err(DirectoryPublishError::NotPublished {
            target_root,
            source: Box::new(source.into()),
            cleanup_failure,
        });
    }
    #[cfg(test)]
    {
        let _ = hit_test_publish_fault(&target_root, TestPublishFaultPoint::AfterOriginalMove);
    }
    record.phase = JournalPhase::CandidateMoveIntent;
    if let Err(source) = append_journal(&state.journal_path, &record, false) {
        return restore_old_after_failure(
            target_root,
            stage_root,
            state,
            original_identity,
            source,
            false,
        );
    }
    #[cfg(test)]
    {
        let _ = hit_test_publish_fault(&target_root, TestPublishFaultPoint::AfterCandidateIntent);
    }
    state.stage_handle.take();
    #[cfg(test)]
    if hit_test_publish_fault(&target_root, TestPublishFaultPoint::BeforeCandidateMove) {
        return restore_old_after_failure(
            target_root,
            stage_root.clone(),
            state,
            original_identity,
            injected_publish_error("移动候选目录", &stage_root),
            false,
        );
    }
    if let Err(source) =
        rename_without_replace_if_identity(&stage_root, &target_root, state.stage_identity)
    {
        let preserve_unknown_stage = matches!(
            source,
            WindowsFsError::FileIdentityChanged { .. }
                | WindowsFsError::RenameTargetUnconfirmed { .. }
        );
        let source = if preserve_unknown_stage {
            SystemFileSystemError::InvalidStagedIdentity {
                path: stage_root.clone(),
            }
        } else {
            source.into()
        };
        return restore_old_after_failure(
            target_root,
            stage_root,
            state,
            original_identity,
            source,
            preserve_unknown_stage,
        );
    }
    #[cfg(test)]
    {
        let _ = hit_test_publish_fault(&target_root, TestPublishFaultPoint::AfterCandidateMove);
    }
    state.cleanup.disarm();
    record.phase = JournalPhase::CandidateVisible;
    if let Err(source) = append_journal(&state.journal_path, &record, false) {
        return if identity_at(&target_root)
            .is_ok_and(|identity| identity == Some(state.stage_identity))
        {
            Err(DirectoryPublishError::PublishedWithResiduals {
                target_root,
                residual_path: state.journal_path.clone(),
                source: Box::new(source),
            })
        } else {
            Err(DirectoryPublishError::OutcomeUnknown {
                target_root,
                recovery_artifacts: vec![state.backup_path.clone(), state.journal_path.clone()],
                source: Box::new(source),
            })
        };
    }
    #[cfg(test)]
    {
        let _ = hit_test_publish_fault(&target_root, TestPublishFaultPoint::AfterCandidateVisible);
    }
    match identity_at(&target_root) {
        Ok(Some(identity)) if identity == state.stage_identity => {}
        Ok(_) | Err(_) => {
            return Err(DirectoryPublishError::OutcomeUnknown {
                target_root,
                recovery_artifacts: vec![state.backup_path.clone(), state.journal_path.clone()],
                source: Box::new(SystemFileSystemError::InvalidStagedIdentity { path: stage_root }),
            });
        }
    }
    #[cfg(test)]
    if hit_test_publish_fault(&target_root, TestPublishFaultPoint::BeforeBackupCleanup) {
        return Err(DirectoryPublishError::PublishedWithResiduals {
            target_root,
            residual_path: state.backup_path.clone(),
            source: Box::new(injected_publish_error(
                "清理目录发布备份",
                &state.backup_path,
            )),
        });
    }
    if let Err(source) = remove_directory_tree_if_identity(&state.backup_path, original_identity) {
        return Err(DirectoryPublishError::PublishedWithResiduals {
            target_root,
            residual_path: state.backup_path.clone(),
            source: Box::new(source),
        });
    }
    #[cfg(test)]
    if hit_test_publish_fault(&target_root, TestPublishFaultPoint::BeforeJournalCleanup) {
        return Err(DirectoryPublishError::PublishedWithResiduals {
            target_root,
            residual_path: state.journal_path.clone(),
            source: Box::new(injected_publish_error(
                "清理目录发布 journal",
                &state.journal_path,
            )),
        });
    }
    if let Err(source) = remove_file_if_exists(&state.journal_path) {
        return Err(DirectoryPublishError::PublishedWithResiduals {
            target_root,
            residual_path: state.journal_path.clone(),
            source: Box::new(source),
        });
    }
    Ok(())
}

fn restore_old_after_failure(
    target_root: PathBuf,
    stage_root: PathBuf,
    state: &mut SystemStagingState,
    original_identity: FileIdentity,
    source: SystemFileSystemError,
    preserve_unknown_stage: bool,
) -> Result<(), DirectoryPublishError<Box<SystemFileSystemError>>> {
    let restore_result: Result<(), SystemFileSystemError> = {
        #[cfg(test)]
        {
            if hit_test_publish_fault(&target_root, TestPublishFaultPoint::BeforeRestoreMove) {
                Err(injected_publish_error("恢复旧发布目标", &state.backup_path))
            } else {
                rename_without_replace_if_identity(
                    &state.backup_path,
                    &target_root,
                    original_identity,
                )
                .map_err(Into::into)
            }
        }
        #[cfg(not(test))]
        {
            rename_without_replace_if_identity(&state.backup_path, &target_root, original_identity)
                .map_err(Into::into)
        }
    };
    match restore_result {
        Ok(()) => {
            let cleanup_failure = cleanup_after_restore(state, &stage_root, preserve_unknown_stage);
            let cleanup_failure = include_file_cleanup_failure(
                cleanup_failure,
                &state.journal_path,
                remove_file_if_exists(&state.journal_path),
            );
            Err(DirectoryPublishError::NotPublished {
                target_root,
                source: Box::new(source),
                cleanup_failure,
            })
        }
        Err(restore) => {
            let target_identity = identity_at(&target_root);
            match target_identity {
                Ok(Some(identity)) if identity == state.stage_identity => {
                    state.cleanup.disarm();
                    Err(DirectoryPublishError::PublishedWithResiduals {
                        target_root,
                        residual_path: state.backup_path.clone(),
                        source: Box::new(restore),
                    })
                }
                Ok(Some(identity)) if identity == original_identity => {
                    let cleanup_failure =
                        cleanup_after_restore(state, &stage_root, preserve_unknown_stage);
                    let cleanup_failure = include_file_cleanup_failure(
                        cleanup_failure,
                        &state.journal_path,
                        remove_file_if_exists(&state.journal_path),
                    );
                    Err(DirectoryPublishError::NotPublished {
                        target_root,
                        source: Box::new(source),
                        cleanup_failure,
                    })
                }
                Ok(None)
                    if identity_at(&state.backup_path)
                        .is_ok_and(|identity| identity == Some(original_identity)) =>
                {
                    state.cleanup.disarm();
                    Err(DirectoryPublishError::RecoveryRequired {
                        target_root,
                        recovery_artifacts: vec![
                            state.backup_path.clone(),
                            stage_root,
                            state.journal_path.clone(),
                        ],
                        source: Box::new(restore),
                    })
                }
                Ok(_) | Err(_) => {
                    state.cleanup.disarm();
                    Err(DirectoryPublishError::OutcomeUnknown {
                        target_root,
                        recovery_artifacts: vec![
                            state.backup_path.clone(),
                            stage_root,
                            state.journal_path.clone(),
                        ],
                        source: Box::new(restore),
                    })
                }
            }
        }
    }
}

fn cleanup_after_restore(
    state: &mut SystemStagingState,
    stage_root: &Path,
    preserve_unknown_stage: bool,
) -> Option<StagingCleanupFailure<Box<SystemFileSystemError>>> {
    if preserve_unknown_stage {
        state.stage_handle.take();
        state.cleanup.disarm();
        Some(StagingCleanupFailure::new(
            stage_root.to_path_buf(),
            Box::new(SystemFileSystemError::InvalidStagedIdentity {
                path: stage_root.to_path_buf(),
            }),
        ))
    } else {
        cleanup_state(state, stage_root)
    }
}

fn cleanup_state(
    state: &mut SystemStagingState,
    stage_root: &Path,
) -> Option<StagingCleanupFailure<Box<SystemFileSystemError>>> {
    state.stage_handle.take();
    state
        .cleanup
        .cleanup()
        .err()
        .map(|source| StagingCleanupFailure::new(stage_root.to_path_buf(), Box::new(source)))
}

fn include_file_cleanup_failure(
    existing: Option<StagingCleanupFailure<Box<SystemFileSystemError>>>,
    residual_path: &Path,
    result: Result<(), SystemFileSystemError>,
) -> Option<StagingCleanupFailure<Box<SystemFileSystemError>>> {
    match (existing, result) {
        (Some(failure), _) => Some(failure),
        (None, Ok(())) => None,
        (None, Err(source)) => Some(StagingCleanupFailure::new(
            residual_path.to_path_buf(),
            Box::new(source),
        )),
    }
}

fn discard_directory_sync(
    staged: StagedDirectory<SystemStagingState>,
    expected_identity: &Arc<()>,
) -> Result<(), SystemFileSystemError> {
    let (_target_root, stage_root, _intent, mut state) = staged.into_parts();
    state.finalized = true;
    if !Arc::ptr_eq(&state.publisher_identity, expected_identity) {
        return Err(SystemFileSystemError::WrongPublisherInstance);
    }
    verify_staged_state(&state, &stage_root)?;
    state.stage_handle.take();
    state.cleanup.cleanup().map_err(|source| match source {
        SystemFileSystemError::Io { .. } => source,
        other => io_error(
            "丢弃目录候选",
            &stage_root,
            io::Error::other(other.to_string()),
        ),
    })
}

#[cfg(test)]
mod tests;
