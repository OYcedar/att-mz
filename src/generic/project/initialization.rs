//! Generic 输入路径约束与首次数据库候选的身份固定、发布和清理。

use super::error::{GenericProjectError, sqlite_operation_error};
use super::{SQLITE_SIDECAR_SUFFIXES, ensure_generic_operation_not_cancelled};
use crate::diagnostic::FileSystemOperation;
use crate::execution::CooperativeCancellation;
use crate::language::LanguageId;
use crate::runtime::sqlite::AttSqliteCancellableConnection;
use crate::runtime::windows::{
    FileIdentity, WindowsFsError, delete_regular_file_if_identity, pin_path_without_reparse,
    rename_without_replace_if_identity,
};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::{fs, io};

pub(super) fn resolve_source_root(path: &Path) -> Result<PathBuf, GenericProjectError> {
    if !path.is_dir() {
        return Err(GenericProjectError::SourceNotDirectory {
            path: path.to_path_buf(),
        });
    }
    fs::canonicalize(path).map_err(|source| GenericProjectError::Io {
        operation: FileSystemOperation::ResolveDirectory,
        path: path.to_path_buf(),
        source,
    })
}

pub(super) fn validate_distinct_languages(
    source: &LanguageId,
    target: &LanguageId,
) -> Result<(), GenericProjectError> {
    if source == target {
        return Err(GenericProjectError::SameSourceAndTargetLanguage {
            language: source.as_str().to_owned(),
        });
    }
    Ok(())
}

pub(super) fn validate_source_write_back_separation(
    source_root: &Path,
    workspace_root: &Path,
) -> Result<(), GenericProjectError> {
    let write_back_root = resolve_planned_path(&workspace_root.join("write_back"))?;
    if source_root == write_back_root
        || source_root.starts_with(&write_back_root)
        || write_back_root.starts_with(source_root)
    {
        return Err(GenericProjectError::SourceWriteBackOverlap {
            source_root: source_root.to_path_buf(),
            write_back_root,
        });
    }
    Ok(())
}

pub(super) fn resolve_planned_path(path: &Path) -> Result<PathBuf, GenericProjectError> {
    let absolute = std::path::absolute(path).map_err(|source| GenericProjectError::Io {
        operation: FileSystemOperation::ResolveDirectory,
        path: path.to_path_buf(),
        source,
    })?;
    let mut cursor = absolute.as_path();
    let mut missing = Vec::<OsString>::new();
    loop {
        match fs::canonicalize(cursor) {
            Ok(mut resolved) => {
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                let component = cursor.file_name().ok_or_else(|| GenericProjectError::Io {
                    operation: FileSystemOperation::ResolveDirectory,
                    path: absolute.clone(),
                    source: io::Error::new(io::ErrorKind::NotFound, "找不到可规范化的现存祖先目录"),
                })?;
                missing.push(component.to_os_string());
                cursor = cursor.parent().ok_or_else(|| GenericProjectError::Io {
                    operation: FileSystemOperation::ResolveDirectory,
                    path: absolute.clone(),
                    source: io::Error::new(io::ErrorKind::NotFound, "找不到可规范化的现存祖先目录"),
                })?;
            }
            Err(source) => {
                return Err(GenericProjectError::Io {
                    operation: FileSystemOperation::ResolveDirectory,
                    path: cursor.to_path_buf(),
                    source,
                });
            }
        }
    }
}

