//! 候选树 manifest、来源身份校验及确定顺序的物化。

use super::error::{SystemFileSystemError, io_error};
use super::path::{
    validate_relative_windows_path, validate_windows_name, windows_ordinal_case_key,
};
#[cfg(test)]
use super::test_faults::cancel_test_candidate_copy_after_chunk;
use super::work_pool::ensure_operation_active;
use super::workspace::identity_at;
use crate::diagnostic::FileSystemPathViolation;
use crate::runtime::performance::RunPerformanceCounters;
use crate::runtime::windows::{
    FileIdentity, PinnedPath, number_of_links, pin_directory_without_reparse,
    pin_path_without_reparse, pin_regular_file_for_snapshot_read,
};
use crate::storage::file_system::{DirectoryFileOverlay, DirectorySourceMapping};
use crate::windows_path::WindowsOrdinalCaseKey;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

pub(super) fn build_candidate(
    stage_root: &Path,
    target_root: &Path,
    source_mappings: &[DirectorySourceMapping],
    overlays: &[DirectoryFileOverlay],
    empty_directories: &[PathBuf],
    cancellation: &AtomicBool,
    materialization_width: usize,
) -> Result<(), SystemFileSystemError> {
    ensure_operation_active(cancellation, "建立目录候选", stage_root)?;
    validate_declared_windows_paths(source_mappings, overlays, empty_directories)?;
    // 先冻结确定性来源 manifest，再创建候选内容。来源树内覆盖在物化时
    // 复核原文件的身份、类型和大小；独立覆盖按请求字节建立，再由调用方候选门禁复核内容一致性。
    let manifest = build_candidate_manifest(
        stage_root,
        target_root,
        source_mappings,
        overlays,
        empty_directories,
        cancellation,
    )?;
    materialize_candidate_manifest(
        stage_root,
        &manifest,
        overlays,
        cancellation,
        materialization_width,
    )
}

#[derive(Debug)]
struct CandidateManifest {
    operations: Vec<CandidateManifestOperation>,
}

#[derive(Debug)]
enum CandidateManifestOperation {
    EnsureDirectory(PathBuf),
    CopySource {
        relative_target: PathBuf,
        source_tree: CandidateManifestSourceTree,
    },
    WriteOverlay {
        relative_file: PathBuf,
        overlay_index: usize,
    },
}

#[derive(Debug)]
struct CandidateManifestSourceTree {
    root_directory: usize,
    directories: Vec<CandidateManifestDirectory>,
    files: Vec<CandidateManifestFile>,
}

#[derive(Debug)]
struct CandidateManifestDirectory {
    source: PathBuf,
    expected_identity: FileIdentity,
    entries: Vec<CandidateManifestEntry>,
}

#[derive(Debug)]
struct CandidateManifestEntry {
    name: OsString,
    kind: CandidateManifestEntryKind,
}

#[derive(Clone, Copy, Debug)]
enum CandidateManifestEntryKind {
    Directory(usize),
    File(usize),
}

#[derive(Debug)]
struct CandidateManifestFile {
    source: PathBuf,
    expected_identity: FileIdentity,
    observed_size: u64,
    overlay_index: Option<usize>,
}

struct CandidateSourceEntry {
    name: OsString,
    wide_name: Vec<u16>,
    physical_path: PathBuf,
}

struct CandidateOverlayLookup {
    windows_paths: HashMap<Vec<WindowsOrdinalCaseKey>, usize>,
}

struct CandidateManifestObservation {
    overlay_lookup: CandidateOverlayLookup,
    matched_overlay_sizes: Vec<Option<u64>>,
}

impl CandidateOverlayLookup {
    fn new(overlays: &[DirectoryFileOverlay]) -> Result<Self, SystemFileSystemError> {
        let mut windows_paths = HashMap::with_capacity(overlays.len());
        for (index, overlay) in overlays.iter().enumerate() {
            windows_paths.insert(windows_relative_path_key(overlay.relative_file())?, index);
        }
        Ok(Self { windows_paths })
    }

    fn find(&self, relative_file: &Path) -> Result<Option<usize>, SystemFileSystemError> {
        Ok(self
            .windows_paths
            .get(&windows_relative_path_key(relative_file)?)
            .copied())
    }
}

