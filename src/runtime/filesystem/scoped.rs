//! 绑定候选身份的受限目录编辑能力。

use super::access::read_all_bytes;
use super::error::{SystemFileSystemError, io_error};
use super::path::{
    validate_relative_windows_path, validate_windows_name, windows_ordinal_case_key,
};
use super::publication::SystemStagingState;
use super::{SystemDirectoryPublisher, SystemFileSystemInner};
use crate::diagnostic::FileSystemPathViolation;
use crate::runtime::windows::{
    FileIdentity, PinnedPath, WindowsFsError, delete_regular_file_if_identity, number_of_links,
    open_read_write_file_without_reparse, pin_directory_without_reparse, pin_path_without_reparse,
};
use crate::storage::file_system::{
    BoundScopedDirectory, ScopedDirectoryBindError, ScopedDirectoryEditError,
    ScopedDirectoryEditor, ScopedDirectoryEntry, ScopedDirectoryEntryKind, ScopedDirectoryPath,
    ScopedDirectoryScope, StagedDirectory,
};
use crate::windows_path::WindowsOrdinalCaseKey;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

#[derive(Debug)]
pub(crate) struct SystemScopedDirectoryState {
    editor_identity: Arc<()>,
    root_identity: FileIdentity,
}

impl ScopedDirectoryEditor for SystemDirectoryPublisher {
    type CandidateState = SystemStagingState;
    type ScopeState = SystemScopedDirectoryState;
    type Error = Box<SystemFileSystemError>;

    fn bind_scoped_directory(
        &self,
        candidate: &StagedDirectory<Self::CandidateState>,
        scope: ScopedDirectoryScope,
    ) -> impl std::future::Future<
        Output = Result<
            BoundScopedDirectory<Self::ScopeState>,
            ScopedDirectoryBindError<Self::Error>,
        >,
    > + Send
    + use<> {
        let root = candidate.staging_root().to_path_buf();
        let candidate_state = candidate.state();
        let same_instance = Arc::ptr_eq(
            &candidate_state.publisher_identity,
            &self.publisher_identity,
        );
        let finalized = candidate_state.finalized;
        let expected_identity = candidate_state.stage_identity;
        let editor_identity = Arc::clone(&self.publisher_identity);
        let inner = Arc::clone(&self.inner);
        let verified_scope = scope.clone();
        async move {
            if !same_instance {
                return Err(ScopedDirectoryBindError::WrongEditorInstance);
            }
            if finalized {
                return Err(ScopedDirectoryBindError::CandidateFinalized { root });
            }
            let error_root = root.clone();
            let verified_root = root.clone();
            inner
                .pool
                .execute("bind_scoped_directory", &error_root, move || {
                    bind_scoped_directory_sync(&verified_root, expected_identity, &verified_scope)
                })
                .await
                .map_err(|source| ScopedDirectoryBindError::Failed {
                    root: error_root.clone(),
                    source: Box::new(source),
                })?
                .map_err(|source| match source {
                    SystemFileSystemError::InvalidStagedIdentity { .. } => {
                        ScopedDirectoryBindError::CandidateIdentityChanged {
                            root: error_root.clone(),
                        }
                    }
                    source => ScopedDirectoryBindError::Failed {
                        root: error_root.clone(),
                        source: Box::new(source),
                    },
                })?;
            Ok(BoundScopedDirectory::new(
                root,
                scope,
                SystemScopedDirectoryState {
                    editor_identity,
                    root_identity: expected_identity,
                },
            ))
        }
    }

