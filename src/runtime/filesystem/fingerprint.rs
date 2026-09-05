//! 自然顺序目录树指纹与两轮来源一致性验证。

use super::SystemFileSystem;
use super::access::absolutize;
use super::error::{SystemFileSystemError, io_error};
use super::path::{
    validate_relative_windows_path, validate_windows_name, windows_ordinal_case_key,
};
use crate::diagnostic::FileSystemPathViolation;
use crate::fingerprint::Sha256Fingerprint;
use crate::runtime::windows::{
    FileIdentity, PinnedPath, WindowsFsError, number_of_links, pin_directory_without_reparse,
    pin_path_without_reparse, pin_regular_file_for_snapshot_read,
};
use crate::storage::file_system::{
    DirectoryTreeFingerprintError, DirectoryTreeFingerprintRequest, DirectoryTreeFingerprinter,
};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

const DIRECTORY_TREE_FINGERPRINT_DOMAIN: &[u8] = b"directory-tree-fingerprint";

impl DirectoryTreeFingerprinter for SystemFileSystem {
    type Error = Box<SystemFileSystemError>;

    fn fingerprint_directory_tree(
        &self,
        request: DirectoryTreeFingerprintRequest,
    ) -> impl std::future::Future<
        Output = Result<Sha256Fingerprint, DirectoryTreeFingerprintError<Self::Error>>,
    > + Send {
        let inner = Arc::clone(&self.inner);
        let error_path = request
            .roots()
            .first()
            .expect("受检目录树指纹请求至少包含一个根")
            .physical_root()
            .to_path_buf();
        async move {
            inner
                .pool
                .execute("fingerprint_directory_tree", &error_path, move || {
                    fingerprint_directory_tree_sync(request)
                })
                .await
                .map_err(|source| DirectoryTreeFingerprintError::Failed {
                    path: error_path,
                    source: Box::new(source),
                })?
        }
    }
}

#[derive(Clone)]
struct FingerprintRoot {
    physical_root: PathBuf,
    logical_root: PathBuf,
    logical_key: Vec<u16>,
}

#[derive(Eq, PartialEq)]
struct FingerprintIdentityObservation {
    entry_type: u8,
    logical_key: Vec<u16>,
    physical_path: PathBuf,
    identity: FileIdentity,
}

struct FingerprintObservation {
    fingerprint: Sha256Fingerprint,
    identities: Vec<FingerprintIdentityObservation>,
}

struct FingerprintPass {
    file_identities: HashSet<FileIdentity>,
    identities: Vec<FingerprintIdentityObservation>,
    hasher: Sha256,
}

#[derive(Clone, Copy)]
struct ExpectedFingerprintFile {
    identity: FileIdentity,
    size: u64,
}

fn fingerprint_directory_tree_sync(
    request: DirectoryTreeFingerprintRequest,
) -> Result<Sha256Fingerprint, DirectoryTreeFingerprintError<Box<SystemFileSystemError>>> {
    fingerprint_directory_tree_sync_with_between(request, || {})
}

fn fingerprint_directory_tree_sync_with_between<F>(
    request: DirectoryTreeFingerprintRequest,
    between_observations: F,
) -> Result<Sha256Fingerprint, DirectoryTreeFingerprintError<Box<SystemFileSystemError>>>
where
    F: FnOnce(),
{
    let mut roots = Vec::with_capacity(request.roots().len());
    for root in request.roots() {
        let physical_root = absolutize(root.physical_root().to_path_buf()).map_err(|source| {
            DirectoryTreeFingerprintError::Failed {
                path: root.physical_root().to_path_buf(),
                source: Box::new(source),
            }
        })?;
        validate_relative_windows_path(root.logical_root()).map_err(|source| {
            DirectoryTreeFingerprintError::Failed {
                path: physical_root.clone(),
                source: Box::new(source),
            }
        })?;
        roots.push(FingerprintRoot {
            physical_root,
            logical_root: root.logical_root().to_path_buf(),
            logical_key: path_utf16_key(root.logical_root()),
        });
    }
    roots.sort_by(|first, second| first.logical_key.cmp(&second.logical_key));
    let first_path = roots
        .first()
        .expect("受检目录树指纹请求至少包含一个根")
        .physical_root
        .clone();
    let first = fingerprint_directory_tree_once(&roots)?;
    between_observations();
    let second = fingerprint_directory_tree_once(&roots)?;
    if first.fingerprint != second.fingerprint || first.identities != second.identities {
        let path = first
            .identities
            .iter()
            .zip(&second.identities)
            .find_map(|(first, second)| (first != second).then(|| first.physical_path.clone()))
            .or_else(|| {
                first
                    .identities
                    .get(second.identities.len())
                    .map(|entry| entry.physical_path.clone())
            })
            .or_else(|| {
                second
                    .identities
                    .get(first.identities.len())
                    .map(|entry| entry.physical_path.clone())
            })
            .unwrap_or(first_path);
        return Err(DirectoryTreeFingerprintError::ChangedDuringObservation { path });
    }
    Ok(second.fingerprint)
}