fn build_candidate_manifest(
    stage_root: &Path,
    target_root: &Path,
    source_mappings: &[DirectorySourceMapping],
    overlays: &[DirectoryFileOverlay],
    empty_directories: &[PathBuf],
    cancellation: &AtomicBool,
) -> Result<CandidateManifest, SystemFileSystemError> {
    let mut operations = Vec::new();
    let mut declared_directories = HashSet::new();
    let mut observation = CandidateManifestObservation {
        overlay_lookup: CandidateOverlayLookup::new(overlays)?,
        matched_overlay_sizes: vec![None; overlays.len()],
    };
    for mapping in source_mappings {
        ensure_operation_active(cancellation, "观察候选来源", mapping.source_directory())?;
        validate_relative_windows_path(mapping.relative_target())?;
        ensure_source_is_physically_disjoint(mapping.source_directory(), stage_root, target_root)?;
        if let Some(relative_parent) = mapping.relative_target().parent() {
            reserve_manifest_directories(
                relative_parent,
                &mut declared_directories,
                &mut operations,
            )?;
        }
        let source_tree = observe_candidate_source_tree(
            mapping.source_directory(),
            mapping.relative_target(),
            None,
            &mut observation,
            cancellation,
        )?;
        operations.push(CandidateManifestOperation::CopySource {
            relative_target: mapping.relative_target().to_path_buf(),
            source_tree,
        });
    }
    for (index, overlay) in overlays.iter().enumerate() {
        ensure_operation_active(cancellation, "观察候选覆盖", overlay.relative_file())?;
        validate_relative_windows_path(overlay.relative_file())?;
        if observation.matched_overlay_sizes[index].is_some() {
            continue;
        }
        let source_path = source_mappings.iter().find_map(|mapping| {
            overlay
                .relative_file()
                .strip_prefix(mapping.relative_target())
                .ok()
                .map(|relative| mapping.source_directory().join(relative))
        });
        if let Some(source_path) = source_path {
            match pin_regular_file_for_snapshot_read(&source_path) {
                Err(source) => return Err(source.into()),
                Ok(_) => {
                    return Err(SystemFileSystemError::InvalidPath {
                        path: source_path,
                        violation: FileSystemPathViolation::SourceChanged,
                    });
                }
            }
        }
        if let Some(parent) = overlay.relative_file().parent() {
            reserve_manifest_directories(parent, &mut declared_directories, &mut operations)?;
        }
        operations.push(CandidateManifestOperation::WriteOverlay {
            relative_file: overlay.relative_file().to_path_buf(),
            overlay_index: index,
        });
    }
    for directory in empty_directories {
        ensure_operation_active(cancellation, "观察候选空目录", directory)?;
        validate_relative_windows_path(directory)?;
        reserve_manifest_directories(directory, &mut declared_directories, &mut operations)?;
    }
    Ok(CandidateManifest { operations })
}

fn reserve_manifest_directories(
    relative: &Path,
    declared_directories: &mut HashSet<PathBuf>,
    operations: &mut Vec<CandidateManifestOperation>,
) -> Result<(), SystemFileSystemError> {
    let mut current = PathBuf::new();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            unreachable!("候选目录路径已经过结构校验")
        };
        current.push(name);
        if declared_directories.insert(current.clone()) {
            operations.push(CandidateManifestOperation::EnsureDirectory(current.clone()));
        }
    }
    Ok(())
}

struct CandidateDirectoryObservation {
    directory_index: usize,
    source: PathBuf,
    relative: PathBuf,
    source_path: PinnedPath,
    source_identity: FileIdentity,
    entries: std::vec::IntoIter<CandidateSourceEntry>,
}

fn begin_candidate_directory_observation(
    source: &Path,
    relative: &Path,
    expected_identity: Option<FileIdentity>,
    directories: &mut Vec<CandidateManifestDirectory>,
    cancellation: &AtomicBool,
) -> Result<CandidateDirectoryObservation, SystemFileSystemError> {
    ensure_operation_active(cancellation, "观察候选来源", source)?;
    let source_path = pin_directory_without_reparse(source)?;
    let source_resolved = source_path.resolved_path().to_path_buf();
    let source_identity = FileIdentity::of(source_path.file(), source)?;
    if expected_identity.is_some_and(|expected| expected != source_identity) {
        return Err(SystemFileSystemError::InvalidPath {
            path: source.to_path_buf(),
            violation: FileSystemPathViolation::IdentityChanged,
        });
    }
    let source_entries = read_candidate_source_entries(&source_resolved, cancellation)?;
    let directory_index = directories.len();
    directories.push(CandidateManifestDirectory {
        source: source.to_path_buf(),
        expected_identity: source_identity,
        entries: Vec::with_capacity(source_entries.len()),
    });
    Ok(CandidateDirectoryObservation {
        directory_index,
        source: source.to_path_buf(),
        relative: relative.to_path_buf(),
        source_path,
        source_identity,
        entries: source_entries.into_iter(),
    })
}

