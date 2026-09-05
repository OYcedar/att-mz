//! Generic 候选的磁盘物化、生产解析器复验与失败清理。
//!
//! 本模块拥有临时候选；成功交接后由目录发布器承担发布或丢弃责任。

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::diagnostic::FileSystemOperation;
use crate::execution::CooperativeCancellation;
use crate::storage::file_system::DirectoryPublishIntent;

use super::{
    GenericWriteBackCandidate, GenericWriteBackError,
    validate_materialized_write_back_file_with_cancellation,
};

pub(crate) const WRITE_BACK_SCRATCH_NAME: &str = ".write_back.tmp";

pub(crate) fn materialize_write_back_source(
    workspace_root: &Path,
    candidate: &GenericWriteBackCandidate,
    cancellation: &CooperativeCancellation,
) -> Result<PathBuf, GenericScratchError> {
    materialize_write_back_source_with(workspace_root, candidate, cancellation, |path, bytes| {
        write_file_with_cancellation(path, bytes, cancellation)
    })
}

fn materialize_write_back_source_with(
    workspace_root: &Path,
    candidate: &GenericWriteBackCandidate,
    cancellation: &CooperativeCancellation,
    mut write_file: impl FnMut(&Path, &[u8]) -> io::Result<()>,
) -> Result<PathBuf, GenericScratchError> {
    ensure_materialization_not_cancelled(cancellation)?;
    let scratch_root = workspace_root.join(WRITE_BACK_SCRATCH_NAME);
    fs::create_dir(&scratch_root).map_err(|source| GenericScratchError::Io {
        operation: FileSystemOperation::Create,
        path: scratch_root.clone(),
        source,
    })?;

    // 临时目录建立成功后，全部失败共用同一清理出口；清理失败仍保留原始错误。
    let result = (|| {
        for file in candidate.files() {
            ensure_materialization_not_cancelled(cancellation)?;
            let relative = file.relative_path();
            if relative.as_os_str().is_empty()
                || relative.is_absolute()
                || relative.components().any(|component| {
                    matches!(
                        component,
                        std::path::Component::ParentDir
                            | std::path::Component::CurDir
                            | std::path::Component::RootDir
                            | std::path::Component::Prefix(_)
                    )
                })
            {
                return Err(GenericScratchError::InvalidRelativePath(
                    relative.to_path_buf(),
                ));
            }
            let target = scratch_root.join(relative);
            let parent = target.parent().expect("相对 JSONL 文件必须拥有暂存父目录");
            fs::create_dir_all(parent).map_err(|source| GenericScratchError::Io {
                operation: FileSystemOperation::Create,
                path: parent.to_path_buf(),
                source,
            })?;
            ensure_materialization_not_cancelled(cancellation)?;
            write_file(&target, file.bytes()).map_err(|source| {
                if cancellation.is_requested() && source.kind() == io::ErrorKind::Interrupted {
                    GenericScratchError::Cancelled
                } else {
                    GenericScratchError::Io {
                        operation: FileSystemOperation::Write,
                        path: target.clone(),
                        source,
                    }
                }
            })?;
            ensure_materialization_not_cancelled(cancellation)?;
            let materialized_bytes =
                read_file_with_cancellation(&target, cancellation).map_err(|source| {
                    if cancellation.is_requested() && source.kind() == io::ErrorKind::Interrupted {
                        GenericScratchError::Cancelled
                    } else {
                        GenericScratchError::Io {
                            operation: FileSystemOperation::Read,
                            path: target.clone(),
                            source,
                        }
                    }
                })?;
            ensure_materialization_not_cancelled(cancellation)?;
            validate_materialized_write_back_file_with_cancellation(
                file,
                materialized_bytes,
                cancellation,
            )
            .map_err(|source| {
                if source.is_cancelled() {
                    GenericScratchError::Cancelled
                } else {
                    GenericScratchError::InvalidMaterializedFile {
                        path: target,
                        source: Box::new(source),
                    }
                }
            })?;
        }
        ensure_materialization_not_cancelled(cancellation)
    })();
    match result {
        Ok(()) => Ok(scratch_root),
        Err(operation) => match cleanup_write_back_source(workspace_root, &scratch_root) {
            Ok(()) => Err(operation),
            Err(cleanup) => Err(GenericScratchError::CleanupAfterFailure {
                operation: Box::new(operation),
                cleanup: Box::new(cleanup),
            }),
        },
    }
}