    fn list_scoped_directory(
        &self,
        scope: &BoundScopedDirectory<Self::ScopeState>,
        path: ScopedDirectoryPath,
    ) -> impl std::future::Future<
        Output = Result<Vec<ScopedDirectoryEntry>, ScopedDirectoryEditError<Self::Error>>,
    > + Send {
        let context = scoped_operation_context(self, scope, path);
        async move {
            let (inner, root, root_identity, relative, absolute) = context?;
            let error_path = relative.as_path().to_path_buf();
            inner
                .pool
                .execute("list_scoped_directory", &error_path, move || {
                    list_scoped_directory_sync(&root, root_identity, &relative, &absolute)
                })
                .await
                .map_err(|source| ScopedDirectoryEditError::Failed {
                    path: error_path,
                    source: Box::new(source),
                })?
        }
    }

    fn list_scoped_root(
        &self,
        scope: &BoundScopedDirectory<Self::ScopeState>,
    ) -> impl std::future::Future<
        Output = Result<Vec<ScopedDirectoryEntry>, ScopedDirectoryEditError<Self::Error>>,
    > + Send {
        let context = scoped_root_context(self, scope);
        async move {
            let (inner, root, root_identity) = context?;
            let error_root = root.clone();
            inner
                .pool
                .execute("list_scoped_root", &error_root, move || {
                    list_scoped_root_sync(&root, root_identity)
                })
                .await
                .map_err(|source| ScopedDirectoryEditError::Failed {
                    path: PathBuf::from("."),
                    source: Box::new(source),
                })?
        }
    }

    fn create_scoped_directory(
        &self,
        scope: &BoundScopedDirectory<Self::ScopeState>,
        path: ScopedDirectoryPath,
    ) -> impl std::future::Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send
    {
        let scope_root = scope.scope().is_scope_root(&path);
        let context = scoped_operation_context(self, scope, path);
        async move {
            let (inner, root, root_identity, relative, absolute) = context?;
            if scope_root {
                return Err(ScopedDirectoryEditError::ScopeRootMutation {
                    path: relative.as_path().to_path_buf(),
                });
            }
            let error_path = relative.as_path().to_path_buf();
            inner
                .pool
                .execute("create_scoped_directory", &error_path, move || {
                    create_scoped_directory_sync(&root, root_identity, &relative, &absolute)
                })
                .await
                .map_err(|source| ScopedDirectoryEditError::Failed {
                    path: error_path,
                    source: Box::new(source),
                })?
        }
    }