fn observe_candidate_source_tree(
    source: &Path,
    relative: &Path,
    expected_identity: Option<FileIdentity>,
    observation: &mut CandidateManifestObservation,
    cancellation: &AtomicBool,
) -> Result<CandidateManifestSourceTree, SystemFileSystemError> {
    let mut directories = Vec::new();
    let mut files = Vec::new();
    let root = begin_candidate_directory_observation(
        source,
        relative,
        expected_identity,
        &mut directories,
        cancellation,
    )?;
    let root_directory = root.directory_index;
    let mut pending = vec![root];
    while !pending.is_empty() {
        let current_source = &pending.last().expect("候选来源观察栈必须非空").source;
        ensure_operation_active(cancellation, "观察候选来源", current_source)?;
        let next_entry = pending
            .last_mut()
            .expect("候选来源观察栈必须非空")
            .entries
            .next();
        let Some(entry) = next_entry else {
            let frame = pending.pop().expect("候选来源观察栈必须非空");
            let after = pin_directory_without_reparse(&frame.source)?;
            let after_identity = FileIdentity::of(after.file(), &frame.source)?;
            let held_identity = FileIdentity::of(frame.source_path.file(), &frame.source)?;
            if after_identity != frame.source_identity || held_identity != frame.source_identity {
                return Err(SystemFileSystemError::InvalidPath {
                    path: frame.source,
                    violation: FileSystemPathViolation::IdentityChanged,
                });
            }
            continue;
        };

        let frame = pending.last().expect("候选来源观察栈必须非空");
        let parent_directory = frame.directory_index;
        let child_source = entry.physical_path;
        let child_relative = frame.relative.join(&entry.name);
        let pinned_child = pin_path_without_reparse(&child_source)?;
        let metadata = pinned_child.metadata()?;
        let child_identity = FileIdentity::of(pinned_child.file(), &child_source)?;
        let kind = if metadata.is_dir() {
            let child = begin_candidate_directory_observation(
                &child_source,
                &child_relative,
                Some(child_identity),
                &mut directories,
                cancellation,
            )?;
            let child_directory = child.directory_index;
            pending.push(child);
            CandidateManifestEntryKind::Directory(child_directory)
        } else if metadata.is_file() {
            let observed_size = metadata.len();
            if number_of_links(pinned_child.file(), &child_source)? != 1 {
                return Err(SystemFileSystemError::InvalidPath {
                    path: child_source,
                    violation: FileSystemPathViolation::HardLink,
                });
            }
            let overlay_index = observation.overlay_lookup.find(&child_relative)?;
            if let Some(index) = overlay_index
                && observation.matched_overlay_sizes[index]
                    .replace(observed_size)
                    .is_some()
            {
                return Err(SystemFileSystemError::InvalidPath {
                    path: child_relative,
                    violation: FileSystemPathViolation::OutsideScope,
                });
            }
            let file = files.len();
            files.push(CandidateManifestFile {
                source: child_source,
                expected_identity: child_identity,
                observed_size,
                overlay_index,
            });
            CandidateManifestEntryKind::File(file)
        } else {
            return Err(SystemFileSystemError::InvalidPath {
                path: child_source,
                violation: FileSystemPathViolation::UnexpectedObject,
            });
        };
        directories[parent_directory]
            .entries
            .push(CandidateManifestEntry {
                name: entry.name,
                kind,
            });
    }
    Ok(CandidateManifestSourceTree {
        root_directory,
        directories,
        files,
    })
}