fn fingerprint_directory_tree_once(
    roots: &[FingerprintRoot],
) -> Result<FingerprintObservation, DirectoryTreeFingerprintError<Box<SystemFileSystemError>>> {
    let mut pass = FingerprintPass {
        file_identities: HashSet::new(),
        identities: Vec::new(),
        hasher: Sha256::new(),
    };
    hash_frame(&mut pass.hasher, 0, DIRECTORY_TREE_FINGERPRINT_DOMAIN);
    for root in roots {
        fingerprint_directory(&root.physical_root, &root.logical_root, None, &mut pass)?;
    }
    Ok(FingerprintObservation {
        fingerprint: Sha256Fingerprint::from_bytes(pass.hasher.finalize().into()),
        identities: pass.identities,
    })
}

fn fingerprint_directory(
    physical_path: &Path,
    logical_path: &Path,
    expected_identity: Option<FileIdentity>,
    pass: &mut FingerprintPass,
) -> Result<(), DirectoryTreeFingerprintError<Box<SystemFileSystemError>>> {
    enum Work {
        Directory {
            physical_path: PathBuf,
            logical_path: PathBuf,
            expected_identity: Option<FileIdentity>,
        },
        File {
            physical_path: PathBuf,
            logical_path: PathBuf,
            expected: ExpectedFingerprintFile,
        },
        FinishDirectory {
            physical_path: PathBuf,
            identity: FileIdentity,
            held: PinnedPath,
        },
    }

    let mut pending = vec![Work::Directory {
        physical_path: physical_path.to_path_buf(),
        logical_path: logical_path.to_path_buf(),
        expected_identity,
    }];
    while let Some(work) = pending.pop() {
        match work {
            Work::File {
                physical_path,
                logical_path,
                expected,
            } => fingerprint_file(&physical_path, &logical_path, expected, pass)?,
            Work::FinishDirectory {
                physical_path,
                identity,
                held,
            } => {
                let after = pin_directory_without_reparse(&physical_path)
                    .map_err(|source| fingerprint_failed(&physical_path, source.into()))?;
                let after_identity = FileIdentity::of(after.file(), &physical_path)
                    .map_err(|source| fingerprint_failed(&physical_path, source.into()))?;
                let held_identity = FileIdentity::of(held.file(), &physical_path)
                    .map_err(|source| fingerprint_failed(&physical_path, source.into()))?;
                if after_identity != identity || held_identity != identity {
                    return Err(DirectoryTreeFingerprintError::ChangedDuringObservation {
                        path: physical_path,
                    });
                }
            }
            Work::Directory {
                physical_path,
                logical_path,
                expected_identity,
            } => {
                let metadata = match fs::symlink_metadata(&physical_path) {
                    Ok(metadata) => metadata,
                    Err(source) if source.kind() == io::ErrorKind::NotFound => {
                        return Err(DirectoryTreeFingerprintError::NotFound {
                            path: physical_path,
                        });
                    }
                    Err(source) => {
                        return Err(fingerprint_failed(
                            &physical_path,
                            io_error("读取目录树根元数据", &physical_path, source),
                        ));
                    }
                };
                if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    return Err(fingerprint_failed(
                        &physical_path,
                        WindowsFsError::ReparsePoint {
                            path: physical_path.clone(),
                        }
                        .into(),
                    ));
                }
                if !metadata.is_dir() {
                    return Err(DirectoryTreeFingerprintError::NotDirectory {
                        path: physical_path,
                    });
                }
                let held = pin_directory_without_reparse(&physical_path)
                    .map_err(|source| fingerprint_failed(&physical_path, source.into()))?;
                let identity = FileIdentity::of(held.file(), &physical_path)
                    .map_err(|source| fingerprint_failed(&physical_path, source.into()))?;
                if expected_identity.is_some_and(|expected| expected != identity) {
                    return Err(DirectoryTreeFingerprintError::ChangedDuringObservation {
                        path: physical_path,
                    });
                }
                hash_tree_path(&mut pass.hasher, 1, &logical_path);
                pass.identities.push(FingerprintIdentityObservation {
                    entry_type: 1,
                    logical_key: path_utf16_key(&logical_path),
                    physical_path: physical_path.clone(),
                    identity,
                });

                let resolved = held.resolved_path();
                let mut entries = Vec::new();
                let mut windows_names = HashSet::new();
                for entry in fs::read_dir(resolved).map_err(|source| {
                    fingerprint_failed(resolved, io_error("列举目录树", resolved, source))
                })? {
                    let entry = entry.map_err(|source| {
                        fingerprint_failed(resolved, io_error("读取目录树目录项", resolved, source))
                    })?;
                    let name = entry.file_name();
                    let path = entry.path();
                    validate_windows_name(&name, &path)
                        .map_err(|source| fingerprint_failed(&path, source))?;
                    let wide = name.encode_wide().collect::<Vec<_>>();
                    let windows_key = windows_ordinal_case_key(&name, &path)
                        .map_err(|source| fingerprint_failed(&path, source))?;
                    if !windows_names.insert(windows_key) {
                        return Err(fingerprint_failed(
                            &path,
                            SystemFileSystemError::InvalidPath {
                                path: path.clone(),
                                violation: FileSystemPathViolation::CaseCollision,
                            },
                        ));
                    }
                    let pinned_entry = pin_path_without_reparse(&path)
                        .map_err(|source| fingerprint_failed(&path, source.into()))?;
                    let metadata = pinned_entry
                        .metadata()
                        .map_err(|source| fingerprint_failed(&path, source.into()))?;
                    let identity = FileIdentity::of(pinned_entry.file(), &path)
                        .map_err(|source| fingerprint_failed(&path, source.into()))?;
                    let kind = if metadata.is_dir() {
                        FingerprintEntryKind::Directory
                    } else if metadata.is_file() {
                        FingerprintEntryKind::File {
                            size: metadata.len(),
                        }
                    } else {
                        return Err(fingerprint_failed(
                            &path,
                            SystemFileSystemError::InvalidPath {
                                path: path.clone(),
                                violation: FileSystemPathViolation::UnexpectedObject,
                            },
                        ));
                    };
                    entries.push(FingerprintEntry {
                        name,
                        wide_name: wide,
                        physical_path: path,
                        identity,
                        kind,
                    });
                }
                entries.sort_by(|first, second| first.wide_name.cmp(&second.wide_name));
                pending.push(Work::FinishDirectory {
                    physical_path,
                    identity,
                    held,
                });
                for entry in entries.into_iter().rev() {
                    let logical_child = logical_path.join(&entry.name);
                    pending.push(match entry.kind {
                        FingerprintEntryKind::Directory => Work::Directory {
                            physical_path: entry.physical_path,
                            logical_path: logical_child,
                            expected_identity: Some(entry.identity),
                        },
                        FingerprintEntryKind::File { size } => Work::File {
                            physical_path: entry.physical_path,
                            logical_path: logical_child,
                            expected: ExpectedFingerprintFile {
                                identity: entry.identity,
                                size,
                            },
                        },
                    });
                }
            }
        }
    }
    Ok(())
}

