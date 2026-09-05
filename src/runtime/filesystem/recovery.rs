//! 以 journal 为权威的同目标目录恢复。

use super::error::{SystemFileSystemError, io_error};
use super::journal::read_journal;
use super::path::windows_ordinal_case_key_from_utf16;
#[cfg(test)]
use super::test_faults::{TestPublishFaultPoint, hit_test_publish_fault, injected_publish_error};
use super::workspace::{
    PUBLICATION_BACKUP_NAME, PUBLICATION_JOURNAL_NAME, PUBLICATION_STAGE_NAME, identity_at,
    publication_workspace_root, remove_directory_tree_if_identity, remove_file_if_exists,
};
use crate::diagnostic::FileSystemRecoveryViolation;
use crate::runtime::windows::{
    FileIdentity, pin_directory_without_reparse, rename_without_replace_if_identity,
};
use crate::storage::file_system::DirectoryRecoveryOutcome;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::{fs, io};
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

fn recovery_cleanup_failed(
    target_root: &Path,
    artifacts: Vec<PathBuf>,
    source: SystemFileSystemError,
) -> SystemFileSystemError {
    SystemFileSystemError::RecoveryCleanupFailed {
        target_root: target_root.to_path_buf(),
        artifacts,
        source: Box::new(source),
    }
}

fn published_recovery_cleanup_failed(
    target_root: &Path,
    artifacts: Vec<PathBuf>,
    source: SystemFileSystemError,
) -> SystemFileSystemError {
    SystemFileSystemError::PublishedRecoveryCleanupFailed {
        target_root: target_root.to_path_buf(),
        artifacts,
        source: Box::new(source),
    }
}

fn recovery_outcome_unknown(
    target_root: &Path,
    artifacts: Vec<PathBuf>,
    source: SystemFileSystemError,
) -> SystemFileSystemError {
    SystemFileSystemError::RecoveryOutcomeUnknown {
        target_root: target_root.to_path_buf(),
        artifacts,
        source: Box::new(source),
    }
}

pub(super) fn recover_target(
    target_root: &Path,
) -> Result<DirectoryRecoveryOutcome, SystemFileSystemError> {
    let parent = target_root.parent().expect("受信发布目标必有父目录");
    let target_name = target_root.file_name().expect("受信发布目标必有名称");
    let workspace_root = publication_workspace_root(parent, target_name);
    let artifacts = scan_recovery_artifacts(target_root, &workspace_root, &[])?;
    validate_recovery_artifact_names(target_root, &workspace_root, &artifacts)?;
    let mut changed = false;
    let journal = workspace_root.join(PUBLICATION_JOURNAL_NAME);
    if artifacts.contains(&journal) {
        recover_journal(target_root, &journal, &artifacts)?;
        changed = true;
    }

    // journal 恢复完成后重新列举；错误只报告这次确实观察到、仍未处理的路径。
    let scanned_residuals = scan_recovery_artifacts(target_root, &workspace_root, &artifacts)?;
    validate_recovery_artifact_names(target_root, &workspace_root, &scanned_residuals)?;
    let mut residuals = Vec::new();
    let mut metadata = Vec::new();
    for (index, artifact) in scanned_residuals.iter().enumerate() {
        let observed = match fs::symlink_metadata(artifact) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(recovery_outcome_unknown(
                    target_root,
                    merge_recovery_artifacts(&residuals, &scanned_residuals[index..]),
                    io_error("读取目录恢复产物元数据", artifact, source),
                ));
            }
        };
        residuals.push(artifact.clone());
        metadata.push(observed);
    }
    for (artifact, metadata) in residuals.iter().zip(&metadata) {
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(SystemFileSystemError::OutcomeUnknown {
                target_root: target_root.to_path_buf(),
                artifacts: residuals.clone(),
                violation: FileSystemRecoveryViolation::ArtifactReparsePoint,
            });
        }
        if artifact.file_name() != Some(OsStr::new(PUBLICATION_STAGE_NAME)) {
            return Err(SystemFileSystemError::OutcomeUnknown {
                target_root: target_root.to_path_buf(),
                artifacts: residuals.clone(),
                violation: FileSystemRecoveryViolation::UnexpectedResidualArtifact,
            });
        }
    }

    // 能到这里的只可能是尚未建立 journal 的孤立候选；它们从未接管目标。
    for (index, stage) in residuals.iter().enumerate() {
        let cleanup = (|| {
            let pinned = pin_directory_without_reparse(stage)?;
            let identity = FileIdentity::of(pinned.file(), stage)?;
            drop(pinned);
            remove_directory_tree_if_identity(stage, identity)
        })();
        if let Err(source) = cleanup {
            return Err(recovery_cleanup_failed(
                target_root,
                residuals[index..].to_vec(),
                source,
            ));
        }
        changed = true;
    }
    Ok(if changed {
        DirectoryRecoveryOutcome::Recovered
    } else {
        DirectoryRecoveryOutcome::Unchanged
    })
}

