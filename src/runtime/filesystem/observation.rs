//! 终态非权威文件的原子提交与精确临时文件清理。

#[cfg(test)]
use super::error::{OBSERVATION_CLEANUP_OPERATION, OBSERVATION_COMMIT_OPERATION};
use super::error::{
    OBSERVATION_CREATE_OPERATION, OBSERVATION_FLUSH_OPERATION, OBSERVATION_SYNC_OPERATION,
    OBSERVATION_WRITE_OPERATION, SystemFileSystemError, io_error,
};
#[cfg(test)]
use super::test_faults::{TestObservationFaultPoint, hit_test_observation_fault};
use crate::diagnostic::{FileSystemPathViolation, FileSystemRecoveryViolation};
use crate::runtime::windows::{
    FileIdentity, WindowsFsError, create_directories_without_reparse,
    create_new_atomic_replace_candidate, delete_open_atomic_replace_candidate,
    rename_open_atomic_replace_candidate_without_replace,
};
use std::ffi::OsString;
use std::fs::File;
#[cfg(test)]
use std::io;
use std::io::Write;
use std::path::Path;

pub(super) fn write_new_terminal_observation_file_sync(
    path: &Path,
    bytes: &[u8],
) -> Result<(), SystemFileSystemError> {
    let parent = path
        .parent()
        .ok_or_else(|| SystemFileSystemError::InvalidPath {
            path: path.to_path_buf(),
            violation: FileSystemPathViolation::MissingParent,
        })?;
    let file_name = path
        .file_name()
        .ok_or_else(|| SystemFileSystemError::InvalidPath {
            path: path.to_path_buf(),
            violation: FileSystemPathViolation::MissingFileName,
        })?;
    let pinned_parent = create_directories_without_reparse(parent)?;
    let resolved_path = pinned_parent.resolved_path().join(file_name);
    let mut temporary_name = OsString::from(".");
    temporary_name.push(file_name);
    temporary_name.push(".tmp");
    let temporary_path = pinned_parent.resolved_path().join(temporary_name);

    let mut file =
        create_new_atomic_replace_candidate(&temporary_path).map_err(|source| match source {
            WindowsFsError::Io { source, .. } => {
                io_error(OBSERVATION_CREATE_OPERATION, &temporary_path, source)
            }
            source => source.into(),
        })?;
    let temporary_identity = terminal_observation_candidate_identity(
        &file,
        &temporary_path,
        FileIdentity::of(&file, &temporary_path),
    )?;

    #[cfg(test)]
    if hit_test_observation_fault(path, TestObservationFaultPoint::AfterPartialWrite) {
        let partial_length = bytes.len().min(1);
        if partial_length > 0
            && let Err(source) = file.write_all(&bytes[..partial_length])
        {
            let operation = io_error(OBSERVATION_WRITE_OPERATION, &temporary_path, source);
            return Err(cleanup_open_terminal_observation(
                path,
                &file,
                &temporary_path,
                operation,
            ));
        }
        let operation = io_error(
            OBSERVATION_WRITE_OPERATION,
            &temporary_path,
            io::Error::other("测试注入的部分写入故障"),
        );
        return Err(cleanup_open_terminal_observation(
            path,
            &file,
            &temporary_path,
            operation,
        ));
    }

    if let Err(source) = file.write_all(bytes) {
        let operation = io_error(OBSERVATION_WRITE_OPERATION, &temporary_path, source);
        return Err(cleanup_open_terminal_observation(
            path,
            &file,
            &temporary_path,
            operation,
        ));
    }
    #[cfg(test)]
    if hit_test_observation_fault(path, TestObservationFaultPoint::BeforeFlush) {
        let operation = io_error(
            OBSERVATION_FLUSH_OPERATION,
            &temporary_path,
            io::Error::other("测试注入的 flush 故障"),
        );
        return Err(cleanup_open_terminal_observation(
            path,
            &file,
            &temporary_path,
            operation,
        ));
    }
    if let Err(source) = file.flush() {
        let operation = io_error(OBSERVATION_FLUSH_OPERATION, &temporary_path, source);
        return Err(cleanup_open_terminal_observation(
            path,
            &file,
            &temporary_path,
            operation,
        ));
    }
    #[cfg(test)]
    if hit_test_observation_fault(path, TestObservationFaultPoint::BeforeSync) {
        let operation = io_error(
            OBSERVATION_SYNC_OPERATION,
            &temporary_path,
            io::Error::other("测试注入的 sync 故障"),
        );
        return Err(cleanup_open_terminal_observation(
            path,
            &file,
            &temporary_path,
            operation,
        ));
    }
    if let Err(source) = file.sync_all() {
        let operation = io_error(OBSERVATION_SYNC_OPERATION, &temporary_path, source);
        return Err(cleanup_open_terminal_observation(
            path,
            &file,
            &temporary_path,
            operation,
        ));
    }
    #[cfg(test)]
    if hit_test_observation_fault(path, TestObservationFaultPoint::BeforeRename) {
        let operation = io_error(
            OBSERVATION_COMMIT_OPERATION,
            &resolved_path,
            io::Error::other("测试注入的重命名故障"),
        );
        return Err(cleanup_open_terminal_observation(
            path,
            &file,
            &temporary_path,
            operation,
        ));
    }

    let renamed = rename_open_atomic_replace_candidate_without_replace(
        file,
        &temporary_path,
        &resolved_path,
        temporary_identity,
    );
    match renamed {
        Ok(()) => Ok(()),
        Err(failure) => {
            let (source, candidate) = failure.into_parts();
            match candidate {
                Some(candidate) => Err(cleanup_open_terminal_observation(
                    path,
                    &candidate,
                    &temporary_path,
                    source.into(),
                )),
                None => {
                    finish_terminal_observation_rename(&resolved_path, &temporary_path, Err(source))
                }
            }
        }
    }
}