struct FingerprintEntry {
    name: OsString,
    wide_name: Vec<u16>,
    physical_path: PathBuf,
    identity: FileIdentity,
    kind: FingerprintEntryKind,
}

enum FingerprintEntryKind {
    Directory,
    File { size: u64 },
}

fn fingerprint_file(
    physical_path: &Path,
    logical_path: &Path,
    expected: ExpectedFingerprintFile,
    pass: &mut FingerprintPass,
) -> Result<(), DirectoryTreeFingerprintError<Box<SystemFileSystemError>>> {
    let mut pinned = pin_regular_file_for_snapshot_read(physical_path)
        .map_err(|source| fingerprint_failed(physical_path, source.into()))?;
    let before = pinned
        .metadata()
        .map_err(|source| fingerprint_failed(physical_path, source.into()))?;
    if !before.is_file() {
        return Err(fingerprint_failed(
            physical_path,
            SystemFileSystemError::InvalidPath {
                path: physical_path.to_path_buf(),
                violation: FileSystemPathViolation::NotRegularFile,
            },
        ));
    }
    if number_of_links(pinned.file(), physical_path)
        .map_err(|source| fingerprint_failed(physical_path, source.into()))?
        != 1
    {
        return Err(fingerprint_failed(
            physical_path,
            SystemFileSystemError::InvalidPath {
                path: physical_path.to_path_buf(),
                violation: FileSystemPathViolation::HardLink,
            },
        ));
    }
    let identity = FileIdentity::of(pinned.file(), physical_path)
        .map_err(|source| fingerprint_failed(physical_path, source.into()))?;
    if identity != expected.identity || before.len() != expected.size {
        return Err(DirectoryTreeFingerprintError::ChangedDuringObservation {
            path: physical_path.to_path_buf(),
        });
    }
    if !pass.file_identities.insert(identity) {
        return Err(fingerprint_failed(
            physical_path,
            SystemFileSystemError::InvalidPath {
                path: physical_path.to_path_buf(),
                violation: FileSystemPathViolation::HardLink,
            },
        ));
    }
    hash_tree_path(&mut pass.hasher, 2, logical_path);
    hash_frame(&mut pass.hasher, 4, &before.len().to_be_bytes());
    hash_frame_prefix(&mut pass.hasher, 5, before.len());
    let mut observed = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = pinned.file_mut().read(&mut buffer).map_err(|source| {
            fingerprint_failed(
                physical_path,
                io_error("读取目录树文件", physical_path, source),
            )
        })?;
        if read == 0 {
            break;
        }
        observed = observed.saturating_add(read as u64);
        pass.hasher.update(&buffer[..read]);
    }
    let after = pinned
        .metadata()
        .map_err(|source| fingerprint_failed(physical_path, source.into()))?;
    let after_identity = FileIdentity::of(pinned.file(), physical_path)
        .map_err(|source| fingerprint_failed(physical_path, source.into()))?;
    let after_links = number_of_links(pinned.file(), physical_path)
        .map_err(|source| fingerprint_failed(physical_path, source.into()))?;
    if observed != before.len()
        || after.len() != before.len()
        || after_identity != identity
        || after_links != 1
    {
        return Err(DirectoryTreeFingerprintError::ChangedDuringObservation {
            path: physical_path.to_path_buf(),
        });
    }
    pass.identities.push(FingerprintIdentityObservation {
        entry_type: 2,
        logical_key: path_utf16_key(logical_path),
        physical_path: physical_path.to_path_buf(),
        identity,
    });
    Ok(())
}

