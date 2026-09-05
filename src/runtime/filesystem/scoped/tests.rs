use super::super::error::SystemFileSystemError;
use super::super::test_support::*;
use crate::diagnostic::FileSystemPathViolation;
use crate::runtime::windows::WindowsFsError;
use crate::storage::file_system::{
    DirectoryPublishIntent, DirectorySourceMapping, DirectoryStageRequest,
    RecoverableDirectoryPublisher, ScopedDirectoryBindError, ScopedDirectoryEditError,
    ScopedDirectoryEditor, ScopedDirectoryEntry, ScopedDirectoryEntryKind, ScopedDirectoryPath,
    ScopedDirectoryScope,
};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

fn scoped_stage_request(
    target: PathBuf,
    source_root: &Path,
    intent: DirectoryPublishIntent,
) -> DirectoryStageRequest {
    DirectoryStageRequest::new(
        target,
        intent,
        vec![
            DirectorySourceMapping::new(source_root.join("assets"), PathBuf::from("assets"))
                .expect("资源候选映射应该合法"),
            DirectorySourceMapping::new(source_root.join("scripts"), PathBuf::from("scripts"))
                .expect("脚本候选映射应该合法"),
        ],
        Vec::new(),
        Vec::new(),
    )
    .expect("受限编辑候选请求应该合法")
}

fn scoped_path(path: &str) -> ScopedDirectoryPath {
    ScopedDirectoryPath::new(PathBuf::from(path)).expect("测试候选路径应该合法")
}

fn scoped_scope() -> ScopedDirectoryScope {
    ScopedDirectoryScope::new([OsString::from("assets"), OsString::from("scripts")])
        .expect("测试候选范围应该合法")
}

fn scoped_source(root: &Path) {
    fs::create_dir_all(root.join("assets")).expect("应该可建立资源来源");
    fs::create_dir_all(root.join("scripts")).expect("应该可建立脚本来源");
    fs::write(root.join("assets/catalog.json"), b"items").expect("应该可建立资源文件");
    fs::write(root.join("scripts/main.lua"), b"scripts").expect("应该可建立脚本文件");
}

#[tokio::test]
async fn scoped_editor_mutates_only_declared_roots_and_the_publisher_revalidates_the_result() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source");
    scoped_source(&source);
    let target = temporary.path().join("output");
    let root = TestDirectoryPublisher::new();
    let staged = root
        .prepare(scoped_stage_request(
            target.clone(),
            &source,
            DirectoryPublishIntent::CreateNew,
        ))
        .await
        .expect("候选应该可准备");
    let scope = root
        .bind_scoped_directory(&staged, scoped_scope())
        .await
        .expect("应该可绑定未发布候选");

    assert_eq!(
        root.list_scoped_directory(&scope, scoped_path("assets"))
            .await
            .expect("应该可列举资源目录"),
        vec![ScopedDirectoryEntry::new(
            OsString::from("catalog.json"),
            ScopedDirectoryEntryKind::File,
        )]
    );
    root.create_scoped_directory(&scope, scoped_path("assets/generated/nested"))
        .await
        .expect("应该可逐段建立候选目录");
    root.write_scoped_file(
        &scope,
        scoped_path("assets/generated/nested/result.json"),
        b"generated".to_vec(),
    )
    .await
    .expect("应该可建立候选文件");
    root.write_scoped_file(&scope, scoped_path("scripts/main.lua"), b"changed".to_vec())
        .await
        .expect("应该可替换候选文件");
    for operation in [
        root.create_scoped_directory(&scope, scoped_path("assets"))
            .await,
        root.write_scoped_file(&scope, scoped_path("assets"), Vec::new())
            .await,
    ] {
        assert!(matches!(
            operation,
            Err(ScopedDirectoryEditError::ScopeRootMutation { .. })
        ));
    }

    fn assert_send<T: Send>(_: T) {}
    assert_send(root.list_scoped_directory(&scope, scoped_path("scripts")));
    drop(scope);

    assert_eq!(
        root.candidate_validation_counts(),
        (0, 0),
        "候选构建和受限编辑中的树遍历不得冒充最终完整校验"
    );
    root.publish(staged)
        .await
        .expect("发布根应该重新验证并整体发布编辑后候选");
    assert_eq!(
        root.candidate_validation_counts(),
        (1, 1),
        "完整 candidate 全树校验必须且只能发生一次"
    );
    assert_eq!(
        fs::read(target.join("assets/catalog.json")).expect("未修改文件应该原样发布"),
        b"items"
    );
    assert_eq!(
        fs::read(target.join("assets/generated/nested/result.json"))
            .expect("应该可读取已发布新文件"),
        b"generated"
    );
    assert_eq!(
        fs::read(target.join("scripts/main.lua")).expect("应该可读取已发布替换文件"),
        b"changed"
    );
    root.shutdown().await.expect("文件系统根应该可终结");
}

