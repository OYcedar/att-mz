use super::super::SystemFileSystem;
use super::super::error::SystemFileSystemError;
use super::super::test_support::*;
use super::*;
use crate::diagnostic::FileSystemPathViolation;
use crate::runtime::performance::RunPerformanceCounters;
use crate::runtime::windows::{FileIdentity, WindowsFsError, pin_path_without_reparse};
use crate::storage::file_system::{
    DirectoryTreeFingerprintError, DirectoryTreeFingerprintRequest, DirectoryTreeFingerprinter,
};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

fn tree_fingerprint_request(assets: &Path, scripts: &Path) -> DirectoryTreeFingerprintRequest {
    DirectoryTreeFingerprintRequest::new(vec![
        crate::storage::file_system::DirectoryTreeRoot::new(
            assets.to_path_buf(),
            PathBuf::from("assets"),
        )
        .expect("资源指纹根应该合法"),
        crate::storage::file_system::DirectoryTreeRoot::new(
            scripts.to_path_buf(),
            PathBuf::from("scripts"),
        )
        .expect("脚本指纹根应该合法"),
    ])
    .expect("资源与脚本指纹根应该互不重叠")
}

#[tokio::test]
async fn directory_tree_fingerprint_depends_only_on_framed_logical_names_kinds_and_bytes() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let first = temporary.path().join("first");
    let second = temporary.path().join("second");
    for root in [&first, &second] {
        fs::create_dir_all(root.join("assets/catalog")).expect("应该可建立资源子目录");
        fs::create_dir_all(root.join("assets/empty")).expect("应该可建立空目录");
        fs::create_dir_all(root.join("scripts/modules")).expect("应该可建立脚本子目录");
    }
    fs::write(first.join("scripts/modules/main.lua"), b"module").expect("应该可写入第一份脚本");
    fs::write(first.join("assets/catalog/index.json"), b"catalog").expect("应该可写入第一份资源");
    // 反转创建顺序，证明枚举顺序不参与指纹。
    fs::write(second.join("assets/catalog/index.json"), b"catalog").expect("应该可写入第二份资源");
    fs::write(second.join("scripts/modules/main.lua"), b"module").expect("应该可写入第二份脚本");
    let root =
        SystemFileSystem::new_with_worker_threads(2, Arc::new(RunPerformanceCounters::default()))
            .expect("应该可建立文件系统根");

    let first_fingerprint = root
        .fingerprint_directory_tree(tree_fingerprint_request(
            &first.join("assets"),
            &first.join("scripts"),
        ))
        .await
        .expect("第一份目录树应该可建立指纹");
    let second_fingerprint = root
        .fingerprint_directory_tree(tree_fingerprint_request(
            &second.join("assets"),
            &second.join("scripts"),
        ))
        .await
        .expect("第二份目录树应该可建立指纹");
    assert_eq!(first_fingerprint, second_fingerprint);

    fs::write(second.join("assets/catalog/index.json"), b"changed").expect("应该可修改指纹来源");
    let changed_bytes = root
        .fingerprint_directory_tree(tree_fingerprint_request(
            &second.join("assets"),
            &second.join("scripts"),
        ))
        .await
        .expect("修改后应该仍可建立指纹");
    assert_ne!(changed_bytes, first_fingerprint);

    fs::write(second.join("assets/catalog/index.json"), b"catalog").expect("应该可恢复原文件");
    fs::create_dir(second.join("assets/another-empty")).expect("应该可增加空目录");
    let changed_empty_directory = root
        .fingerprint_directory_tree(tree_fingerprint_request(
            &second.join("assets"),
            &second.join("scripts"),
        ))
        .await
        .expect("增加空目录后应该可建立指纹");
    assert_ne!(changed_empty_directory, first_fingerprint);

    root.shutdown().await.expect("文件系统根应该可终结");
}

#[test]
fn relative_directory_walk_keeps_deterministic_depth_first_identity_order() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let first = temporary.path().join("first");
    let second = temporary.path().join("second");
    for root in [&first, &second] {
        fs::create_dir_all(root.join("a/deep")).expect("应该可建立深层目录");
        fs::create_dir_all(root.join("z")).expect("应该可建立末尾目录");
    }
    fs::write(first.join("z/last.json"), b"last").expect("应该可写入末尾文件");
    fs::write(first.join("a/deep/first.json"), b"first").expect("应该可写入深层文件");
    // 反转物理创建顺序，避免把 NTFS 枚举顺序误当成确定性来源。
    fs::write(second.join("a/deep/first.json"), b"first").expect("应该可写入深层文件");
    fs::write(second.join("z/last.json"), b"last").expect("应该可写入末尾文件");

    let root = |physical_root: &Path| FingerprintRoot {
        physical_root: physical_root.to_path_buf(),
        logical_root: PathBuf::from("assets"),
        logical_key: path_utf16_key(Path::new("assets")),
    };
    let first =
        fingerprint_directory_tree_once(&[root(&first)]).expect("第一份树应该可完成单轮观察");
    let second =
        fingerprint_directory_tree_once(&[root(&second)]).expect("第二份树应该可完成单轮观察");
    let order = |observation: &FingerprintObservation| {
        observation
            .identities
            .iter()
            .map(|entry| (entry.entry_type, entry.logical_key.clone()))
            .collect::<Vec<_>>()
    };

    assert_eq!(first.fingerprint, second.fingerprint);
    assert_eq!(order(&first), order(&second));
}