    fn write_scoped_file(
        &self,
        scope: &BoundScopedDirectory<Self::ScopeState>,
        path: ScopedDirectoryPath,
        bytes: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send
    {
        let scope_root = scope.scope().is_scope_root(&path);
        let context = scoped_operation_context(self, scope, path);
        async move {
            let (inner, root, root_identity, relative, absolute) = context?;
            if scope_root {
                return Err(ScopedDirectoryEditError::ScopeRootMutation {
                    path: relative.as_path().to_path_buf(),
                });
            }
            let error_path = relative.as_path().to_path_buf();
            inner
                .pool
                .execute("write_scoped_file", &error_path, move || {
                    write_scoped_file_sync(&root, root_identity, &relative, &absolute, bytes)
                })
                .await
                .map_err(|source| ScopedDirectoryEditError::Failed {
                    path: error_path,
                    source: Box::new(source),
                })?
        }
    }
}

type ScopedOperationContext = (
    Arc<SystemFileSystemInner>,
    PathBuf,
    FileIdentity,
    ScopedDirectoryPath,
    PathBuf,
);

type ScopedRootContext = (Arc<SystemFileSystemInner>, PathBuf, FileIdentity);

fn scoped_root_context(
    publisher: &SystemDirectoryPublisher,
    scope: &BoundScopedDirectory<SystemScopedDirectoryState>,
) -> Result<ScopedRootContext, ScopedDirectoryEditError<Box<SystemFileSystemError>>> {
    if !Arc::ptr_eq(
        &scope.state().editor_identity,
        &publisher.publisher_identity,
    ) {
        return Err(ScopedDirectoryEditError::WrongEditorInstance);
    }
    Ok((
        Arc::clone(&publisher.inner),
        scope.root().to_path_buf(),
        scope.state().root_identity,
    ))
}

fn scoped_operation_context(
    publisher: &SystemDirectoryPublisher,
    scope: &BoundScopedDirectory<SystemScopedDirectoryState>,
    relative: ScopedDirectoryPath,
) -> Result<ScopedOperationContext, ScopedDirectoryEditError<Box<SystemFileSystemError>>> {
    if !scope.scope().contains(&relative) {
        return Err(ScopedDirectoryEditError::OutsideScope {
            path: relative.as_path().to_path_buf(),
        });
    }
    let (inner, root, root_identity) = scoped_root_context(publisher, scope)?;
    let absolute = root.join(relative.as_path());
    Ok((inner, root, root_identity, relative, absolute))
}

fn bind_scoped_directory_sync(
    root: &Path,
    expected_identity: FileIdentity,
    scope: &ScopedDirectoryScope,
) -> Result<(), SystemFileSystemError> {
    let pinned_root = pin_directory_without_reparse(root)?;
    if FileIdentity::of(pinned_root.file(), root)? != expected_identity {
        return Err(SystemFileSystemError::InvalidStagedIdentity {
            path: root.to_path_buf(),
        });
    }
    let mut declared_roots =
        HashMap::<WindowsOrdinalCaseKey, OsString>::with_capacity(scope.roots().len());
    for declared in scope.roots() {
        let path = pinned_root.resolved_path().join(declared);
        let key = windows_ordinal_case_key(declared, &path)?;
        if declared_roots.insert(key, declared.clone()).is_some() {
            return Err(SystemFileSystemError::InvalidPath {
                path: root.to_path_buf(),
                violation: FileSystemPathViolation::CaseCollision,
            });
        }
        let child = pin_directory_without_reparse(&path)?;
        if !child.metadata()?.is_dir() {
            return Err(SystemFileSystemError::InvalidPath {
                path,
                violation: FileSystemPathViolation::NotDirectory,
            });
        }
    }
    Ok(())
}

fn pin_scoped_root(
    root: &Path,
    expected_identity: FileIdentity,
) -> Result<PinnedPath, ScopedDirectoryEditError<Box<SystemFileSystemError>>> {
    let pinned =
        pin_directory_without_reparse(root).map_err(|source| ScopedDirectoryEditError::Failed {
            path: root.to_path_buf(),
            source: Box::new(source.into()),
        })?;
    let actual = FileIdentity::of(pinned.file(), root).map_err(|source| {
        ScopedDirectoryEditError::Failed {
            path: root.to_path_buf(),
            source: Box::new(source.into()),
        }
    })?;
    if actual != expected_identity {
        return Err(ScopedDirectoryEditError::CandidateIdentityChanged {
            root: root.to_path_buf(),
        });
    }
    Ok(pinned)
}

fn scoped_failed(
    path: &ScopedDirectoryPath,
    source: impl Into<SystemFileSystemError>,
) -> ScopedDirectoryEditError<Box<SystemFileSystemError>> {
    ScopedDirectoryEditError::Failed {
        path: path.as_path().to_path_buf(),
        source: Box::new(source.into()),
    }
}

fn list_scoped_directory_sync(
    root: &Path,
    root_identity: FileIdentity,
    relative: &ScopedDirectoryPath,
    absolute: &Path,
) -> Result<Vec<ScopedDirectoryEntry>, ScopedDirectoryEditError<Box<SystemFileSystemError>>> {
    let _root = pin_scoped_root(root, root_identity)?;
    validate_relative_windows_path(relative.as_path())
        .map_err(|source| scoped_failed(relative, source))?;
    let metadata = match fs::symlink_metadata(absolute) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(ScopedDirectoryEditError::NotFound {
                path: relative.as_path().to_path_buf(),
            });
        }
        Err(source) => {
            return Err(scoped_failed(
                relative,
                io_error("读取候选目录元数据", absolute, source),
            ));
        }
    };
    if !metadata.is_dir() {
        return Err(ScopedDirectoryEditError::NotDirectory {
            path: relative.as_path().to_path_buf(),
        });
    }
    let pinned = pin_directory_without_reparse(absolute)
        .map_err(|source| scoped_failed(relative, source))?;
    let resolved = pinned.resolved_path();
    let mut entries = Vec::new();
    let mut windows_names = HashSet::new();
    for entry in fs::read_dir(resolved)
        .map_err(|source| scoped_failed(relative, io_error("列举候选编辑目录", resolved, source)))?
    {
        let entry = entry.map_err(|source| {
            scoped_failed(relative, io_error("读取候选编辑目录项", resolved, source))
        })?;
        let name = entry.file_name();
        let child_path = entry.path();
        validate_windows_name(&name, &child_path)
            .map_err(|source| scoped_failed(relative, source))?;
        let windows_key = windows_ordinal_case_key(&name, &child_path)
            .map_err(|source| scoped_failed(relative, source))?;
        if !windows_names.insert(windows_key) {
            return Err(scoped_failed(
                relative,
                SystemFileSystemError::InvalidPath {
                    path: child_path,
                    violation: FileSystemPathViolation::CaseCollision,
                },
            ));
        }
        let child = pin_path_without_reparse(&child_path)
            .map_err(|source| scoped_failed(relative, source))?;
        let metadata = child
            .metadata()
            .map_err(|source| scoped_failed(relative, source))?;
        let kind = if metadata.is_dir() {
            ScopedDirectoryEntryKind::Directory
        } else if metadata.is_file() {
            if number_of_links(child.file(), &child_path)
                .map_err(|source| scoped_failed(relative, source))?
                != 1
            {
                return Err(scoped_failed(
                    relative,
                    SystemFileSystemError::InvalidPath {
                        path: child_path,
                        violation: FileSystemPathViolation::HardLink,
                    },
                ));
            }
            ScopedDirectoryEntryKind::File
        } else {
            return Err(scoped_failed(
                relative,
                SystemFileSystemError::InvalidPath {
                    path: child_path,
                    violation: FileSystemPathViolation::UnexpectedObject,
                },
            ));
        };
        entries.push(ScopedDirectoryEntry::new(name, kind));
    }
    entries.sort_by(|left, right| left.name().cmp(right.name()));
    Ok(entries)
}

