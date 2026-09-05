use super::super::error::SystemFileSystemError;
use super::super::test_support::*;
use super::*;
use crate::runtime::windows::{FileIdentity, WindowsFsError, open_directory};
use std::fs;

#[test]
fn identity_fixed_cleanup_removes_the_complete_expected_directory_tree() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let root = temporary.path().join("candidate");
    fs::create_dir_all(root.join("嵌套/更深")).expect("应该可创建候选目录树");
    fs::write(root.join("root.txt"), b"root").expect("应该可创建根文件");
    fs::write(root.join("嵌套/child.txt"), b"child").expect("应该可创建子文件");
    fs::write(root.join("嵌套/更深/leaf.bin"), b"leaf").expect("应该可创建叶子文件");
    let handle = open_directory(&root, true).expect("应该可打开候选根");
    let identity = FileIdentity::of(&handle, &root).expect("应该可读取候选根身份");
    drop(handle);

    remove_directory_tree_if_identity(&root, identity)
        .expect("身份匹配的整棵候选目录树应该被精确删除");

    assert!(!root.exists());
}

#[test]
fn identity_fixed_cleanup_refuses_and_preserves_a_foreign_root_replacement() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let root = temporary.path().join("candidate");
    let displaced = temporary.path().join("displaced-candidate");
    fs::create_dir(&root).expect("应该可创建原候选根");
    fs::write(root.join("owned.txt"), b"owned").expect("应该可创建候选文件");
    let handle = open_directory(&root, true).expect("应该可打开原候选根");
    let original_identity = FileIdentity::of(&handle, &root).expect("应该可读取原候选根身份");
    drop(handle);

    fs::rename(&root, &displaced).expect("应该可移开原候选根");
    fs::create_dir(&root).expect("应该可在同路径创建外来目录");
    fs::write(root.join("foreign.txt"), b"foreign").expect("应该可创建外来文件");

    assert!(matches!(
        remove_directory_tree_if_identity(&root, original_identity),
        Err(SystemFileSystemError::InvalidStagedIdentity { path }) if path == root
    ));
    assert_eq!(
        fs::read(root.join("foreign.txt")).expect("外来文件应该被保留"),
        b"foreign"
    );
    assert!(displaced.join("owned.txt").is_file());
}

#[test]
fn identity_fixed_cleanup_rejects_a_reparse_child_without_following_it() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let root = temporary.path().join("candidate");
    let external = temporary.path().join("external.txt");
    let link = root.join("linked.txt");
    fs::create_dir(&root).expect("应该可创建候选根");
    fs::write(&external, b"must-stay").expect("应该可创建外部目标");
    if let Err(error) = std::os::windows::fs::symlink_file(&external, &link) {
        if symlink_unavailable(&error) {
            return;
        }
        panic!("应该可创建文件符号链接：{error}");
    }
    let handle = open_directory(&root, true).expect("应该可打开候选根");
    let identity = FileIdentity::of(&handle, &root).expect("应该可读取候选根身份");
    drop(handle);

    assert!(matches!(
        remove_directory_tree_if_identity(&root, identity),
        Err(SystemFileSystemError::Windows(WindowsFsError::ReparsePoint { path }))
            if path == link
    ));
    assert_eq!(
        fs::read(&external).expect("外部目标应该仍可读取"),
        b"must-stay"
    );
    assert!(root.exists());
}
