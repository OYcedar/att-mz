//! 项目租约与目录发布锁的自然路径及独占获取。

use super::SystemFileSystem;
use super::access::absolutize;
use super::error::{SystemFileSystemError, io_error};
use super::path::validate_windows_name;
use crate::diagnostic::FileSystemPathViolation;
use crate::runtime::windows::{ExclusiveFileLock, pin_directory_without_reparse};
use crate::storage::file_system::{
    ExclusiveFileLease, ExclusiveFileLeaseError, ExclusiveFileLeaseProvider,
    ExclusiveFileLeaseRequest,
};
use std::ffi::OsStr;
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::{fs, io};
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

impl ExclusiveFileLeaseProvider for SystemFileSystem {
    type Error = Box<SystemFileSystemError>;
    type LeaseState = ExclusiveFileLock;

    fn acquire_exclusive_file_lease(
        &self,
        request: ExclusiveFileLeaseRequest,
    ) -> impl std::future::Future<
        Output = Result<ExclusiveFileLease<Self::LeaseState>, ExclusiveFileLeaseError<Self::Error>>,
    > + Send {
        let inner = Arc::clone(&self.inner);
        let cancellation = inner.pool.cancellation();
        let identity = request.identity().to_os_string();
        let lock_directory = request.lock_directory().to_path_buf();
        async move {
            let failure_identity = identity.clone();
            inner
                .pool
                .execute("acquire_exclusive_file_lease", &lock_directory, move || {
                    acquire_exclusive_file_lease_sync(request, &cancellation)
                })
                .await
                .map_err(|source| ExclusiveFileLeaseError::Unavailable {
                    identity: failure_identity,
                    source: Box::new(source),
                })?
        }
    }
}

fn acquire_exclusive_file_lease_sync(
    request: ExclusiveFileLeaseRequest,
    cancellation: &AtomicBool,
) -> Result<
    ExclusiveFileLease<ExclusiveFileLock>,
    ExclusiveFileLeaseError<Box<SystemFileSystemError>>,
> {
    let identity = request.identity().to_os_string();
    let result: Result<_, SystemFileSystemError> = (|| {
        let lock_directory = trusted_lock_directory(request.lock_directory())?;
        let lock_path = stable_lock_path(&lock_directory, request.identity())?;
        ExclusiveFileLock::acquire(&lock_path, cancellation)
            .map(ExclusiveFileLease::new)
            .map_err(Into::into)
    })();
    match result {
        Ok(lease) => Ok(lease),
        Err(source) => Err(ExclusiveFileLeaseError::Unavailable {
            identity,
            source: Box::new(source),
        }),
    }
}

pub(super) fn target_lock_path(
    configured_lock_directory: &Path,
    target_root: &Path,
) -> Result<PathBuf, SystemFileSystemError> {
    let lock_directory = trusted_lock_directory(configured_lock_directory)?;
    let target_name =
        target_root
            .file_name()
            .ok_or_else(|| SystemFileSystemError::InvalidPath {
                path: target_root.to_path_buf(),
                violation: FileSystemPathViolation::MissingFileName,
            })?;
    stable_lock_path(&lock_directory, target_name)
}

fn trusted_lock_directory(path: &Path) -> Result<PathBuf, SystemFileSystemError> {
    let lock_directory = absolutize(path.to_path_buf())?;
    let mut current = PathBuf::new();
    for component in lock_directory.components() {
        current.push(component.as_os_str());
        if matches!(component, Component::Prefix(_) | Component::RootDir) {
            continue;
        }
        match fs::create_dir(&current) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
            Err(source) => return Err(io_error("建立锁目录", &current, source)),
        }
        let metadata = fs::symlink_metadata(&current)
            .map_err(|source| io_error("读取锁目录元数据", &current, source))?;
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(SystemFileSystemError::InvalidPath {
                path: current,
                violation: FileSystemPathViolation::ReparsePoint,
            });
        }
    }
    let pinned = pin_directory_without_reparse(&lock_directory)?;
    Ok(pinned.resolved_path().to_path_buf())
}

fn stable_lock_path(
    lock_directory: &Path,
    identity: &OsStr,
) -> Result<PathBuf, SystemFileSystemError> {
    validate_windows_name(identity, Path::new(identity))?;
    Ok(lock_directory.join(identity))
}

#[cfg(test)]
mod tests;