fn list_scoped_root_sync(
    root: &Path,
    root_identity: FileIdentity,
) -> Result<Vec<ScopedDirectoryEntry>, ScopedDirectoryEditError<Box<SystemFileSystemError>>> {
    let pinned = pin_scoped_root(root, root_identity)?;
    let resolved = pinned.resolved_path();
    let mut entries = Vec::new();
    let mut windows_names = HashSet::new();
    for entry in fs::read_dir(resolved).map_err(|source| ScopedDirectoryEditError::Failed {
        path: PathBuf::from("."),
        source: Box::new(io_error("列举候选根", resolved, source)),
    })? {
        let entry = entry.map_err(|source| ScopedDirectoryEditError::Failed {
            path: PathBuf::from("."),
            source: Box::new(io_error("读取候选根目录项", resolved, source)),
        })?;
        let name = entry.file_name();
        let child_path = entry.path();
        validate_windows_name(&name, &child_path).map_err(|source| {
            ScopedDirectoryEditError::Failed {
                path: PathBuf::from("."),
                source: Box::new(source),
            }
        })?;
        let windows_key = windows_ordinal_case_key(&name, &child_path).map_err(|source| {
            ScopedDirectoryEditError::Failed {
                path: PathBuf::from("."),
                source: Box::new(source),
            }
        })?;
        if !windows_names.insert(windows_key) {
            return Err(ScopedDirectoryEditError::Failed {
                path: PathBuf::from("."),
                source: Box::new(SystemFileSystemError::InvalidPath {
                    path: child_path,
                    violation: FileSystemPathViolation::CaseCollision,
                }),
            });
        }
        let child = pin_path_without_reparse(&child_path).map_err(|source| {
            ScopedDirectoryEditError::Failed {
                path: PathBuf::from("."),
                source: Box::new(source.into()),
            }
        })?;
        let metadata = child
            .metadata()
            .map_err(|source| ScopedDirectoryEditError::Failed {
                path: PathBuf::from("."),
                source: Box::new(source.into()),
            })?;
        let kind = if metadata.is_dir() {
            ScopedDirectoryEntryKind::Directory
        } else if metadata.is_file() {
            if number_of_links(child.file(), &child_path).map_err(|source| {
                ScopedDirectoryEditError::Failed {
                    path: PathBuf::from("."),
                    source: Box::new(source.into()),
                }
            })? != 1
            {
                return Err(ScopedDirectoryEditError::Failed {
                    path: PathBuf::from("."),
                    source: Box::new(SystemFileSystemError::InvalidPath {
                        path: child_path,
                        violation: FileSystemPathViolation::HardLink,
                    }),
                });
            }
            ScopedDirectoryEntryKind::File
        } else {
            return Err(ScopedDirectoryEditError::Failed {
                path: PathBuf::from("."),
                source: Box::new(SystemFileSystemError::InvalidPath {
                    path: child_path,
                    violation: FileSystemPathViolation::UnexpectedObject,
                }),
            });
        };
        entries.push(ScopedDirectoryEntry::new(name, kind));
    }
    entries.sort_by(|left, right| left.name().cmp(right.name()));
    Ok(entries)
}