pub(super) fn publish_initial_database_candidate(
    connection: AttSqliteCancellableConnection,
    candidate_file: fs::File,
    identity: FileIdentity,
    candidate_path: &Path,
    database_path: &Path,
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericProjectError> {
    const CHECKPOINT_OPERATION: &str = "收束 Generic 初始数据库 WAL";
    const JOURNAL_MODE_OPERATION: &str = "切换 Generic 初始数据库日志模式";
    const CLOSE_OPERATION: &str = "关闭 Generic 初始数据库候选";

    ensure_generic_operation_not_cancelled(cancellation)?;
    let (busy, log_frames, checkpointed_frames) = connection
        .query_row("PRAGMA main.wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(|source| sqlite_operation_error(CHECKPOINT_OPERATION, source))?;
    if busy != 0 || log_frames != checkpointed_frames {
        if cancellation.is_requested() {
            return Err(GenericProjectError::Cancelled);
        }
        let code = if busy != 0 {
            rusqlite::ffi::SQLITE_BUSY
        } else {
            rusqlite::ffi::SQLITE_ERROR
        };
        return Err(GenericProjectError::Sqlite {
            operation: CHECKPOINT_OPERATION,
            source: rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                Some(format!(
                    "WAL checkpoint 未完成：busy={busy}, log_frames={log_frames}, \
                     checkpointed_frames={checkpointed_frames}"
                )),
            ),
        });
    }

    ensure_generic_operation_not_cancelled(cancellation)?;
    let journal_mode = connection
        .query_row("PRAGMA main.journal_mode = DELETE", [], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|source| sqlite_operation_error(JOURNAL_MODE_OPERATION, source))?;
    if !journal_mode.eq_ignore_ascii_case("delete") {
        return Err(GenericProjectError::Sqlite {
            operation: JOURNAL_MODE_OPERATION,
            source: rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
                Some(format!(
                    "期望 journal_mode=delete，SQLite 实际返回 {journal_mode:?}"
                )),
            ),
        });
    }

    ensure_generic_operation_not_cancelled(cancellation)?;
    connection
        .close()
        .map_err(|source| GenericProjectError::Sqlite {
            operation: CLOSE_OPERATION,
            source,
        })?;
    ensure_generic_operation_not_cancelled(cancellation)?;
    ensure_initial_database_path_has_no_sidecars(
        candidate_path,
        FileSystemOperation::Metadata,
        "候选数据库切换到 DELETE 后仍存在 SQLite sidecar",
        cancellation,
    )?;
    ensure_initial_database_path_has_no_sidecars(
        database_path,
        FileSystemOperation::Metadata,
        "首次 Init 的发布目标旁存在不属于当前项目的 SQLite sidecar",
        cancellation,
    )?;
    let verify_identity = (|| {
        let current = pin_path_without_reparse(candidate_path)?;
        if FileIdentity::of(&candidate_file, candidate_path)? == identity
            && FileIdentity::of(current.file(), candidate_path)? == identity
        {
            Ok(())
        } else {
            Err(WindowsFsError::FileIdentityChanged {
                path: candidate_path.to_path_buf(),
            })
        }
    })();
    verify_identity.map_err(|source| {
        GenericProjectError::InitialDatabaseOutcomeUnknown(Box::new(
            initial_database_file_system_error(FileSystemOperation::Metadata, source),
        ))
    })?;
    drop(candidate_file);
    rename_without_replace_if_identity(candidate_path, database_path, identity).map_err(|source| {
        let unknown = matches!(source, WindowsFsError::RenameTargetUnconfirmed { .. });
        let source = initial_database_file_system_error(FileSystemOperation::Rename, source);
        if unknown {
            GenericProjectError::InitialDatabaseOutcomeUnknown(Box::new(source))
        } else {
            source
        }
    })
}

pub(super) fn ensure_initial_database_path_has_no_sidecars(
    database_path: &Path,
    operation: FileSystemOperation,
    present_message: &'static str,
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericProjectError> {
    for suffix in SQLITE_SIDECAR_SUFFIXES {
        ensure_generic_operation_not_cancelled(cancellation)?;
        let sidecar = sqlite_sidecar_path(database_path, suffix);
        match fs::symlink_metadata(&sidecar) {
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(GenericProjectError::Io {
                    operation,
                    path: sidecar,
                    source,
                });
            }
            Ok(_) => {
                return Err(GenericProjectError::Io {
                    operation,
                    path: sidecar,
                    source: io::Error::new(io::ErrorKind::AlreadyExists, present_message),
                });
            }
        }
    }
    ensure_generic_operation_not_cancelled(cancellation)
}

pub(super) fn sqlite_sidecar_path(database_path: &Path, suffix: &str) -> PathBuf {
    let mut sidecar = database_path.as_os_str().to_os_string();
    sidecar.push(suffix);
    PathBuf::from(sidecar)
}

pub(super) fn initial_database_file_system_error(
    operation: FileSystemOperation,
    source: WindowsFsError,
) -> GenericProjectError {
    GenericProjectError::InitialDatabaseFileSystem { operation, source }
}

pub(super) fn observe_initial_database_sidecars(
    candidate_path: &Path,
    targets: &mut Vec<(PathBuf, FileIdentity)>,
) -> Result<(), GenericProjectError> {
    for suffix in SQLITE_SIDECAR_SUFFIXES {
        let path = sqlite_sidecar_path(candidate_path, suffix);
        let observed =
            pin_path_without_reparse(&path).and_then(|file| FileIdentity::of(file.file(), &path));
        match observed {
            Ok(identity) => targets.push((path, identity)),
            Err(WindowsFsError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(GenericProjectError::InitialDatabaseOutcomeUnknown(
                    Box::new(initial_database_file_system_error(
                        FileSystemOperation::Metadata,
                        source,
                    )),
                ));
            }
        }
    }
    Ok(())
}

pub(super) fn cleanup_initial_database_candidate(
    targets: &[(PathBuf, FileIdentity)],
) -> Result<(), Vec<GenericProjectError>> {
    let mut failures = Vec::new();
    for (path, identity) in targets {
        match delete_regular_file_if_identity(path, *identity) {
            Ok(()) => {}
            Err(WindowsFsError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => failures.push(initial_database_file_system_error(
                FileSystemOperation::Remove,
                source,
            )),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures)
    }
}