#[tokio::test]
async fn scoped_root_listing_is_generic_and_reports_all_top_level_entries() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source");
    scoped_source(&source);
    let root = TestDirectoryPublisher::new();
    let staged = root
        .prepare(scoped_stage_request(
            temporary.path().join("output"),
            &source,
            DirectoryPublishIntent::CreateNew,
        ))
        .await
        .expect("候选应该可准备");
    let candidate_root = staged.staging_root().to_path_buf();
    let scope = root
        .bind_scoped_directory(&staged, scoped_scope())
        .await
        .expect("应该可绑定候选");

    fs::create_dir(candidate_root.join("evil")).expect("测试应可模拟可信 Lua 绕过门面写入");
    assert_eq!(
        root.list_scoped_root(&scope)
            .await
            .expect("应该可列举候选根")
            .into_iter()
            .map(|entry| entry.name().to_os_string())
            .collect::<Vec<_>>(),
        vec![
            OsString::from("assets"),
            OsString::from("evil"),
            OsString::from("scripts"),
        ]
    );
    fs::remove_dir(candidate_root.join("evil")).expect("应该可清理测试目录");

    drop(scope);
    root.discard(staged).await.expect("应该可丢弃候选");
    root.shutdown().await.expect("文件系统根应该可终结");
}

#[tokio::test]
async fn scoped_editor_accepts_arbitrary_declared_top_level_directories() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source");
    fs::create_dir_all(source.join("assets")).expect("应该可建立 assets 来源");
    fs::create_dir_all(source.join("scripts")).expect("应该可建立 scripts 来源");
    fs::write(source.join("assets/title.png"), b"image").expect("应该可建立资源文件");
    let request = DirectoryStageRequest::new(
        temporary.path().join("output"),
        DirectoryPublishIntent::CreateNew,
        vec![
            DirectorySourceMapping::new(source.join("assets"), PathBuf::from("assets"))
                .expect("assets 映射应该合法"),
            DirectorySourceMapping::new(source.join("scripts"), PathBuf::from("scripts"))
                .expect("scripts 映射应该合法"),
        ],
        Vec::new(),
        Vec::new(),
    )
    .expect("通用候选请求应该合法");
    let root = TestDirectoryPublisher::new();
    let staged = root.prepare(request).await.expect("候选应该可准备");
    let scope = root
        .bind_scoped_directory(
            &staged,
            ScopedDirectoryScope::new([OsString::from("assets"), OsString::from("scripts")])
                .expect("通用范围应该合法"),
        )
        .await
        .expect("通用顶层目录应该可绑定");

    assert_eq!(
        root.list_scoped_directory(&scope, scoped_path("assets"))
            .await
            .expect("应该可列举声明范围内目录"),
        vec![ScopedDirectoryEntry::new(
            OsString::from("title.png"),
            ScopedDirectoryEntryKind::File,
        )]
    );
    assert!(matches!(
        root.list_scoped_directory(&scope, scoped_path("other"))
            .await,
        Err(ScopedDirectoryEditError::OutsideScope { .. })
    ));

    drop(scope);
    root.discard(staged).await.expect("应该可丢弃候选");
    root.shutdown().await.expect("文件系统根应该可终结");
}

#[tokio::test]
async fn scoped_tokens_are_bound_to_the_creating_file_system_instance() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source");
    scoped_source(&source);
    let owner = TestDirectoryPublisher::new();
    let foreign = TestDirectoryPublisher::new();
    let staged = owner
        .prepare(scoped_stage_request(
            temporary.path().join("output"),
            &source,
            DirectoryPublishIntent::CreateNew,
        ))
        .await
        .expect("候选应该可准备");

    assert!(matches!(
        foreign.bind_scoped_directory(&staged, scoped_scope()).await,
        Err(ScopedDirectoryBindError::WrongEditorInstance)
    ));
    let scope = owner
        .bind_scoped_directory(&staged, scoped_scope())
        .await
        .expect("所有者应该可绑定候选");
    assert!(matches!(
        foreign
            .list_scoped_directory(&scope, scoped_path("assets"))
            .await,
        Err(ScopedDirectoryEditError::WrongEditorInstance)
    ));
    drop(scope);
    owner.discard(staged).await.expect("所有者应该可丢弃候选");
    foreign.shutdown().await.expect("外来根应该可终结");
    owner.shutdown().await.expect("所有者根应该可终结");
}