fn create_scoped_directory_sync(
    root: &Path,
    root_identity: FileIdentity,
    relative: &ScopedDirectoryPath,
    absolute: &Path,
) -> Result<(), ScopedDirectoryEditError<Box<SystemFileSystemError>>> {
    let _root_pin = pin_scoped_root(root, root_identity)?;
    validate_relative_windows_path(relative.as_path())
        .map_err(|source| scoped_failed(relative, source))?;

    let mut current = root.to_path_buf();
    let mut pins = Vec::new();
    for component in relative.as_path().components() {
        let Component::Normal(name) = component else {
            unreachable!("ScopedDirectoryPath 已建立普通相对段不变量")
        };
        current.push(name);
        match pin_directory_without_reparse(&current) {
            Ok(pin) => pins.push(pin),
            Err(WindowsFsError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|source| {
                    scoped_failed(relative, io_error("建立候选编辑目录", &current, source))
                })?;
                let pin = pin_directory_without_reparse(&current)
                    .map_err(|source| scoped_failed(relative, source))?;
                pins.push(pin);
            }
            Err(source) => return Err(scoped_failed(relative, source)),
        }
    }

    debug_assert_eq!(current, absolute);
    Ok(())
}

fn write_scoped_file_sync(
    root: &Path,
    root_identity: FileIdentity,
    relative: &ScopedDirectoryPath,
    absolute: &Path,
    bytes: Vec<u8>,
) -> Result<(), ScopedDirectoryEditError<Box<SystemFileSystemError>>> {
    let _root = pin_scoped_root(root, root_identity)?;
    validate_relative_windows_path(relative.as_path())
        .map_err(|source| scoped_failed(relative, source))?;
    let parent = absolute.parent().expect("受检候选文件路径必须包含父目录");
    let _parent =
        pin_directory_without_reparse(parent).map_err(|source| scoped_failed(relative, source))?;

    match fs::symlink_metadata(absolute) {
        Ok(metadata) if metadata.is_file() => write_existing_scoped_file(relative, absolute, bytes),
        Ok(_) => Err(ScopedDirectoryEditError::NotFile {
            path: relative.as_path().to_path_buf(),
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            write_new_scoped_file(relative, absolute, bytes)
        }
        Err(source) => Err(scoped_failed(
            relative,
            io_error("读取候选写入目标元数据", absolute, source),
        )),
    }
}

fn write_existing_scoped_file(
    relative: &ScopedDirectoryPath,
    absolute: &Path,
    bytes: Vec<u8>,
) -> Result<(), ScopedDirectoryEditError<Box<SystemFileSystemError>>> {
    let mut pinned = open_read_write_file_without_reparse(absolute, false)
        .map_err(|source| scoped_failed(relative, source))?;
    if number_of_links(pinned.file(), absolute).map_err(|source| scoped_failed(relative, source))?
        != 1
    {
        return Err(scoped_failed(
            relative,
            SystemFileSystemError::InvalidPath {
                path: absolute.to_path_buf(),
                violation: FileSystemPathViolation::HardLink,
            },
        ));
    }
    pinned
        .file_mut()
        .seek(SeekFrom::Start(0))
        .map_err(|source| {
            scoped_failed(relative, io_error("定位候选文件回滚副本", absolute, source))
        })?;
    let original = read_all_bytes(pinned.file_mut()).map_err(|source| {
        scoped_failed(relative, io_error("读取候选文件回滚副本", absolute, source))
    })?;

    if let Err(operation) = replace_pinned_file_contents(&mut pinned, absolute, &bytes) {
        return restore_scoped_file_after_failure(relative, absolute, pinned, original, operation);
    }
    Ok(())
}

fn write_new_scoped_file(
    relative: &ScopedDirectoryPath,
    absolute: &Path,
    bytes: Vec<u8>,
) -> Result<(), ScopedDirectoryEditError<Box<SystemFileSystemError>>> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(absolute)
        .map_err(|source| {
            scoped_failed(relative, io_error("建立候选编辑文件", absolute, source))
        })?;
    let identity =
        FileIdentity::of(&file, absolute).map_err(|source| scoped_failed(relative, source))?;
    let operation = file
        .write_all(&bytes)
        .and_then(|()| file.sync_data())
        .map_err(|source| io_error("写入候选编辑文件", absolute, source));
    drop(file);
    if let Err(operation) = operation {
        return match delete_regular_file_if_identity(absolute, identity) {
            Ok(()) => Err(scoped_failed(relative, operation)),
            Err(rollback) => Err(scoped_failed(
                relative,
                SystemFileSystemError::ScopedEditRollbackFailed {
                    path: absolute.to_path_buf(),
                    operation: Box::new(operation),
                    rollback: Box::new(rollback.into()),
                },
            )),
        };
    }
    Ok(())
}