fn scan_recovery_artifacts(
    target_root: &Path,
    workspace_root: &Path,
    last_known: &[PathBuf],
) -> Result<Vec<PathBuf>, SystemFileSystemError> {
    match fs::symlink_metadata(workspace_root) {
        Ok(metadata) => {
            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                return Err(SystemFileSystemError::OutcomeUnknown {
                    target_root: target_root.to_path_buf(),
                    artifacts: vec![workspace_root.to_path_buf()],
                    violation: FileSystemRecoveryViolation::ArtifactReparsePoint,
                });
            }
            if !metadata.is_dir() {
                return Err(SystemFileSystemError::OutcomeUnknown {
                    target_root: target_root.to_path_buf(),
                    artifacts: vec![workspace_root.to_path_buf()],
                    violation: FileSystemRecoveryViolation::UnexpectedResidualArtifact,
                });
            }
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(recovery_outcome_unknown(
                target_root,
                last_known.to_vec(),
                io_error("读取目录发布工作目录", workspace_root, source),
            ));
        }
    }
    let mut artifacts = Vec::new();
    let entries = fs::read_dir(workspace_root).map_err(|source| {
        recovery_outcome_unknown(
            target_root,
            last_known.to_vec(),
            io_error("列举目录恢复产物", workspace_root, source),
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| {
            recovery_outcome_unknown(
                target_root,
                merge_recovery_artifacts(last_known, &artifacts),
                io_error("读取目录恢复产物", workspace_root, source),
            )
        })?;
        artifacts.push(entry.path());
    }
    artifacts.sort();
    Ok(artifacts)
}

fn validate_recovery_artifact_names(
    target_root: &Path,
    workspace_root: &Path,
    artifacts: &[PathBuf],
) -> Result<(), SystemFileSystemError> {
    let expected = [
        workspace_root.join(PUBLICATION_STAGE_NAME),
        workspace_root.join(PUBLICATION_BACKUP_NAME),
        workspace_root.join(PUBLICATION_JOURNAL_NAME),
    ];
    if artifacts.iter().all(|path| expected.contains(path)) {
        Ok(())
    } else {
        Err(SystemFileSystemError::OutcomeUnknown {
            target_root: target_root.to_path_buf(),
            artifacts: artifacts.to_vec(),
            violation: FileSystemRecoveryViolation::UnexpectedResidualArtifact,
        })
    }
}

