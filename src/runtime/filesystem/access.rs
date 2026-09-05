//! 现存目录解析、直接子项列举和完整／快照文件读取。

use super::SystemFileSystem;
use super::error::{SystemFileSystemError, io_error};
use super::path::validate_windows_name;
use crate::diagnostic::FileSystemPathViolation;
use crate::runtime::windows::{
    FileIdentity, WindowsFsError, delete_empty_directory_if_identity, number_of_links,
    open_directory, pin_directory_without_reparse, pin_path_without_reparse,
    pin_regular_file_for_snapshot_read, validate_local_case_insensitive_ntfs_directory,
};
use crate::storage::file_system::{
    DirectChildDirectoryEnsurer, DirectoryEntry, DirectoryEntryKind, DirectoryLister,
    ExistingDirectoryResolver, FileReader, ListDirectoryError, ReadFile, ReadFileError,
    ResolveDirectoryError, SnapshotFileReader,
};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

impl ExistingDirectoryResolver for SystemFileSystem {
    type Error = SystemFileSystemError;

    fn resolve_existing_directory(
        &self,
        path: PathBuf,
    ) -> impl std::future::Future<Output = Result<PathBuf, ResolveDirectoryError<Self::Error>>> + Send
    {
        let requested = absolutize(path);
        let inner = Arc::clone(&self.inner);
        async move {
            let requested = requested.map_err(|source| ResolveDirectoryError::Io {
                path: PathBuf::from("."),
                source,
            })?;
            let error_path = requested.clone();
            inner
                .pool
                .execute("resolve_existing_directory", &error_path, move || {
                    resolve_directory_sync(requested)
                })
                .await
                .map_err(|source| ResolveDirectoryError::Io {
                    path: error_path,
                    source,
                })?
        }
    }
}

impl DirectChildDirectoryEnsurer for SystemFileSystem {
    type Error = SystemFileSystemError;

    fn ensure_direct_child_directory(
        &self,
        parent: PathBuf,
        child: OsString,
    ) -> impl std::future::Future<Output = Result<PathBuf, Self::Error>> + Send {
        let parent = absolutize(parent);
        let inner = Arc::clone(&self.inner);
        async move {
            let parent = parent?;
            let error_path = parent.clone();
            inner
                .pool
                .execute("ensure_direct_child_directory", &error_path, move || {
                    ensure_direct_child_directory_sync(parent, child)
                })
                .await?
        }
    }
}

impl DirectoryLister for SystemFileSystem {
    type Error = SystemFileSystemError;

    fn list_directory(
        &self,
        path: PathBuf,
    ) -> impl std::future::Future<
        Output = Result<Vec<DirectoryEntry>, ListDirectoryError<Self::Error>>,
    > + Send {
        let requested = absolutize(path);
        let inner = Arc::clone(&self.inner);
        async move {
            let requested = requested.map_err(|source| ListDirectoryError::Io {
                path: PathBuf::from("."),
                source,
            })?;
            let error_path = requested.clone();
            inner
                .pool
                .execute("list_directory", &error_path, move || {
                    list_directory_sync(requested)
                })
                .await
                .map_err(|source| ListDirectoryError::Io {
                    path: error_path,
                    source,
                })?
        }
    }
}

impl FileReader for SystemFileSystem {
    type Error = SystemFileSystemError;

    fn read_file(
        &self,
        path: PathBuf,
    ) -> impl std::future::Future<Output = Result<ReadFile, ReadFileError<Self::Error>>> + Send
    {
        let requested = absolutize(path);
        let inner = Arc::clone(&self.inner);
        async move {
            let requested = requested.map_err(|source| ReadFileError::Io {
                path: PathBuf::from("."),
                source,
            })?;
            let error_path = requested.clone();
            inner
                .pool
                .execute("read_file", &error_path, move || read_file_sync(requested))
                .await
                .map_err(|source| ReadFileError::Io {
                    path: error_path,
                    source,
                })?
        }
    }
}

impl SnapshotFileReader for SystemFileSystem {
    fn read_snapshot_file(
        &self,
        path: PathBuf,
    ) -> impl std::future::Future<Output = Result<ReadFile, ReadFileError<Self::Error>>> + Send
    {
        let requested = absolutize(path);
        let inner = Arc::clone(&self.inner);
        async move {
            let requested = requested.map_err(|source| ReadFileError::Io {
                path: PathBuf::from("."),
                source,
            })?;
            let error_path = requested.clone();
            inner
                .pool
                .execute("read_snapshot_file", &error_path, move || {
                    read_snapshot_file_sync(requested)
                })
                .await
                .map_err(|source| ReadFileError::Io {
                    path: error_path,
                    source,
                })?
        }
    }
}

pub(super) fn absolutize(path: PathBuf) -> Result<PathBuf, SystemFileSystemError> {
    if path.is_absolute() {
        Ok(path)
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .map_err(|source| io_error("读取当前工作目录", Path::new("."), source))
    }
}