fn write_file_with_cancellation(
    path: &Path,
    bytes: &[u8],
    cancellation: &CooperativeCancellation,
) -> io::Result<()> {
    const CHUNK_BYTES: usize = 64 * 1024;

    let mut file = fs::File::create(path)?;
    for chunk in bytes.chunks(CHUNK_BYTES) {
        if cancellation.is_requested() {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        io::Write::write_all(&mut file, chunk)?;
    }
    if cancellation.is_requested() {
        Err(io::Error::from(io::ErrorKind::Interrupted))
    } else {
        Ok(())
    }
}

fn read_file_with_cancellation(
    path: &Path,
    cancellation: &CooperativeCancellation,
) -> io::Result<Vec<u8>> {
    const CHUNK_BYTES: usize = 64 * 1024;

    let mut file = fs::File::open(path)?;
    let capacity = file
        .metadata()
        .ok()
        .and_then(|metadata| usize::try_from(metadata.len()).ok())
        .unwrap_or_default();
    let mut output = Vec::with_capacity(capacity);
    let mut buffer = [0_u8; CHUNK_BYTES];
    loop {
        if cancellation.is_requested() {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        let read = loop {
            match io::Read::read(&mut file, &mut buffer) {
                Err(source)
                    if source.kind() == io::ErrorKind::Interrupted
                        && !cancellation.is_requested() =>
                {
                    continue;
                }
                result => break result?,
            }
        };
        if read == 0 {
            break;
        }
        output.extend_from_slice(&buffer[..read]);
    }
    if cancellation.is_requested() {
        Err(io::Error::from(io::ErrorKind::Interrupted))
    } else {
        Ok(output)
    }
}

fn ensure_materialization_not_cancelled(
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericScratchError> {
    if cancellation.is_requested() {
        Err(GenericScratchError::Cancelled)
    } else {
        Ok(())
    }
}

pub(crate) fn cleanup_write_back_source(
    workspace_root: &Path,
    scratch_root: &Path,
) -> Result<(), GenericScratchError> {
    let valid_parent = scratch_root.parent() == Some(workspace_root);
    let valid_name = scratch_root
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == WRITE_BACK_SCRATCH_NAME);
    if !valid_parent || !valid_name {
        return Err(GenericScratchError::UnsafeCleanupTarget {
            workspace_root: workspace_root.to_path_buf(),
            scratch_root: scratch_root.to_path_buf(),
        });
    }
    match fs::remove_dir_all(scratch_root) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(GenericScratchError::Io {
            operation: FileSystemOperation::Remove,
            path: scratch_root.to_path_buf(),
            source,
        }),
    }
}

pub(crate) fn publish_intent_for(
    target_root: &Path,
) -> Result<DirectoryPublishIntent, GenericScratchError> {
    match fs::symlink_metadata(target_root) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            Ok(DirectoryPublishIntent::ReplaceExisting)
        }
        Ok(_) => Err(GenericScratchError::TargetNotDirectory(
            target_root.to_path_buf(),
        )),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            Ok(DirectoryPublishIntent::CreateNew)
        }
        Err(source) => Err(GenericScratchError::Io {
            operation: FileSystemOperation::Metadata,
            path: target_root.to_path_buf(),
            source,
        }),
    }
}

#[derive(Debug)]
pub(crate) enum GenericScratchError {
    Cancelled,
    InvalidRelativePath(PathBuf),
    InvalidMaterializedFile {
        path: PathBuf,
        source: Box<GenericWriteBackError>,
    },
    TargetNotDirectory(PathBuf),
    UnsafeCleanupTarget {
        workspace_root: PathBuf,
        scratch_root: PathBuf,
    },
    Io {
        operation: FileSystemOperation,
        path: PathBuf,
        source: io::Error,
    },
    CleanupAfterFailure {
        operation: Box<GenericScratchError>,
        cleanup: Box<GenericScratchError>,
    },
}