#[tokio::test]
async fn directory_tree_fingerprint_rejects_a_reparse_child_without_following_it() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let assets = temporary.path().join("assets");
    let scripts = temporary.path().join("scripts");
    let outside = temporary.path().join("outside.json");
    fs::create_dir(&assets).expect("应该可建立资源目录");
    fs::create_dir(&scripts).expect("应该可建立脚本目录");
    fs::write(&outside, b"outside").expect("应该可建立树外目标");
    let link = assets.join("linked.json");
    let expected_link = assets
        .canonicalize()
        .expect("应该可规范化无 reparse 的资源根")
        .join("linked.json");
    if let Err(error) = std::os::windows::fs::symlink_file(&outside, &link) {
        if symlink_unavailable(&error) {
            return;
        }
        panic!("应该可建立文件符号链接：{error}");
    }
    let root =
        SystemFileSystem::new_with_worker_threads(2, Arc::new(RunPerformanceCounters::default()))
            .expect("应该可建立文件系统根");

    let error = root
        .fingerprint_directory_tree(tree_fingerprint_request(&assets, &scripts))
        .await
        .expect_err("目录树指纹必须拒绝最终分量 reparse point");
    match error {
        DirectoryTreeFingerprintError::Failed { path, source } => match *source {
            SystemFileSystemError::Windows(WindowsFsError::ReparsePoint { path: reparse_path }) => {
                assert_eq!(path, expected_link, "外层错误必须指向被拒绝的目录项");
                assert_eq!(
                    reparse_path, expected_link,
                    "Windows 错误必须指向被拒绝的 reparse point"
                );
            }
            other => panic!("预期 Windows reparse point 错误，实际来源：{other:?}"),
        },
        other => panic!("预期目录树指纹失败，实际：{other:?}"),
    }
    assert_eq!(fs::read(&outside).expect("树外目标仍应可读取"), b"outside");

    root.shutdown().await.expect("文件系统根应该可终结");
}

#[tokio::test]
async fn directory_tree_fingerprint_rejects_hardlinks() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let assets = temporary.path().join("assets");
    let scripts = temporary.path().join("scripts");
    fs::create_dir(&assets).expect("应该可建立资源目录");
    fs::create_dir(&scripts).expect("应该可建立脚本目录");
    fs::write(assets.join("first.json"), b"same physical file").expect("应该可建立原始文件");
    fs::hard_link(assets.join("first.json"), assets.join("second.json"))
        .expect("本地 NTFS 测试目录应该支持硬链接");
    let root =
        SystemFileSystem::new_with_worker_threads(2, Arc::new(RunPerformanceCounters::default()))
            .expect("应该可建立文件系统根");

    let error = root
        .fingerprint_directory_tree(tree_fingerprint_request(&assets, &scripts))
        .await
        .expect_err("硬链接必须阻止精确目录树指纹");
    assert!(matches!(
        error,
        DirectoryTreeFingerprintError::Failed { source, .. }
            if matches!(
                *source,
                SystemFileSystemError::InvalidPath {
                    violation: FileSystemPathViolation::HardLink,
                    ..
                }
            )
    ));

    root.shutdown().await.expect("文件系统根应该可终结");
}

#[test]
fn directory_tree_fingerprint_rejects_same_bytes_with_new_identity_between_rounds() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let assets = temporary.path().join("assets");
    let scripts = temporary.path().join("scripts");
    fs::create_dir(&assets).expect("应该可建立资源目录");
    fs::create_dir(&scripts).expect("应该可建立脚本目录");
    let observed = assets.join("catalog.json");
    let replacement = temporary.path().join("replacement.json");
    fs::write(&observed, b"same bytes").expect("应该可建立首个对象");
    fs::write(&replacement, b"same bytes").expect("应该可建立不同身份的替换对象");

    let error = fingerprint_directory_tree_sync_with_between(
        tree_fingerprint_request(&assets, &scripts),
        || {
            fs::remove_file(&observed).expect("第一轮结束后应该可移除原对象");
            fs::rename(&replacement, &observed).expect("应该可换入相同内容的新身份对象");
        },
    )
    .expect_err("内容相同但物理身份变化仍必须使观察失败");

    assert!(matches!(
        error,
        DirectoryTreeFingerprintError::ChangedDuringObservation { path }
            if path.ends_with("catalog.json")
    ));
}

#[test]
fn fingerprint_file_rechecks_the_identity_captured_during_enumeration() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let observed = temporary.path().join("catalog.json");
    let replacement = temporary.path().join("replacement.json");
    fs::write(&observed, b"same bytes").expect("应该可建立首个对象");
    fs::write(&replacement, b"same bytes").expect("应该可建立替换对象");
    let enumerated = pin_path_without_reparse(&observed).expect("枚举阶段应该可固定原对象");
    let expected_identity =
        FileIdentity::of(enumerated.file(), &observed).expect("应该可读取枚举身份");
    let expected_size = enumerated.metadata().expect("应该可读取枚举大小").len();
    drop(enumerated);
    fs::remove_file(&observed).expect("应该可移除已枚举对象");
    fs::rename(&replacement, &observed).expect("应该可换入同内容的新对象");

    let mut pass = FingerprintPass {
        file_identities: HashSet::new(),
        identities: Vec::new(),
        hasher: Sha256::new(),
    };
    let error = fingerprint_file(
        &observed,
        Path::new("assets/catalog.json"),
        ExpectedFingerprintFile {
            identity: expected_identity,
            size: expected_size,
        },
        &mut pass,
    )
    .expect_err("目录项在枚举窗口被替换后必须失败");

    assert!(matches!(
        error,
        DirectoryTreeFingerprintError::ChangedDuringObservation { path }
            if path == observed
    ));
}