fn read_candidate_source_entries(
    directory: &Path,
    cancellation: &AtomicBool,
) -> Result<Vec<CandidateSourceEntry>, SystemFileSystemError> {
    ensure_operation_active(cancellation, "列举候选来源", directory)?;
    let mut entries = Vec::new();
    for entry in
        fs::read_dir(directory).map_err(|source| io_error("列举复制来源", directory, source))?
    {
        ensure_operation_active(cancellation, "列举候选来源", directory)?;
        let entry = entry.map_err(|source| io_error("读取复制来源目录项", directory, source))?;
        let name = entry.file_name();
        entries.push(CandidateSourceEntry {
            wide_name: name.encode_wide().collect(),
            name,
            physical_path: entry.path(),
        });
    }
    entries.sort_by(|first, second| first.wide_name.cmp(&second.wide_name));
    let mut names = HashSet::with_capacity(entries.len());
    for entry in &entries {
        validate_windows_name(&entry.name, &entry.physical_path)?;
        if !names.insert(windows_ordinal_case_key(&entry.name, &entry.physical_path)?) {
            return Err(SystemFileSystemError::InvalidPath {
                path: entry.physical_path.clone(),
                violation: FileSystemPathViolation::CaseCollision,
            });
        }
    }
    Ok(entries)
}

fn materialize_candidate_manifest(
    stage_root: &Path,
    manifest: &CandidateManifest,
    overlays: &[DirectoryFileOverlay],
    cancellation: &AtomicBool,
    worker_width: usize,
) -> Result<(), SystemFileSystemError> {
    let mut files = Vec::new();
    for operation in &manifest.operations {
        ensure_operation_active(cancellation, "物化目录候选", stage_root)?;
        match operation {
            CandidateManifestOperation::EnsureDirectory(relative) => {
                let path = stage_root.join(relative);
                match fs::create_dir(&path) {
                    Ok(()) => {}
                    Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(source) => return Err(io_error("建立候选目录", &path, source)),
                }
                let _pinned = pin_directory_without_reparse(&path)?;
            }
            CandidateManifestOperation::CopySource {
                relative_target,
                source_tree,
            } => prepare_candidate_source_tree(
                source_tree,
                &stage_root.join(relative_target),
                relative_target.as_os_str().is_empty(),
                &mut files,
                cancellation,
            )?,
            CandidateManifestOperation::WriteOverlay {
                relative_file,
                overlay_index,
            } => files.push(CandidateFileTask {
                ordinal: files.len(),
                kind: CandidateFileTaskKind::StandaloneOverlay {
                    overlay_index: *overlay_index,
                },
                destination: stage_root.join(relative_file),
            }),
        }
    }
    materialize_candidate_files(&files, overlays, cancellation, worker_width)?;
    for operation in &manifest.operations {
        ensure_operation_active(cancellation, "验证候选来源", stage_root)?;
        if let CandidateManifestOperation::CopySource { source_tree, .. } = operation {
            validate_materialized_source_tree(source_tree, cancellation)?;
        }
    }
    Ok(())
}

struct CandidateFileTask<'a> {
    ordinal: usize,
    kind: CandidateFileTaskKind<'a>,
    destination: PathBuf,
}

#[derive(Clone, Copy)]
enum CandidateFileTaskKind<'a> {
    Source(&'a CandidateManifestFile),
    StandaloneOverlay { overlay_index: usize },
}