fn fingerprint_failed(
    path: &Path,
    source: SystemFileSystemError,
) -> DirectoryTreeFingerprintError<Box<SystemFileSystemError>> {
    DirectoryTreeFingerprintError::Failed {
        path: path.to_path_buf(),
        source: Box::new(source),
    }
}

fn path_utf16_key(path: &Path) -> Vec<u16> {
    let mut key = Vec::new();
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        key.push(0);
        key.extend(name.encode_wide());
    }
    key
}

fn hash_tree_path(hasher: &mut Sha256, entry_type: u8, path: &Path) {
    hash_frame(hasher, 1, &[entry_type]);
    let components = path.components().count() as u64;
    hash_frame(hasher, 2, &components.to_be_bytes());
    for component in path.components() {
        let Component::Normal(name) = component else {
            unreachable!("受检逻辑路径只能包含普通相对段")
        };
        let mut bytes = Vec::new();
        for unit in name.encode_wide() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        hash_frame(hasher, 3, &bytes);
    }
}

fn hash_frame(hasher: &mut Sha256, tag: u8, bytes: &[u8]) {
    hash_frame_prefix(hasher, tag, bytes.len() as u64);
    hasher.update(bytes);
}

fn hash_frame_prefix(hasher: &mut Sha256, tag: u8, length: u64) {
    hasher.update([tag]);
    hasher.update(length.to_be_bytes());
}

#[cfg(test)]
mod tests;