impl fmt::Display for GenericScratchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("Generic 写回暂存已取消"),
            Self::InvalidRelativePath(path) => {
                write!(
                    formatter,
                    "候选 JSONL 路径不是普通相对路径：{}",
                    path.display()
                )
            }
            Self::InvalidMaterializedFile { path, source } => {
                write!(
                    formatter,
                    "暂存 JSONL 未通过落盘复查：{}（{source}）",
                    path.display()
                )
            }
            Self::TargetNotDirectory(path) => {
                write!(formatter, "目标存在但不是普通目录：{}", path.display())
            }
            Self::UnsafeCleanupTarget {
                workspace_root,
                scratch_root,
            } => write!(
                formatter,
                "拒绝清理无法证明属于项目工作区的暂存目录：工作区 {}，目标 {}",
                workspace_root.display(),
                scratch_root.display()
            ),
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "{} {} 失败：{source}",
                operation.as_str(),
                path.display()
            ),
            Self::CleanupAfterFailure { operation, cleanup } => {
                write!(formatter, "{operation}；随后清理也失败：{cleanup}")
            }
        }
    }
}

impl Error for GenericScratchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::InvalidMaterializedFile { source, .. } => Some(source.as_ref()),
            Self::CleanupAfterFailure { operation, .. } => Some(operation.as_ref()),
            Self::Cancelled
            | Self::InvalidRelativePath(_)
            | Self::TargetNotDirectory(_)
            | Self::UnsafeCleanupTarget { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::generic::{
        GenericInitRequest, GenericProjectStore, GenericUnitMap, build_write_back_candidate,
    };
    use crate::language::LanguageId;

    use super::*;

    #[test]
    fn materialization_rejects_changed_disk_bytes_and_removes_scratch() {
        let temporary = tempfile::tempdir().expect("应该可建立临时目录");
        let source_root = temporary.path().join("source");
        fs::create_dir_all(&source_root).expect("应该可建立输入目录");
        fs::write(
            source_root.join("scene.jsonl"),
            concat!(
                r#"{"id":"group","kind":"dialogue","units":["#,
                r#"{"id":"unit","text":"原文"}]}"#,
                "\n"
            ),
        )
        .expect("应该可写入 Generic 输入");
        let workspace_root = temporary.path().join("project");
        let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "materialize-test".parse().expect("项目名应该合法"),
            workspace_root: workspace_root.clone(),
            source_root: Some(source_root),
            source_language: Some(LanguageId::parse("ja").expect("源语言应该合法")),
            target_language: Some(LanguageId::parse("zh-Hans").expect("目标语言应该合法")),
        })
        .expect("Generic 项目应该可初始化");
        store.extract().expect("Generic 输入应该可提取");
        let (stored, live) = store.ensure_input_current().expect("输入应该仍为 Current");
        let candidate = build_write_back_candidate(&stored, &live, &GenericUnitMap::new())
            .expect("应该可建立写回候选");

        let result = materialize_write_back_source_with(
            &workspace_root,
            &candidate,
            &CooperativeCancellation::default(),
            |path, bytes| {
                fs::write(path, bytes)?;
                fs::write(
                    path,
                    concat!(
                        r#"{"id":"group","kind":"dialogue","units":["#,
                        r#"{"id":"unit","text":"落盘后被改写"}]}"#,
                        "\n"
                    ),
                )
            },
        );

        assert!(matches!(
            result,
            Err(GenericScratchError::InvalidMaterializedFile { source, .. })
                if matches!(
                    source.as_ref(),
                    GenericWriteBackError::MaterializedMismatch {
                        bytes_changed: true,
                        structure_changed: true,
                        ..
                    }
                )
        ));
        assert!(
            fs::read_dir(&workspace_root)
                .expect("应该可列举项目工作区")
                .filter_map(Result::ok)
                .all(|entry| entry.file_name().to_string_lossy() != WRITE_BACK_SCRATCH_NAME),
            "校验失败后不应残留 Generic 写回暂存目录"
        );

        let cancellation = CooperativeCancellation::default();
        let write_cancellation = cancellation.clone();
        let result = materialize_write_back_source_with(
            &workspace_root,
            &candidate,
            &cancellation,
            move |path, bytes| {
                fs::write(path, bytes)?;
                write_cancellation.request();
                Ok(())
            },
        );
        assert!(matches!(result, Err(GenericScratchError::Cancelled)));
        assert!(
            fs::read_dir(&workspace_root)
                .expect("应该可列举项目工作区")
                .filter_map(Result::ok)
                .all(|entry| entry.file_name().to_string_lossy() != WRITE_BACK_SCRATCH_NAME),
            "取消后不应残留 Generic 写回暂存目录"
        );
    }
}