fn prepare_candidate_source_tree<'a>(
    source_tree: &'a CandidateManifestSourceTree,
    destination: &Path,
    destination_is_existing_candidate_root: bool,
    files: &mut Vec<CandidateFileTask<'a>>,
    cancellation: &AtomicBool,
) -> Result<(), SystemFileSystemError> {
    enum Work {
        Directory {
            directory: usize,
            destination: PathBuf,
            create_destination: bool,
        },
        File {
            file: usize,
            destination: PathBuf,
        },
    }

    let mut pending = vec![Work::Directory {
        directory: source_tree.root_directory,
        destination: destination.to_path_buf(),
        create_destination: !destination_is_existing_candidate_root,
    }];
    while let Some(work) = pending.pop() {
        ensure_operation_active(cancellation, "准备候选来源", destination)?;
        match work {
            Work::File { file, destination } => files.push(CandidateFileTask {
                ordinal: files.len(),
                kind: CandidateFileTaskKind::Source(&source_tree.files[file]),
                destination,
            }),
            Work::Directory {
                directory,
                destination,
                create_destination,
            } => {
                let manifest = &source_tree.directories[directory];
                let source_path = pin_directory_without_reparse(&manifest.source)?;
                let source_identity = FileIdentity::of(source_path.file(), &manifest.source)?;
                if source_identity != manifest.expected_identity {
                    return Err(SystemFileSystemError::InvalidPath {
                        path: manifest.source.clone(),
                        violation: FileSystemPathViolation::IdentityChanged,
                    });
                }
                validate_manifest_directory_entries(
                    manifest,
                    source_path.resolved_path(),
                    cancellation,
                )?;
                if create_destination {
                    fs::create_dir(&destination)
                        .map_err(|source| io_error("建立候选目录", &destination, source))?;
                }
                let destination_path = pin_directory_without_reparse(&destination)?;
                let destination_resolved = destination_path.resolved_path().to_path_buf();

                for entry in manifest.entries.iter().rev() {
                    let child_destination = destination_resolved.join(&entry.name);
                    pending.push(match entry.kind {
                        CandidateManifestEntryKind::Directory(directory) => Work::Directory {
                            directory,
                            destination: child_destination,
                            create_destination: true,
                        },
                        CandidateManifestEntryKind::File(file) => Work::File {
                            file,
                            destination: child_destination,
                        },
                    });
                }
            }
        }
    }
    Ok(())
}

