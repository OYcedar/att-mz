//! 目录发布工作路径和按物理身份约束的现场清理。

use super::error::{SystemFileSystemError, io_error};
use crate::diagnostic::FileSystemPathViolation;
use crate::runtime::windows::{
    FileIdentity, WindowsFsError, delete_empty_directory_if_identity,
    delete_regular_file_if_identity, open_directory, pin_directory_without_reparse,
    pin_path_without_reparse,
};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::{fs, io};

pub(super) const PUBLICATION_DIRECTORY_NAME: &str = ".directory-publish";

pub(super) const PUBLICATION_STAGE_NAME: &str = "stage";

pub(super) const PUBLICATION_BACKUP_NAME: &str = "backup";

pub(super) const PUBLICATION_JOURNAL_NAME: &str = "journal";

pub(super) struct StageCleanupGuard {
    path: PathBuf,
    expected_identity: FileIdentity,
    pub(super) armed: bool,
}

impl StageCleanupGuard {
    pub(super) fn new(path: PathBuf, expected_identity: FileIdentity) -> Self {
        Self {
            path,
            expected_identity,
            armed: true,
        }
    }

    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }

    pub(super) fn cleanup(&mut self) -> Result<(), SystemFileSystemError> {
        if !self.armed {
            return Ok(());
        }
        self.armed = false;
        remove_directory_tree_if_identity(&self.path, self.expected_identity)
    }
}

impl Drop for StageCleanupGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = remove_directory_tree_if_identity(&self.path, self.expected_identity);
        }
    }
}

/// 使用显式堆栈删除一个已知身份的目录树。
///
/// 根目录在整个枚举期间由无删除共享句柄固定；叶子删除也使用 file ID
/// 复核后的 handle disposition。因此路径在检查与删除之间被替换时，会显式失败而不会
/// 跟随 reparse point 或删除新的根对象。
pub(super) fn remove_directory_tree_if_identity(
    path: &Path,
    expected_identity: FileIdentity,
) -> Result<(), SystemFileSystemError> {
    let mut pending = vec![(path.to_path_buf(), expected_identity, false)];
    while let Some((directory, identity, children_visited)) = pending.pop() {
        if children_visited {
            delete_empty_directory_if_identity(&directory, identity)?;
            continue;
        }
        let pinned = match pin_directory_without_reparse(&directory) {
            Ok(pinned) => pinned,
            Err(WindowsFsError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                continue;
            }
            Err(source) => return Err(source.into()),
        };
        if FileIdentity::of(pinned.file(), &directory)? != identity {
            return Err(SystemFileSystemError::InvalidStagedIdentity { path: directory });
        }
        pending.push((directory.clone(), identity, true));
        let entries = fs::read_dir(&directory)
            .map_err(|source| io_error("枚举待清理目录", &directory, source))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| io_error("读取待清理目录项", &directory, source))?;
        for entry in entries {
            let child_path = entry.path();
            let child = match pin_path_without_reparse(&child_path) {
                Ok(child) => child,
                Err(WindowsFsError::Io { source, .. })
                    if source.kind() == io::ErrorKind::NotFound =>
                {
                    continue;
                }
                Err(source) => return Err(source.into()),
            };
            let metadata = child.metadata()?;
            let child_identity = FileIdentity::of(child.file(), &child_path)?;
            drop(child);
            if metadata.is_dir() {
                pending.push((child_path, child_identity, false));
            } else if metadata.is_file() {
                delete_regular_file_if_identity(&child_path, child_identity)?;
            } else {
                return Err(SystemFileSystemError::InvalidPath {
                    path: child_path,
                    violation: FileSystemPathViolation::UnexpectedObject,
                });
            }
        }
    }
    Ok(())
}

pub(super) fn publication_workspace_root(parent_root: &Path, target_name: &OsStr) -> PathBuf {
    parent_root
        .join(PUBLICATION_DIRECTORY_NAME)
        .join(target_name)
}

pub(super) fn ensure_publication_workspace(
    workspace_root: &Path,
) -> Result<(), SystemFileSystemError> {
    let publication_root = workspace_root
        .parent()
        .expect("目录发布工作目录必有公共父目录");
    ensure_plain_directory(publication_root, "建立目录发布根目录")?;
    ensure_plain_directory(workspace_root, "建立目标目录发布工作目录")
}

fn ensure_plain_directory(
    path: &Path,
    operation: &'static str,
) -> Result<(), SystemFileSystemError> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
        Err(source) => return Err(io_error(operation, path, source)),
    }
    let pinned = pin_directory_without_reparse(path)?;
    if !pinned.metadata()?.is_dir() {
        return Err(SystemFileSystemError::InvalidPath {
            path: path.to_path_buf(),
            violation: FileSystemPathViolation::UnexpectedObject,
        });
    }
    Ok(())
}

pub(super) fn remove_file_if_exists(path: &Path) -> Result<(), SystemFileSystemError> {
    let pinned = match pin_path_without_reparse(path) {
        Ok(pinned) => pinned,
        Err(WindowsFsError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(source) => return Err(source.into()),
    };
    if !pinned.metadata()?.is_file() {
        return Err(SystemFileSystemError::InvalidPath {
            path: path.to_path_buf(),
            violation: FileSystemPathViolation::NotRegularFile,
        });
    }
    let identity = FileIdentity::of(pinned.file(), path)?;
    drop(pinned);
    delete_regular_file_if_identity(path, identity).map_err(Into::into)
}

pub(super) fn identity_at(path: &Path) -> Result<Option<FileIdentity>, SystemFileSystemError> {
    match open_directory(path, true) {
        Ok(file) => FileIdentity::of(&file, path).map(Some).map_err(Into::into),
        Err(WindowsFsError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            Ok(None)
        }
        Err(source) => Err(source.into()),
    }
}

#[cfg(test)]
mod tests;