fn resolve_directory_sync(
    path: PathBuf,
) -> Result<PathBuf, ResolveDirectoryError<SystemFileSystemError>> {
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(ResolveDirectoryError::NotFound { path });
        }
        Err(source) => {
            return Err(ResolveDirectoryError::Io {
                path: path.clone(),
                source: io_error("读取目录元数据", &path, source),
            });
        }
    };
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(ResolveDirectoryError::Io {
            path: path.clone(),
            source: WindowsFsError::ReparsePoint { path }.into(),
        });
    }
    if !metadata.is_dir() {
        return Err(ResolveDirectoryError::NotDirectory { path });
    }
    let pinned = pin_path_without_reparse(&path).map_err(|source| ResolveDirectoryError::Io {
        path: path.clone(),
        source: source.into(),
    })?;
    if !pinned
        .metadata()
        .map_err(|source| ResolveDirectoryError::Io {
            path: path.clone(),
            source: source.into(),
        })?
        .is_dir()
    {
        return Err(ResolveDirectoryError::NotDirectory { path });
    }
    Ok(pinned.resolved_path().to_path_buf())
}

fn ensure_direct_child_directory_sync(
    parent: PathBuf,
    child: OsString,
) -> Result<PathBuf, SystemFileSystemError> {
    let parent = validate_local_case_insensitive_ntfs_directory(&parent)?;
    let parent_handle = open_directory(&parent, false)?;
    let parent_identity = FileIdentity::of(&parent_handle, &parent)?;
    let child_path = parent.join(&child);
    validate_windows_name(&child, &child_path)?;
    let created = match fs::create_dir(&child_path) {
        Ok(()) => true,
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => false,
        Err(source) => return Err(io_error("建立直接子目录", &child_path, source)),
    };
    let pinned_child = pin_directory_without_reparse(&child_path)?;
    let child_identity = FileIdentity::of(pinned_child.file(), &child_path)?;
    let resolved_child = pinned_child.resolved_path().to_path_buf();
    let parent_unchanged = (|| -> Result<bool, SystemFileSystemError> {
        let current_parent = open_directory(&parent, false)?;
        Ok(FileIdentity::of(&current_parent, &parent)? == parent_identity)
    })();
    let operation = match parent_unchanged {
        Ok(true) => return Ok(resolved_child),
        Ok(false) => SystemFileSystemError::InvalidPath {
            path: parent,
            violation: FileSystemPathViolation::IdentityChanged,
        },
        Err(source) => source,
    };
    drop(pinned_child);
    drop(parent_handle);
    if !created {
        return Err(operation);
    }
    match delete_empty_directory_if_identity(&child_path, child_identity) {
        Ok(()) => Err(operation),
        Err(rollback) => Err(SystemFileSystemError::DirectChildRollbackFailed {
            path: child_path,
            operation: Box::new(operation),
            rollback: Box::new(rollback.into()),
        }),
    }
}

fn list_directory_sync(
    path: PathBuf,
) -> Result<Vec<DirectoryEntry>, ListDirectoryError<SystemFileSystemError>> {
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(ListDirectoryError::NotFound { path });
        }
        Err(source) => {
            return Err(ListDirectoryError::Io {
                path: path.clone(),
                source: io_error("读取目录元数据", &path, source),
            });
        }
    };
    if !metadata.is_dir() {
        return Err(ListDirectoryError::NotDirectory { path });
    }
    let pinned_directory =
        pin_path_without_reparse(&path).map_err(|source| ListDirectoryError::Io {
            path: path.clone(),
            source: source.into(),
        })?;
    if !pinned_directory
        .metadata()
        .map_err(|source| ListDirectoryError::Io {
            path: path.clone(),
            source: source.into(),
        })?
        .is_dir()
    {
        return Err(ListDirectoryError::NotDirectory { path });
    }
    let resolved_directory = pinned_directory.resolved_path().to_path_buf();
    let entries = fs::read_dir(&resolved_directory).map_err(|source| ListDirectoryError::Io {
        path: path.clone(),
        source: io_error("列举目录", &resolved_directory, source),
    })?;
    let mut result = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| ListDirectoryError::Io {
            path: path.clone(),
            source: io_error("读取目录项", &path, source),
        })?;
        let child = entry.path();
        let pinned_child =
            pin_path_without_reparse(&child).map_err(|source| ListDirectoryError::Io {
                path: child.clone(),
                source: source.into(),
            })?;
        let child_metadata = pinned_child
            .metadata()
            .map_err(|source| ListDirectoryError::Io {
                path: child.clone(),
                source: source.into(),
            })?;
        let kind = if child_metadata.is_dir() {
            DirectoryEntryKind::Directory
        } else if child_metadata.is_file() {
            if number_of_links(pinned_child.file(), &child).map_err(|source| {
                ListDirectoryError::Io {
                    path: child.clone(),
                    source: source.into(),
                }
            })? != 1
            {
                return Err(ListDirectoryError::Io {
                    path: child.clone(),
                    source: SystemFileSystemError::InvalidPath {
                        path: child,
                        violation: FileSystemPathViolation::HardLink,
                    },
                });
            }
            DirectoryEntryKind::RegularFile
        } else {
            return Err(ListDirectoryError::Io {
                path: child.clone(),
                source: SystemFileSystemError::InvalidPath {
                    path: child,
                    violation: FileSystemPathViolation::UnexpectedObject,
                },
            });
        };
        result.push(DirectoryEntry::new(
            pinned_child.resolved_path().to_path_buf(),
            kind,
        ));
    }
    Ok(result)
}