fn materialize_candidate_files(
    files: &[CandidateFileTask<'_>],
    overlays: &[DirectoryFileOverlay],
    cancellation: &AtomicBool,
    worker_width: usize,
) -> Result<(), SystemFileSystemError> {
    if files.is_empty() {
        return ensure_operation_active(cancellation, "物化候选文件", Path::new("<candidate>"));
    }
    ensure_operation_active(cancellation, "物化候选文件", &files[0].destination)?;
    let next = AtomicUsize::new(0);
    let errors = Mutex::new(Vec::<(usize, SystemFileSystemError)>::new());
    let width = worker_width.max(1).min(files.len());
    thread::scope(|scope| {
        for _ in 1..width {
            scope.spawn(|| {
                materialize_candidate_file_worker(files, overlays, cancellation, &next, &errors)
            });
        }
        materialize_candidate_file_worker(files, overlays, cancellation, &next, &errors);
    });
    let mut errors = errors
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    errors.sort_by_key(|(ordinal, _)| *ordinal);
    match errors.into_iter().next() {
        Some((_, error)) => Err(error),
        None => ensure_operation_active(cancellation, "物化候选文件", &files[0].destination),
    }
}

fn materialize_candidate_file_worker(
    files: &[CandidateFileTask<'_>],
    overlays: &[DirectoryFileOverlay],
    cancellation: &AtomicBool,
    next: &AtomicUsize,
    errors: &Mutex<Vec<(usize, SystemFileSystemError)>>,
) {
    loop {
        let index = next.fetch_add(1, Ordering::Relaxed);
        let Some(task) = files.get(index) else {
            return;
        };
        let result = ensure_operation_active(cancellation, "物化候选文件", &task.destination)
            .and_then(|()| match task.kind {
                CandidateFileTaskKind::Source(manifest) => match manifest.overlay_index {
                    Some(overlay) => write_candidate_overlay(
                        manifest,
                        &task.destination,
                        &overlays[overlay],
                        cancellation,
                    ),
                    None => copy_manifest_regular_file(manifest, &task.destination, cancellation)
                        .map(|_| ()),
                },
                CandidateFileTaskKind::StandaloneOverlay { overlay_index } => {
                    write_candidate_overlay_bytes(
                        &task.destination,
                        &overlays[overlay_index],
                        cancellation,
                    )
                }
            });
        let cancelled = matches!(result, Err(SystemFileSystemError::Cancelled { .. }));
        if let Err(error) = result {
            errors
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push((task.ordinal, error));
        }
        if cancelled {
            return;
        };
    }
}

fn validate_materialized_source_tree(
    source_tree: &CandidateManifestSourceTree,
    cancellation: &AtomicBool,
) -> Result<(), SystemFileSystemError> {
    let mut pending = vec![source_tree.root_directory];
    while let Some(directory) = pending.pop() {
        let manifest = &source_tree.directories[directory];
        ensure_operation_active(cancellation, "验证候选来源", &manifest.source)?;
        let after = pin_directory_without_reparse(&manifest.source)?;
        let after_identity = FileIdentity::of(after.file(), &manifest.source)?;
        if after_identity != manifest.expected_identity {
            return Err(SystemFileSystemError::InvalidPath {
                path: manifest.source.clone(),
                violation: FileSystemPathViolation::IdentityChanged,
            });
        }
        validate_manifest_directory_entries(manifest, after.resolved_path(), cancellation)?;
        for entry in manifest.entries.iter().rev() {
            if let CandidateManifestEntryKind::Directory(directory) = entry.kind {
                pending.push(directory);
            }
        }
    }
    Ok(())
}

fn validate_manifest_directory_entries(
    manifest: &CandidateManifestDirectory,
    source_resolved: &Path,
    cancellation: &AtomicBool,
) -> Result<(), SystemFileSystemError> {
    let current = read_candidate_source_entries(source_resolved, cancellation)?;
    if current.len() != manifest.entries.len()
        || current
            .iter()
            .zip(&manifest.entries)
            .any(|(current, expected)| current.name != expected.name)
    {
        return Err(SystemFileSystemError::InvalidPath {
            path: manifest.source.clone(),
            violation: FileSystemPathViolation::SourceChanged,
        });
    }
    Ok(())
}

fn windows_relative_path_key(
    path: &Path,
) -> Result<Vec<WindowsOrdinalCaseKey>, SystemFileSystemError> {
    path.components()
        .map(|component| {
            let Component::Normal(name) = component else {
                unreachable!("候选相对路径已经过结构校验")
            };
            windows_ordinal_case_key(name, path)
        })
        .collect()
}

fn validate_declared_windows_paths(
    source_mappings: &[DirectorySourceMapping],
    overlays: &[DirectoryFileOverlay],
    empty_directories: &[PathBuf],
) -> Result<(), SystemFileSystemError> {
    #[derive(Default)]
    struct TrieNode {
        spelling: Option<OsString>,
        children: HashMap<WindowsOrdinalCaseKey, usize>,
    }

    let declared_paths = source_mappings
        .iter()
        .map(DirectorySourceMapping::relative_target)
        .chain(overlays.iter().map(DirectoryFileOverlay::relative_file))
        .chain(empty_directories.iter().map(PathBuf::as_path));
    let mut trie = vec![TrieNode::default()];
    for path in declared_paths {
        validate_relative_windows_path(path)?;
        let mut node_index = 0;
        for component in path.components() {
            let Component::Normal(name) = component else {
                unreachable!("候选声明路径已经过结构校验");
            };
            let key = windows_ordinal_case_key(name, path)?;
            let child_index = match trie[node_index].children.get(&key).copied() {
                Some(index) => index,
                None => {
                    let index = trie.len();
                    trie.push(TrieNode {
                        spelling: Some(name.to_os_string()),
                        children: HashMap::new(),
                    });
                    trie[node_index].children.insert(key, index);
                    index
                }
            };
            if trie[child_index]
                .spelling
                .as_deref()
                .is_some_and(|spelling| spelling != name)
            {
                return Err(SystemFileSystemError::InvalidPath {
                    path: path.to_path_buf(),
                    violation: FileSystemPathViolation::CaseCollision,
                });
            }
            node_index = child_index;
        }
    }
    Ok(())
}

/// 发布前重新观测完整候选，把 `prepare` 返回后由受信非根服务新增的文件
/// 调用方在候选内新增的普通文件也纳入同一组资源、名称和 reparse 不变量。
pub(super) fn validate_complete_candidate(
    stage_root: &Path,
    expected_identity: FileIdentity,
    performance: &RunPerformanceCounters,
) -> Result<(), SystemFileSystemError> {
    performance.candidate_validation_started();
    let pinned_root = pin_directory_without_reparse(stage_root)?;
    if FileIdentity::of(pinned_root.file(), stage_root)? != expected_identity {
        return Err(SystemFileSystemError::InvalidStagedIdentity {
            path: stage_root.to_path_buf(),
        });
    }
    let mut file_identities = HashSet::new();
    let mut pending_directories = vec![stage_root.to_path_buf()];
    while let Some(directory) = pending_directories.pop() {
        let pinned_directory = pin_directory_without_reparse(&directory)?;
        let resolved = pinned_directory.resolved_path().to_path_buf();
        let mut names = HashSet::new();
        for entry in fs::read_dir(&resolved)
            .map_err(|source| io_error("发布前枚举完整候选", &resolved, source))?
        {
            let entry =
                entry.map_err(|source| io_error("发布前读取候选目录项", &resolved, source))?;
            let name = entry.file_name();
            let child_path = entry.path();
            validate_windows_name(&name, &child_path)?;
            if !names.insert(windows_ordinal_case_key(&name, &child_path)?) {
                return Err(SystemFileSystemError::InvalidPath {
                    path: child_path,
                    violation: FileSystemPathViolation::CaseCollision,
                });
            }
            let child = pin_path_without_reparse(&child_path)?;
            let metadata = child.metadata()?;
            if metadata.is_dir() {
                pending_directories.push(child_path);
            } else if metadata.is_file() {
                if number_of_links(child.file(), &child_path)? != 1 {
                    return Err(SystemFileSystemError::InvalidPath {
                        path: child_path,
                        violation: FileSystemPathViolation::HardLink,
                    });
                }
                let identity = FileIdentity::of(child.file(), &child_path)?;
                if !file_identities.insert(identity) {
                    return Err(SystemFileSystemError::InvalidPath {
                        path: child_path,
                        violation: FileSystemPathViolation::HardLink,
                    });
                }
            } else {
                return Err(SystemFileSystemError::InvalidPath {
                    path: child_path,
                    violation: FileSystemPathViolation::UnexpectedObject,
                });
            }
        }
    }
    performance.candidate_validation_completed();
    Ok(())
}

fn ensure_source_is_physically_disjoint(
    source: &Path,
    stage_root: &Path,
    target_root: &Path,
) -> Result<(), SystemFileSystemError> {
    let source_path = pin_directory_without_reparse(source)?;
    let source_root = source_path.resolved_path().to_path_buf();
    let source_identity = FileIdentity::of(source_path.file(), &source_root)?;
    let stage_ancestors = directory_ancestor_identities(stage_root)?;
    if stage_ancestors.contains(&source_identity) {
        return Err(SystemFileSystemError::InvalidPath {
            path: source_root,
            violation: FileSystemPathViolation::OutsideScope,
        });
    }
    let source_ancestors = directory_ancestor_identities(&source_root)?;
    let stage_identity =
        identity_at(stage_root)?.expect("候选目录在复制开始前已经建立并持有文件身份");
    if source_ancestors.contains(&stage_identity) {
        return Err(SystemFileSystemError::InvalidPath {
            path: source_root,
            violation: FileSystemPathViolation::OutsideScope,
        });
    }
    let target_identity = match fs::symlink_metadata(target_root) {
        Ok(metadata)
            if metadata.is_dir()
                && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 =>
        {
            identity_at(target_root)?
        }
        Ok(_) => None,
        Err(source) if source.kind() == io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(io_error("读取发布目标物理身份", target_root, source));
        }
    };
    if let Some(target_identity) = target_identity {
        if source_ancestors.contains(&target_identity) {
            return Err(SystemFileSystemError::InvalidPath {
                path: source_root,
                violation: FileSystemPathViolation::OutsideScope,
            });
        }
        let target_ancestors = directory_ancestor_identities(target_root)?;
        if target_ancestors.contains(&source_identity) {
            return Err(SystemFileSystemError::InvalidPath {
                path: source_root,
                violation: FileSystemPathViolation::OutsideScope,
            });
        }
    }
    Ok(())
}

fn directory_ancestor_identities(path: &Path) -> Result<Vec<FileIdentity>, SystemFileSystemError> {
    pin_directory_without_reparse(path)?
        .component_identities()
        .map_err(Into::into)
}

fn pin_manifest_regular_file(
    manifest: &CandidateManifestFile,
) -> Result<PinnedPath, SystemFileSystemError> {
    let input = pin_regular_file_for_snapshot_read(&manifest.source)?;
    let before = input.metadata()?;
    if !before.is_file() {
        return Err(SystemFileSystemError::InvalidPath {
            path: manifest.source.clone(),
            violation: FileSystemPathViolation::NotRegularFile,
        });
    }
    let identity = FileIdentity::of(input.file(), &manifest.source)?;
    if identity != manifest.expected_identity || before.len() != manifest.observed_size {
        return Err(SystemFileSystemError::InvalidPath {
            path: manifest.source.clone(),
            violation: FileSystemPathViolation::SourceChanged,
        });
    }
    if number_of_links(input.file(), &manifest.source)? != 1 {
        return Err(SystemFileSystemError::InvalidPath {
            path: manifest.source.clone(),
            violation: FileSystemPathViolation::HardLink,
        });
    }
    Ok(input)
}

fn validate_materialized_source_file(
    input: &PinnedPath,
    manifest: &CandidateManifestFile,
) -> Result<(), SystemFileSystemError> {
    let after = input.metadata()?;
    let after_identity = FileIdentity::of(input.file(), &manifest.source)?;
    let after_links = number_of_links(input.file(), &manifest.source)?;
    if after.len() != manifest.observed_size
        || after_identity != manifest.expected_identity
        || after_links != 1
    {
        return Err(SystemFileSystemError::InvalidPath {
            path: manifest.source.clone(),
            violation: FileSystemPathViolation::SourceChanged,
        });
    }
    Ok(())
}

fn copy_manifest_regular_file(
    manifest: &CandidateManifestFile,
    destination: &Path,
    cancellation: &AtomicBool,
) -> Result<u64, SystemFileSystemError> {
    ensure_operation_active(cancellation, "复制候选文件", destination)?;
    let mut input = pin_manifest_regular_file(manifest)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|source| io_error("建立候选文件", destination, source))?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut copied = 0_u64;
    loop {
        ensure_operation_active(cancellation, "复制候选文件", destination)?;
        let read = input
            .file_mut()
            .read(&mut buffer)
            .map_err(|source| io_error("读取复制来源文件", &manifest.source, source))?;
        if read == 0 {
            break;
        }
        copied = copied.saturating_add(read as u64);
        output
            .write_all(&buffer[..read])
            .map_err(|source| io_error("写入候选文件", destination, source))?;
        #[cfg(test)]
        cancel_test_candidate_copy_after_chunk(&manifest.source, cancellation);
    }
    ensure_operation_active(cancellation, "复制候选文件", destination)?;
    validate_materialized_source_file(&input, manifest)?;
    if copied != manifest.observed_size {
        return Err(SystemFileSystemError::InvalidPath {
            path: manifest.source.clone(),
            violation: FileSystemPathViolation::SourceChanged,
        });
    }
    output
        .sync_data()
        .map_err(|source| io_error("同步候选文件", destination, source))?;
    ensure_operation_active(cancellation, "复制候选文件", destination)?;
    Ok(copied)
}

