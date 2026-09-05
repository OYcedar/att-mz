use super::super::SystemFileSystem;
use super::super::error::SystemFileSystemError;
use super::super::test_support::*;
use crate::diagnostic::FileSystemPathViolation;
use crate::runtime::performance::RunPerformanceCounters;
use crate::storage::file_system::{
    DirectChildDirectoryEnsurer, DirectoryEntry, DirectoryEntryKind, DirectoryLister,
    ExistingDirectoryResolver, FileReader, ListDirectoryError,
};
use std::ffi::OsString;
use std::fs;
use std::sync::Arc;

#[tokio::test]
async fn direct_child_directory_ensure_is_concurrently_idempotent() {
    let temporary = tempfile::tempdir().expect("应该可创建临时项目根");
    let parent = temporary.path().join("projects");
    fs::create_dir(&parent).expect("应该可创建现存父目录");
    let root =
        SystemFileSystem::new_with_worker_threads(2, Arc::new(RunPerformanceCounters::default()))
            .expect("应该可建立文件系统根");

    let first = root.ensure_direct_child_directory(parent.clone(), OsString::from("mz"));
    let second = root.ensure_direct_child_directory(parent.clone(), OsString::from("mz"));
    let (first, second) = tokio::join!(first, second);
    let expected = parent
        .join("mz")
        .canonicalize()
        .expect("MZ 直接子目录应该可规范化");
    assert_eq!(first.expect("首次建立应该成功"), expected);
    assert_eq!(second.expect("并发重复建立应该成功"), expected);
    assert_eq!(fs::read_dir(&parent).expect("应该可列举项目根").count(), 1);

    root.shutdown().await.expect("文件系统根应该可终结");
}

#[tokio::test]
async fn direct_child_directory_ensure_rejects_non_segment_and_existing_file() {
    let temporary = tempfile::tempdir().expect("应该可创建临时项目根");
    let parent = temporary.path().join("projects");
    fs::create_dir(&parent).expect("应该可创建现存父目录");
    let root =
        SystemFileSystem::new_with_worker_threads(2, Arc::new(RunPerformanceCounters::default()))
            .expect("应该可建立文件系统根");

    for child in ["", ".", "nested/mz"] {
        assert!(
            root.ensure_direct_child_directory(parent.clone(), OsString::from(child))
                .await
                .is_err(),
            "非普通单段名称必须被拒绝：{child:?}"
        );
    }
    assert!(!parent.join("nested").exists());

    let missing_parent = temporary.path().join("missing/projects");
    assert!(
        root.ensure_direct_child_directory(missing_parent.clone(), OsString::from("mz"),)
            .await
            .is_err(),
        "能力不得递归建立缺失父目录"
    );
    assert!(!temporary.path().join("missing").exists());

    let occupied = parent.join("mz");
    fs::write(&occupied, b"occupied").expect("应该可用普通文件占用名称");
    assert!(
        root.ensure_direct_child_directory(parent.clone(), OsString::from("mz"))
            .await
            .is_err(),
        "已有普通文件不得被当作目录"
    );
    assert!(occupied.is_file());

    root.shutdown().await.expect("文件系统根应该可终结");
}

#[tokio::test]
async fn direct_child_directory_ensure_rejects_reparse_point() {
    let temporary = tempfile::tempdir().expect("应该可创建临时项目根");
    let parent = temporary.path().join("projects");
    let target = temporary.path().join("foreign");
    fs::create_dir(&parent).expect("应该可创建现存父目录");
    fs::create_dir(&target).expect("应该可创建链接目标");
    let link = parent.join("mz");
    if let Err(error) = std::os::windows::fs::symlink_dir(&target, &link) {
        if symlink_unavailable(&error) {
            return;
        }
        panic!("应该可创建目录符号链接：{error}");
    }
    let root =
        SystemFileSystem::new_with_worker_threads(2, Arc::new(RunPerformanceCounters::default()))
            .expect("应该可建立文件系统根");

    assert!(
        root.ensure_direct_child_directory(parent, OsString::from("mz"))
            .await
            .is_err(),
        "reparse point 不得成为受信直接子目录"
    );
    assert!(target.is_dir(), "拒绝 reparse point 不得修改链接目标");

    root.shutdown().await.expect("文件系统根应该可终结");
}

#[tokio::test]
async fn ordinary_file_capabilities_use_real_unicode_paths_without_att_size_caps() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let directory = temporary.path().join("剧情 数据");
    fs::create_dir(&directory).expect("应该可创建 Unicode 目录");
    let file = directory.join("角色.json");
    fs::write(&file, b"1234").expect("应该可创建 Unicode 文件");
    let root =
        SystemFileSystem::new_with_worker_threads(2, Arc::new(RunPerformanceCounters::default()))
            .expect("应该可建立文件系统根");

    let resolved = root
        .resolve_existing_directory(directory.clone())
        .await
        .expect("现存目录应该可解析");
    assert!(resolved.is_absolute());
    assert_eq!(
        root.list_directory(directory.clone())
            .await
            .expect("目录应该可列举"),
        vec![DirectoryEntry::new(
            file.canonicalize().expect("文件应该可规范化"),
            DirectoryEntryKind::RegularFile,
        )]
    );
    assert_eq!(
        root.read_file(file.clone())
            .await
            .expect("文件应该可读取")
            .into_bytes(),
        b"1234"
    );

    fs::write(&file, vec![0_u8; 1025]).expect("应该可扩大测试文件");
    assert_eq!(
        root.read_file(file)
            .await
            .expect("扩大后的文件仍应实际读取")
            .into_bytes()
            .len(),
        1025
    );
    root.shutdown().await.expect("文件系统根应该可终结");
}

#[tokio::test]
async fn directory_listing_reports_entry_kinds_and_rejects_hardlinked_files() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let directory = temporary.path().join("source");
    let nested = directory.join("nested");
    fs::create_dir_all(&nested).expect("应该可创建测试目录");
    let ordinary = directory.join("state.db");
    fs::write(&ordinary, b"state").expect("应该可创建普通文件");
    let root =
        SystemFileSystem::new_with_worker_threads(2, Arc::new(RunPerformanceCounters::default()))
            .expect("应该可建立文件系统根");

    let mut entries = root
        .list_directory(directory.clone())
        .await
        .expect("目录列举应该返回受信种类");
    entries.sort_by(|left, right| left.resolved_path().cmp(right.resolved_path()));
    let mut expected = vec![
        DirectoryEntry::new(
            nested.canonicalize().expect("目录应该可规范化"),
            DirectoryEntryKind::Directory,
        ),
        DirectoryEntry::new(
            ordinary.canonicalize().expect("文件应该可规范化"),
            DirectoryEntryKind::RegularFile,
        ),
    ];
    expected.sort_by(|left, right| left.resolved_path().cmp(right.resolved_path()));
    assert_eq!(entries, expected);

    let hardlink = directory.join("state-copy.db");
    fs::hard_link(&ordinary, &hardlink).expect("应该可创建硬链接测试入口");
    assert!(matches!(
        root.list_directory(directory).await,
        Err(ListDirectoryError::Io {
            source: SystemFileSystemError::InvalidPath {
                violation: FileSystemPathViolation::HardLink,
                ..
            },
            ..
        })
    ));

    root.shutdown().await.expect("文件系统根应该可终结");
}