fn read_file_sync(path: PathBuf) -> Result<ReadFile, ReadFileError<SystemFileSystemError>> {
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(ReadFileError::NotFound { path });
        }
        Err(source) => {
            return Err(ReadFileError::Io {
                path: path.clone(),
                source: io_error("读取文件元数据", &path, source),
            });
        }
    };
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(ReadFileError::Io {
            path: path.clone(),
            source: WindowsFsError::ReparsePoint { path }.into(),
        });
    }
    if !metadata.is_file() {
        return Err(ReadFileError::NotFile { path });
    }
    let pinned = pin_path_without_reparse(&path).map_err(|source| ReadFileError::Io {
        path: path.clone(),
        source: source.into(),
    })?;
    let metadata = pinned.metadata().map_err(|source| ReadFileError::Io {
        path: path.clone(),
        source: source.into(),
    })?;
    if !metadata.is_file() {
        return Err(ReadFileError::NotFile { path });
    }
    let mut file = pinned.file();
    let resolved_path = pinned.resolved_path().to_path_buf();
    let bytes = read_all_bytes(&mut file).map_err(|source| ReadFileError::Io {
        path: path.clone(),
        source: io_error("读取文件", &path, source),
    })?;
    Ok(ReadFile::new(resolved_path, bytes))
}

fn read_snapshot_file_sync(
    path: PathBuf,
) -> Result<ReadFile, ReadFileError<SystemFileSystemError>> {
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(ReadFileError::NotFound { path });
        }
        Err(source) => {
            return Err(ReadFileError::Io {
                path: path.clone(),
                source: io_error("读取快照文件元数据", &path, source),
            });
        }
    };
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(ReadFileError::Io {
            path: path.clone(),
            source: WindowsFsError::ReparsePoint { path }.into(),
        });
    }
    if !metadata.is_file() {
        return Err(ReadFileError::NotFile { path });
    }

    let mut pinned =
        pin_regular_file_for_snapshot_read(&path).map_err(|source| ReadFileError::Io {
            path: path.clone(),
            source: source.into(),
        })?;
    let before = pinned.metadata().map_err(|source| ReadFileError::Io {
        path: path.clone(),
        source: source.into(),
    })?;
    if !before.is_file() {
        return Err(ReadFileError::NotFile { path });
    }
    let before_links =
        number_of_links(pinned.file(), &path).map_err(|source| ReadFileError::Io {
            path: path.clone(),
            source: source.into(),
        })?;
    if before_links != 1 {
        return Err(snapshot_file_violation(
            path,
            FileSystemPathViolation::HardLink,
        ));
    }
    let before_identity =
        FileIdentity::of(pinned.file(), &path).map_err(|source| ReadFileError::Io {
            path: path.clone(),
            source: source.into(),
        })?;
    let resolved_path = pinned.resolved_path().to_path_buf();
    let bytes = read_all_bytes(pinned.file_mut()).map_err(|source| ReadFileError::Io {
        path: path.clone(),
        source: io_error("读取快照文件", &path, source),
    })?;
    let after = pinned.metadata().map_err(|source| ReadFileError::Io {
        path: path.clone(),
        source: source.into(),
    })?;
    let after_identity =
        FileIdentity::of(pinned.file(), &path).map_err(|source| ReadFileError::Io {
            path: path.clone(),
            source: source.into(),
        })?;
    let after_links =
        number_of_links(pinned.file(), &path).map_err(|source| ReadFileError::Io {
            path: path.clone(),
            source: source.into(),
        })?;
    if before.len() != bytes.len() as u64
        || after.len() != before.len()
        || after_identity != before_identity
        || after_links != 1
    {
        return Err(snapshot_file_violation(
            path,
            FileSystemPathViolation::SourceChanged,
        ));
    }
    Ok(ReadFile::new(resolved_path, bytes))
}

fn snapshot_file_violation(
    path: PathBuf,
    violation: FileSystemPathViolation,
) -> ReadFileError<SystemFileSystemError> {
    ReadFileError::Io {
        path: path.clone(),
        source: SystemFileSystemError::InvalidPath { path, violation },
    }
}

/// 只依据底层 `Read` 的实际产出来增长缓冲区；调用方不得把文件元数据长度作为容量门槛。
pub(super) fn read_all_bytes(reader: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests;