fn finish_terminal_observation_rename(
    resolved_path: &Path,
    temporary_path: &Path,
    renamed: Result<(), WindowsFsError>,
) -> Result<(), SystemFileSystemError> {
    match renamed {
        Ok(()) => Ok(()),
        Err(WindowsFsError::RenameTargetUnconfirmed { .. }) => {
            Err(SystemFileSystemError::OutcomeUnknown {
                target_root: resolved_path.to_path_buf(),
                artifacts: vec![temporary_path.to_path_buf()],
                violation: FileSystemRecoveryViolation::TargetIdentityUnknown,
            })
        }
        Err(source) => Err(source.into()),
    }
}

fn terminal_observation_candidate_identity(
    file: &File,
    temporary_path: &Path,
    identity: Result<FileIdentity, WindowsFsError>,
) -> Result<FileIdentity, SystemFileSystemError> {
    identity.map_err(|source| {
        let operation = SystemFileSystemError::from(source);
        cleanup_open_terminal_observation(temporary_path, file, temporary_path, operation)
    })
}

fn cleanup_open_terminal_observation(
    _requested_path: &Path,
    file: &File,
    temporary_path: &Path,
    operation: SystemFileSystemError,
) -> SystemFileSystemError {
    #[cfg(test)]
    let cleanup =
        if hit_test_observation_fault(_requested_path, TestObservationFaultPoint::BeforeCleanup) {
            Err(io_error(
                OBSERVATION_CLEANUP_OPERATION,
                temporary_path,
                io::Error::other("测试注入的临时文件清理故障"),
            ))
        } else {
            delete_open_atomic_replace_candidate(file, temporary_path).map_err(Into::into)
        };
    #[cfg(not(test))]
    let cleanup = delete_open_atomic_replace_candidate(file, temporary_path).map_err(Into::into);

    match cleanup {
        Ok(()) => operation,
        Err(cleanup) => SystemFileSystemError::ObservationCleanupFailed {
            temporary_path: temporary_path.to_path_buf(),
            operation: Box::new(operation),
            cleanup: Box::new(cleanup),
        },
    }
}

#[cfg(test)]
mod tests;