fn merge_recovery_artifacts(left: &[PathBuf], right: &[PathBuf]) -> Vec<PathBuf> {
    left.iter()
        .chain(right)
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn pending_operation_artifacts(
    observed_artifacts: &[PathBuf],
    _journal: &Path,
    current_operation: impl IntoIterator<Item = PathBuf>,
) -> Vec<PathBuf> {
    observed_artifacts
        .iter()
        .cloned()
        .chain(current_operation)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn recover_journal(
    target_root: &Path,
    journal: &Path,
    observed_artifacts: &[PathBuf],
) -> Result<(), SystemFileSystemError> {
    let workspace_root = journal.parent().expect("受信 journal 必有工作目录");
    let records = match read_journal(journal) {
        Ok(records) => records,
        Err(SystemFileSystemError::JournalCorrupt { path, violation }) => {
            return Err(SystemFileSystemError::RecoveryJournalCorrupt {
                path,
                artifacts: observed_artifacts.to_vec(),
                violation,
            });
        }
        Err(source @ SystemFileSystemError::RecoveryJournalCorrupt { .. })
        | Err(source @ SystemFileSystemError::RecoveryRequired { .. })
        | Err(source @ SystemFileSystemError::RecoveryCleanupFailed { .. })
        | Err(source @ SystemFileSystemError::PublishedRecoveryCleanupFailed { .. })
        | Err(source @ SystemFileSystemError::RecoveryOutcomeUnknown { .. })
        | Err(source @ SystemFileSystemError::OutcomeUnknown { .. }) => return Err(source),
        Err(source) => {
            return Err(recovery_outcome_unknown(
                target_root,
                observed_artifacts.to_vec(),
                source,
            ));
        }
    };
    if records.is_empty() {
        remove_file_if_exists(journal).map_err(|source| {
            recovery_cleanup_failed(target_root, observed_artifacts.to_vec(), source)
        })?;
        return Ok(());
    }
    let record = records.last().expect("非空 journal 必有末帧");
    let target_name: Vec<u16> = target_root
        .file_name()
        .expect("受信发布目标必有名称")
        .encode_wide()
        .collect();
    let recorded_target_key = windows_ordinal_case_key_from_utf16(&record.target_name, journal)
        .map_err(|source| {
            recovery_outcome_unknown(target_root, observed_artifacts.to_vec(), source)
        })?;
    let requested_target_key = windows_ordinal_case_key_from_utf16(&target_name, target_root)
        .map_err(|source| {
            recovery_outcome_unknown(target_root, observed_artifacts.to_vec(), source)
        })?;
    if recorded_target_key != requested_target_key {
        return Err(SystemFileSystemError::OutcomeUnknown {
            target_root: target_root.to_path_buf(),
            artifacts: observed_artifacts.to_vec(),
            violation: FileSystemRecoveryViolation::TargetNameMismatch,
        });
    }
    let stage = workspace_root.join(PUBLICATION_STAGE_NAME);
    let backup = workspace_root.join(PUBLICATION_BACKUP_NAME);
    let target_identity = identity_at(target_root).map_err(|source| {
        recovery_outcome_unknown(target_root, observed_artifacts.to_vec(), source)
    })?;
    if target_identity == Some(record.candidate_identity) {
        let stage_identity = identity_at(&stage).map_err(|source| {
            published_recovery_cleanup_failed(target_root, observed_artifacts.to_vec(), source)
        })?;
        let backup_identity = identity_at(&backup).map_err(|source| {
            published_recovery_cleanup_failed(target_root, observed_artifacts.to_vec(), source)
        })?;
        let mut current = Vec::new();
        if backup_identity.is_some() {
            current.push(backup.clone());
        }
        if stage_identity.is_some() {
            current.push(stage.clone());
        }
        current.push(journal.to_path_buf());
        let pending = pending_operation_artifacts(observed_artifacts, journal, current);
        #[cfg(test)]
        if hit_test_publish_fault(target_root, TestPublishFaultPoint::BeforeRecoveryCleanup) {
            return Err(published_recovery_cleanup_failed(
                target_root,
                pending,
                injected_publish_error("清理已发布目录的恢复产物", journal),
            ));
        }
        remove_matching_directory(&backup, record.original_identity)
            .map_err(|source| published_recovery_cleanup_failed(target_root, pending, source))?;
        let mut current = Vec::new();
        if stage_identity.is_some() {
            current.push(stage.clone());
        }
        current.push(journal.to_path_buf());
        let pending = pending_operation_artifacts(observed_artifacts, journal, current);
        #[cfg(test)]
        if hit_test_publish_fault(target_root, TestPublishFaultPoint::BeforeRecoveryCleanup) {
            return Err(published_recovery_cleanup_failed(
                target_root,
                pending,
                injected_publish_error("清理已发布目录的恢复产物", journal),
            ));
        }
        remove_matching_directory(&stage, record.candidate_identity)
            .map_err(|source| published_recovery_cleanup_failed(target_root, pending, source))?;
        remove_file_if_exists(journal).map_err(|source| {
            published_recovery_cleanup_failed(
                target_root,
                pending_operation_artifacts(observed_artifacts, journal, [journal.to_path_buf()]),
                source,
            )
        })?;
        return Ok(());
    }
    if target_identity == Some(record.original_identity) {
        let stage_identity = identity_at(&stage).map_err(|source| {
            recovery_cleanup_failed(target_root, observed_artifacts.to_vec(), source)
        })?;
        let backup_identity = identity_at(&backup).map_err(|source| {
            recovery_cleanup_failed(target_root, observed_artifacts.to_vec(), source)
        })?;
        let mut current = Vec::new();
        if stage_identity.is_some() {
            current.push(stage.clone());
        }
        if backup_identity.is_some() {
            current.push(backup.clone());
        }
        current.push(journal.to_path_buf());
        let pending = pending_operation_artifacts(observed_artifacts, journal, current);
        remove_matching_directory(&stage, record.candidate_identity)
            .map_err(|source| recovery_cleanup_failed(target_root, pending, source))?;
        let mut current = Vec::new();
        if backup_identity.is_some() {
            current.push(backup.clone());
        }
        current.push(journal.to_path_buf());
        let pending = pending_operation_artifacts(observed_artifacts, journal, current);
        remove_matching_directory(&backup, record.original_identity)
            .map_err(|source| recovery_cleanup_failed(target_root, pending, source))?;
        remove_file_if_exists(journal).map_err(|source| {
            recovery_cleanup_failed(
                target_root,
                pending_operation_artifacts(observed_artifacts, journal, [journal.to_path_buf()]),
                source,
            )
        })?;
        return Ok(());
    }
    if target_identity.is_some() {
        return Err(SystemFileSystemError::OutcomeUnknown {
            target_root: target_root.to_path_buf(),
            artifacts: observed_artifacts.to_vec(),
            violation: FileSystemRecoveryViolation::TargetIdentityUnknown,
        });
    }
    let stage_identity = identity_at(&stage).map_err(|source| {
        recovery_outcome_unknown(target_root, observed_artifacts.to_vec(), source)
    })?;
    let backup_identity = identity_at(&backup).map_err(|source| {
        recovery_outcome_unknown(target_root, observed_artifacts.to_vec(), source)
    })?;
    if backup_identity == Some(record.original_identity) {
        if let Err(source) =
            rename_without_replace_if_identity(&backup, target_root, record.original_identity)
        {
            return Err(recovery_outcome_unknown(
                target_root,
                observed_artifacts.to_vec(),
                source.into(),
            ));
        }
        let mut post_restore_current = Vec::new();
        if stage_identity.is_some() {
            post_restore_current.push(stage.clone());
        }
        post_restore_current.push(journal.to_path_buf());
        let post_restore_artifacts =
            pending_operation_artifacts(observed_artifacts, journal, post_restore_current);
        let restored_identity = identity_at(target_root).map_err(|source| {
            recovery_outcome_unknown(target_root, post_restore_artifacts.clone(), source)
        })?;
        if restored_identity != Some(record.original_identity) {
            return Err(SystemFileSystemError::OutcomeUnknown {
                target_root: target_root.to_path_buf(),
                artifacts: post_restore_artifacts,
                violation: FileSystemRecoveryViolation::RestoredIdentityMismatch,
            });
        }
        #[cfg(test)]
        if hit_test_publish_fault(target_root, TestPublishFaultPoint::BeforeRecoveryCleanup) {
            let mut current = Vec::new();
            if stage_identity.is_some() {
                current.push(stage.clone());
            }
            current.push(journal.to_path_buf());
            let pending = pending_operation_artifacts(observed_artifacts, journal, current);
            return Err(recovery_cleanup_failed(
                target_root,
                pending,
                injected_publish_error("清理已恢复目录的受管产物", journal),
            ));
        }
        if stage_identity == Some(record.candidate_identity) {
            remove_directory_tree_if_identity(&stage, record.candidate_identity).map_err(
                |source| {
                    recovery_cleanup_failed(
                        target_root,
                        pending_operation_artifacts(
                            observed_artifacts,
                            journal,
                            [stage.clone(), journal.to_path_buf()],
                        ),
                        source,
                    )
                },
            )?;
        } else if stage_identity.is_some() {
            return Err(recovery_cleanup_failed(
                target_root,
                pending_operation_artifacts(
                    observed_artifacts,
                    journal,
                    [stage.clone(), journal.to_path_buf()],
                ),
                SystemFileSystemError::InvalidStagedIdentity { path: stage },
            ));
        }
        remove_file_if_exists(journal).map_err(|source| {
            recovery_cleanup_failed(
                target_root,
                pending_operation_artifacts(observed_artifacts, journal, [journal.to_path_buf()]),
                source,
            )
        })?;
        return Ok(());
    }
    if backup_identity.is_some() {
        return Err(SystemFileSystemError::OutcomeUnknown {
            target_root: target_root.to_path_buf(),
            artifacts: observed_artifacts.to_vec(),
            violation: FileSystemRecoveryViolation::BackupIdentityUnknown,
        });
    }
    let mut current = Vec::new();
    if stage_identity.is_some() {
        current.push(stage);
    }
    current.push(journal.to_path_buf());
    let pending = pending_operation_artifacts(observed_artifacts, journal, current);
    Err(SystemFileSystemError::RecoveryRequired {
        target_root: target_root.to_path_buf(),
        artifacts: pending,
        violation: FileSystemRecoveryViolation::OriginalAndTargetMissing,
    })
}

fn remove_matching_directory(
    path: &Path,
    expected: FileIdentity,
) -> Result<(), SystemFileSystemError> {
    match identity_at(path)? {
        None => Ok(()),
        Some(identity) if identity == expected => remove_directory_tree_if_identity(path, expected),
        Some(_) => Err(SystemFileSystemError::InvalidStagedIdentity {
            path: path.to_path_buf(),
        }),
    }
}

#[cfg(test)]
mod tests;