fn write_candidate_overlay(
    manifest: &CandidateManifestFile,
    destination: &Path,
    overlay: &DirectoryFileOverlay,
    cancellation: &AtomicBool,
) -> Result<(), SystemFileSystemError> {
    ensure_operation_active(cancellation, "写入候选覆盖", destination)?;
    let input = pin_manifest_regular_file(manifest)?;
    write_candidate_overlay_bytes(destination, overlay, cancellation)?;
    validate_materialized_source_file(&input, manifest)
}

fn write_candidate_overlay_bytes(
    destination: &Path,
    overlay: &DirectoryFileOverlay,
    cancellation: &AtomicBool,
) -> Result<(), SystemFileSystemError> {
    ensure_operation_active(cancellation, "写入候选覆盖", destination)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|source| io_error("建立候选覆盖", destination, source))?;
    for chunk in overlay.bytes().chunks(64 * 1024) {
        ensure_operation_active(cancellation, "写入候选覆盖", destination)?;
        output
            .write_all(chunk)
            .map_err(|source| io_error("写入候选覆盖", destination, source))?;
    }
    ensure_operation_active(cancellation, "写入候选覆盖", destination)?;
    output
        .sync_data()
        .map_err(|source| io_error("同步候选覆盖", destination, source))?;
    ensure_operation_active(cancellation, "写入候选覆盖", destination)
}

#[cfg(test)]
fn copy_regular_file(
    source: &Path,
    destination: &Path,
    expected_identity: FileIdentity,
    observed_size: u64,
) -> Result<(), SystemFileSystemError> {
    let manifest = CandidateManifestFile {
        source: source.to_path_buf(),
        expected_identity,
        observed_size,
        overlay_index: None,
    };
    let cancellation = AtomicBool::new(true);
    copy_manifest_regular_file(&manifest, destination, &cancellation).map(|_| ())
}

#[cfg(test)]
mod tests;