#[tokio::test]
async fn scoped_editor_rejects_hardlinks_and_reparse_points() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source");
    scoped_source(&source);
    let root = TestDirectoryPublisher::new();
    let staged = root
        .prepare(scoped_stage_request(
            temporary.path().join("output"),
            &source,
            DirectoryPublishIntent::CreateNew,
        ))
        .await
        .expect("候选应该可准备");
    let staging_root = staged.staging_root().to_path_buf();
    let scope = root
        .bind_scoped_directory(&staged, scoped_scope())
        .await
        .expect("应该可绑定候选");

    let hardlink = staging_root.join("assets/hardlink.json");
    fs::hard_link(staging_root.join("assets/catalog.json"), &hardlink)
        .expect("本地 NTFS 测试目录应支持硬链接");
    assert!(matches!(
        root.list_scoped_directory(&scope, scoped_path("assets"))
            .await,
        Err(ScopedDirectoryEditError::Failed { source, .. })
            if matches!(*source, SystemFileSystemError::InvalidPath {
                violation: FileSystemPathViolation::HardLink,
                ..
            })
    ));
    assert!(matches!(
        root.write_scoped_file(
            &scope,
            scoped_path("assets/hardlink.json"),
            b"changed".to_vec(),
        )
        .await,
        Err(ScopedDirectoryEditError::Failed { source, .. })
            if matches!(*source, SystemFileSystemError::InvalidPath {
                violation: FileSystemPathViolation::HardLink,
                ..
            })
    ));
    assert_eq!(
        fs::read(staging_root.join("assets/catalog.json")).unwrap(),
        b"items",
        "硬链接写入失败不得改变共享物理文件"
    );
    fs::remove_file(&hardlink).expect("应该可移除硬链接测试入口");

    let external = temporary.path().join("external.txt");
    let link = staging_root.join("assets/linked.txt");
    fs::write(&external, b"external").expect("应该可建立外部文件");
    match std::os::windows::fs::symlink_file(&external, &link) {
        Ok(()) => {
            assert!(matches!(
                root.list_scoped_directory(&scope, scoped_path("assets"))
                    .await,
                Err(ScopedDirectoryEditError::Failed { source, .. })
                    if matches!(*source, SystemFileSystemError::Windows(
                        WindowsFsError::ReparsePoint { .. }
                    ))
            ));
            fs::remove_file(&link).expect("应该可移除 reparse 测试入口");
        }
        Err(error) if symlink_unavailable(&error) => {}
        Err(error) => panic!("应该可创建文件符号链接：{error}"),
    }

    drop(scope);
    root.discard(staged).await.expect("应该可丢弃候选");
    root.shutdown().await.expect("文件系统根应该可终结");
}

#[tokio::test]
async fn every_scoped_operation_applies_the_same_windows_path_validation() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source");
    scoped_source(&source);
    let root = TestDirectoryPublisher::new();
    let staged = root
        .prepare(scoped_stage_request(
            temporary.path().join("output"),
            &source,
            DirectoryPublishIntent::CreateNew,
        ))
        .await
        .expect("候选应该可准备");
    let staging_root = staged.staging_root().to_path_buf();
    let scope = root
        .bind_scoped_directory(&staged, scoped_scope())
        .await
        .expect("应该可绑定候选");
    let invalid = || scoped_path("assets/CON");

    assert!(matches!(
        root.list_scoped_directory(&scope, invalid()).await,
        Err(ScopedDirectoryEditError::Failed { source, .. })
            if matches!(*source, SystemFileSystemError::InvalidPath { .. })
    ));
    assert!(matches!(
        root.create_scoped_directory(&scope, invalid()).await,
        Err(ScopedDirectoryEditError::Failed { source, .. })
            if matches!(*source, SystemFileSystemError::InvalidPath { .. })
    ));
    assert!(matches!(
        root.write_scoped_file(&scope, invalid(), b"forbidden".to_vec())
            .await,
        Err(ScopedDirectoryEditError::Failed { source, .. })
            if matches!(*source, SystemFileSystemError::InvalidPath { .. })
    ));
    assert!(
        fs::read_dir(staging_root.join("assets"))
            .unwrap()
            .all(|entry| entry.unwrap().file_name() != OsStr::new("CON")),
        "非法设备名操作不得建立目录项"
    );

    drop(scope);
    root.discard(staged).await.expect("应该可丢弃候选");
    root.shutdown().await.expect("文件系统根应该可终结");
}

#[cfg(feature = "release-stress")]
#[tokio::test]
async fn release_stress_scoped_write_accepts_growth_without_an_att_tree_budget() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source");
    fs::create_dir_all(source.join("assets")).expect("应该可建立资源来源");
    fs::create_dir_all(source.join("scripts")).expect("应该可建立脚本来源");
    fs::write(source.join("assets/a.json"), b"1234").expect("应该可写入资源来源");
    fs::write(source.join("scripts/a.lua"), b"12").expect("应该可写入脚本来源");
    let root = TestDirectoryPublisher::new();
    let staged = root
        .prepare(scoped_stage_request(
            temporary.path().join("output"),
            &source,
            DirectoryPublishIntent::CreateNew,
        ))
        .await
        .expect("候选应可准备");
    let candidate_root = staged.staging_root().to_path_buf();
    let scope = root
        .bind_scoped_directory(&staged, scoped_scope())
        .await
        .expect("应该可绑定候选");

    root.write_scoped_file(
        &scope,
        scoped_path("assets/additional.json"),
        vec![1; 1024 * 1024],
    )
    .await
    .expect("候选增长不应触发 ATT 人工总字节拒绝");
    assert_eq!(
        fs::metadata(candidate_root.join("assets/additional.json"))
            .expect("新增文件应存在")
            .len(),
        1024 * 1024
    );

    drop(scope);
    root.discard(staged).await.expect("应该可丢弃候选");
    root.shutdown().await.expect("文件系统根应该可终结");
}