fn replace_pinned_file_contents(
    pinned: &mut PinnedPath,
    absolute: &Path,
    bytes: &[u8],
) -> Result<(), SystemFileSystemError> {
    pinned
        .file_mut()
        .set_len(0)
        .and_then(|()| pinned.file_mut().seek(SeekFrom::Start(0)).map(|_| ()))
        .and_then(|()| pinned.file_mut().write_all(bytes))
        .and_then(|()| pinned.file_mut().sync_data())
        .map_err(|source| io_error("替换候选编辑文件内容", absolute, source))
}

fn restore_scoped_file_after_failure(
    relative: &ScopedDirectoryPath,
    absolute: &Path,
    mut pinned: PinnedPath,
    original: Vec<u8>,
    operation: SystemFileSystemError,
) -> Result<(), ScopedDirectoryEditError<Box<SystemFileSystemError>>> {
    match replace_pinned_file_contents(&mut pinned, absolute, &original) {
        Ok(()) => Err(scoped_failed(relative, operation)),
        Err(rollback) => Err(scoped_failed(
            relative,
            SystemFileSystemError::ScopedEditRollbackFailed {
                path: absolute.to_path_buf(),
                operation: Box::new(operation),
                rollback: Box::new(rollback),
            },
        )),
    }
}

#[cfg(test)]
mod tests;
