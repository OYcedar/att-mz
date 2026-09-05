use super::super::error::SystemFileSystemError;
use super::super::test_faults::register_test_candidate_copy_cancellation;
use super::super::test_support::*;
use super::*;
use crate::diagnostic::FileSystemPathViolation;
use crate::runtime::windows::{
    FileIdentity, pin_path_without_reparse, pin_regular_file_for_snapshot_read,
};
use crate::storage::file_system::{
    DirectoryFileOverlay, DirectoryPrepareError, DirectoryPublishIntent, DirectorySourceMapping,
    DirectoryStageRequest, RecoverableDirectoryPublisher,
};
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

#[test]
fn declared_paths_reject_conflicting_windows_case_spelling() {
    let mappings = vec![
        DirectorySourceMapping::new(PathBuf::from("first"), PathBuf::from("content/first"))
            .expect("第一条声明应合法"),
        DirectorySourceMapping::new(PathBuf::from("second"), PathBuf::from("Content/second"))
            .expect("大小写不同的原始声明尚未进入 Windows 边界"),
    ];
    assert!(matches!(
        validate_declared_windows_paths(&mappings, &[], &[]),
        Err(SystemFileSystemError::InvalidPath {
            violation: FileSystemPathViolation::CaseCollision,
            ..
        })
    ));
}

#[tokio::test]
async fn preparing_a_candidate_rejects_hardlinked_source_files() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source");
    fs::create_dir(&source).expect("应该可建立来源目录");
    fs::write(source.join("first.json"), b"shared").expect("应该可建立来源文件");
    fs::hard_link(source.join("first.json"), source.join("second.json"))
        .expect("本地 NTFS 测试目录应该支持硬链接");
    let root = TestDirectoryPublisher::new();

    let error = match root
        .prepare(stage_request(
            temporary.path().join("target"),
            source,
            DirectoryPublishIntent::CreateNew,
        ))
        .await
    {
        Ok(_) => panic!("复制来源中的硬链接必须阻止候选准备"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        DirectoryPrepareError::NotPrepared { source, .. }
            if matches!(*source, SystemFileSystemError::InvalidPath {
                violation: FileSystemPathViolation::HardLink,
                ..
            })
    ));
    root.shutdown().await.expect("文件系统根应该可终结");
}

#[test]
fn copying_a_file_rechecks_the_enumerated_identity_and_size() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source.json");
    let replacement = temporary.path().join("replacement.json");
    let destination = temporary.path().join("candidate.json");
    fs::write(&source, b"same bytes").expect("应该可建立原来源文件");
    fs::write(&replacement, b"same bytes").expect("应该可建立替换来源文件");
    let enumerated = pin_path_without_reparse(&source).expect("枚举阶段应该可固定来源");
    let expected_identity =
        FileIdentity::of(enumerated.file(), &source).expect("应该可读取来源身份");
    let expected_size = enumerated.metadata().expect("应该可读取来源大小").len();
    drop(enumerated);
    fs::remove_file(&source).expect("应该可移除枚举时来源");
    fs::rename(&replacement, &source).expect("应该可换入相同大小和内容的新身份来源");

    let error = copy_regular_file(&source, &destination, expected_identity, expected_size)
        .expect_err("来源在枚举与复制之间替换必须失败");
    assert!(matches!(
        error,
        SystemFileSystemError::InvalidPath {
            violation: FileSystemPathViolation::SourceChanged,
            ..
        }
    ));
    assert!(!destination.exists());
}

#[test]
fn overlay_aware_manifest_is_sorted_and_materializes_each_file_once() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source");
    let stage = temporary.path().join("stage");
    let target = temporary.path().join("target");
    fs::create_dir(&source).expect("应该可建立来源目录");
    fs::create_dir(&stage).expect("应该可建立候选目录");
    fs::write(source.join("z.json"), b"untouched").expect("应该可建立未覆盖来源");
    fs::write(source.join("catalog.json"), b"source bytes").expect("应该可建立待覆盖来源");
    let mappings = vec![
        DirectorySourceMapping::new(source, PathBuf::from("snapshot/content"))
            .expect("测试来源映射应合法"),
    ];
    let overlays = vec![
        DirectoryFileOverlay::new(
            PathBuf::from("snapshot/content/catalog.json"),
            b"translated".to_vec(),
        )
        .expect("测试覆盖应合法"),
    ];
    let cancellation = AtomicBool::new(true);
    let manifest =
        build_candidate_manifest(&stage, &target, &mappings, &overlays, &[], &cancellation)
            .expect("覆盖感知 manifest 应可建立");
    let source_tree = manifest
        .operations
        .iter()
        .find_map(|operation| match operation {
            CandidateManifestOperation::CopySource { source_tree, .. } => Some(source_tree),
            CandidateManifestOperation::EnsureDirectory(_)
            | CandidateManifestOperation::WriteOverlay { .. } => None,
        })
        .expect("manifest 应包含来源树");
    let directory = &source_tree.directories[source_tree.root_directory];
    assert_eq!(
        directory
            .entries
            .iter()
            .map(|entry| entry.name.clone())
            .collect::<Vec<_>>(),
        [OsString::from("catalog.json"), OsString::from("z.json")]
    );
    let CandidateManifestEntryKind::File(catalog) = directory.entries[0].kind else {
        panic!("catalog 应为普通文件")
    };
    let CandidateManifestEntryKind::File(untouched) = directory.entries[1].kind else {
        panic!("z.json 应为普通文件")
    };
    assert_eq!(source_tree.files[catalog].overlay_index, Some(0));
    assert_eq!(source_tree.files[untouched].overlay_index, None);

    materialize_candidate_manifest(&stage, &manifest, &overlays, &cancellation, 4)
        .expect("manifest 应可物化");
    assert_eq!(
        fs::read(stage.join("snapshot/content/catalog.json")).expect("应该可读取最终覆盖"),
        b"translated"
    );
    assert_eq!(
        fs::read(stage.join("snapshot/content/z.json")).expect("应该可读取未覆盖副本"),
        b"untouched"
    );
}

#[test]
fn standalone_overlays_create_parent_directories_and_write_exact_bytes() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source");
    let stage = temporary.path().join("stage");
    let target = temporary.path().join("target");
    fs::create_dir(&source).expect("应该可建立来源目录");
    fs::create_dir(&stage).expect("应该可建立候选目录");
    fs::write(source.join("catalog.json"), b"source").expect("应该可建立来源文件");
    let mappings = vec![
        DirectorySourceMapping::new(source, PathBuf::from("content")).expect("测试来源映射应合法"),
    ];
    let overlays = vec![
        DirectoryFileOverlay::new(PathBuf::from("package.json"), b"package".to_vec())
            .expect("根文件覆盖应合法"),
        DirectoryFileOverlay::new(
            PathBuf::from("shell/index.html"),
            b"<title>translated</title>".to_vec(),
        )
        .expect("嵌套文件覆盖应合法"),
    ];
    let cancellation = AtomicBool::new(true);
    let manifest =
        build_candidate_manifest(&stage, &target, &mappings, &overlays, &[], &cancellation)
            .expect("独立覆盖应进入候选 manifest");

    materialize_candidate_manifest(&stage, &manifest, &overlays, &cancellation, 4)
        .expect("独立覆盖应可物化");

    assert_eq!(
        fs::read(stage.join("package.json")).expect("应该可读取根覆盖"),
        b"package"
    );
    assert_eq!(
        fs::read(stage.join("shell/index.html")).expect("应该可读取嵌套覆盖"),
        b"<title>translated</title>"
    );
    assert_eq!(
        fs::read(stage.join("content/catalog.json")).expect("应该保留来源树"),
        b"source"
    );
}

#[test]
fn standalone_overlay_uses_shared_file_task_error_order() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source.json");
    let source_destination = temporary.path().join("source-output.json");
    let standalone_destination = temporary.path().join("package.json");
    fs::write(&source, b"source").expect("应该可建立来源文件");
    fs::write(&source_destination, b"occupied").expect("应该可占用来源输出路径");
    fs::write(&standalone_destination, b"occupied").expect("应该可占用独立覆盖路径");

    let enumerated = pin_path_without_reparse(&source).expect("应该可固定来源文件");
    let manifest = CandidateManifestFile {
        source: source.clone(),
        expected_identity: FileIdentity::of(enumerated.file(), &source)
            .expect("应该可读取来源身份"),
        observed_size: enumerated.metadata().expect("应该可读取来源大小").len(),
        overlay_index: None,
    };
    drop(enumerated);
    let overlays = vec![
        DirectoryFileOverlay::new(PathBuf::from("package.json"), b"overlay".to_vec())
            .expect("独立覆盖应合法"),
    ];
    let files = vec![
        CandidateFileTask {
            ordinal: 0,
            kind: CandidateFileTaskKind::Source(&manifest),
            destination: source_destination.clone(),
        },
        CandidateFileTask {
            ordinal: 1,
            kind: CandidateFileTaskKind::StandaloneOverlay { overlay_index: 0 },
            destination: standalone_destination,
        },
    ];
    let cancellation = AtomicBool::new(true);

    let error = materialize_candidate_files(&files, &overlays, &cancellation, 2)
        .expect_err("两个文件任务失败时必须按声明顺序选择主错误");
    assert!(matches!(
        error,
        SystemFileSystemError::Io { path, .. } if path == source_destination
    ));
}

#[test]
fn overlay_source_identity_is_rechecked_before_the_single_write() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source");
    let stage = temporary.path().join("stage");
    let target = temporary.path().join("target");
    fs::create_dir(&source).expect("应该可建立来源目录");
    fs::create_dir(&stage).expect("应该可建立候选目录");
    let observed = source.join("catalog.json");
    let replacement = temporary.path().join("replacement.json");
    fs::write(&observed, b"same bytes").expect("应该可建立原来源文件");
    fs::write(&replacement, b"same bytes").expect("应该可建立替换文件");
    let mappings = vec![
        DirectorySourceMapping::new(source, PathBuf::from("content")).expect("测试来源映射应合法"),
    ];
    let overlays = vec![
        DirectoryFileOverlay::new(
            PathBuf::from("content/catalog.json"),
            b"translated".to_vec(),
        )
        .expect("测试覆盖应合法"),
    ];
    let cancellation = AtomicBool::new(true);
    let manifest =
        build_candidate_manifest(&stage, &target, &mappings, &overlays, &[], &cancellation)
            .expect("manifest 应冻结枚举身份");
    fs::remove_file(&observed).expect("应该可移除已枚举来源");
    fs::rename(&replacement, &observed).expect("应该可换入同大小来源");

    let error = materialize_candidate_manifest(&stage, &manifest, &overlays, &cancellation, 4)
        .expect_err("覆盖来源身份变化必须阻止写入");
    assert!(matches!(
        error,
        SystemFileSystemError::InvalidPath {
            violation: FileSystemPathViolation::SourceChanged,
            ..
        }
    ));
    assert!(!stage.join("content/catalog.json").exists());
}

#[test]
fn candidate_file_copy_observes_cancellation_between_bounded_chunks() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source");
    let stage = temporary.path().join("stage");
    let target = temporary.path().join("target");
    fs::create_dir(&source).expect("应该可建立来源目录");
    fs::create_dir(&stage).expect("应该可建立候选目录");
    let source_file = source.join("large.bin");
    fs::write(&source_file, vec![7_u8; 3 * 64 * 1024]).expect("应该可建立多块来源文件");
    let mappings = vec![
        DirectorySourceMapping::new(source, PathBuf::from("content")).expect("测试来源映射应合法"),
    ];
    let cancellation = AtomicBool::new(true);
    let manifest = build_candidate_manifest(&stage, &target, &mappings, &[], &[], &cancellation)
        .expect("manifest 应可建立");
    let registered_source = pin_regular_file_for_snapshot_read(&source_file)
        .expect("应该可固定测试来源")
        .resolved_path()
        .to_path_buf();
    register_test_candidate_copy_cancellation(registered_source);

    let error = materialize_candidate_manifest(&stage, &manifest, &[], &cancellation, 1)
        .expect_err("复制必须在分块边界观察取消");

    assert!(matches!(
        error,
        SystemFileSystemError::Cancelled {
            operation: "复制候选文件",
            path,
        } if path.ends_with("content/large.bin")
    ));
    assert_eq!(
        fs::metadata(stage.join("content/large.bin"))
            .expect("取消前写入的第一块应仍位于不可见候选中")
            .len(),
        64 * 1024,
        "取消后不得继续复制剩余块"
    );
}

#[cfg(feature = "release-stress")]
#[test]
fn release_stress_overlay_source_has_no_att_size_cap() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let oversized_source = temporary.path().join("oversized-source");
    let oversized_stage = temporary.path().join("oversized-stage");
    fs::create_dir(&oversized_source).expect("应该可建立超限来源目录");
    fs::create_dir(&oversized_stage).expect("应该可建立超限候选目录");
    fs::write(
        oversized_source.join("catalog.json"),
        vec![0_u8; 512 * 1024 + 1],
    )
    .expect("应该可建立超限来源文件");
    let oversized_mappings = vec![
        DirectorySourceMapping::new(oversized_source, PathBuf::from("content"))
            .expect("测试来源映射应合法"),
    ];
    let overlays = vec![
        DirectoryFileOverlay::new(PathBuf::from("content/catalog.json"), vec![1])
            .expect("测试覆盖应合法"),
    ];
    let cancellation = AtomicBool::new(true);
    build_candidate_manifest(
        &oversized_stage,
        &temporary.path().join("oversized-target"),
        &oversized_mappings,
        &overlays,
        &[],
        &cancellation,
    )
    .expect("ATT 不应按来源文件大小提前拒绝覆盖 manifest");
}

#[test]
fn overlay_source_rejects_hardlinks() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let hardlink_source = temporary.path().join("hardlink-source");
    let hardlink_stage = temporary.path().join("hardlink-stage");
    fs::create_dir(&hardlink_source).expect("应该可建立硬链接来源目录");
    fs::create_dir(&hardlink_stage).expect("应该可建立硬链接候选目录");
    let linked = hardlink_source.join("catalog.json");
    fs::write(&linked, b"source").expect("应该可建立硬链接来源文件");
    fs::hard_link(&linked, temporary.path().join("external-link.json"))
        .expect("本地 NTFS 测试目录应该支持硬链接");
    let hardlink_mappings = vec![
        DirectorySourceMapping::new(hardlink_source, PathBuf::from("content"))
            .expect("测试来源映射应合法"),
    ];
    let overlays = vec![
        DirectoryFileOverlay::new(PathBuf::from("content/catalog.json"), vec![1])
            .expect("测试覆盖应合法"),
    ];
    let cancellation = AtomicBool::new(true);
    let error = build_candidate_manifest(
        &hardlink_stage,
        &temporary.path().join("hardlink-target"),
        &hardlink_mappings,
        &overlays,
        &[],
        &cancellation,
    )
    .expect_err("覆盖不能绕过来源硬链接拒绝");
    assert!(matches!(
        error,
        SystemFileSystemError::InvalidPath {
            violation: FileSystemPathViolation::HardLink,
            ..
        }
    ));
}

#[tokio::test]
async fn root_source_mapping_replaces_the_target_with_the_exact_source_tree() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source");
    let target = temporary.path().join("output");
    fs::create_dir_all(source.join("nested")).expect("应该可建立嵌套来源");
    fs::write(source.join("scene.jsonl"), b"scene\n").expect("应该可建立顶层来源文件");
    fs::write(source.join("nested/extra.jsonl"), b"extra\n").expect("应该可建立嵌套来源文件");
    fs::create_dir(&target).expect("应该可建立旧输出目录");
    fs::write(target.join("stale.jsonl"), b"stale\n").expect("应该可建立旧输出文件");

    let request = DirectoryStageRequest::new(
        target.clone(),
        DirectoryPublishIntent::ReplaceExisting,
        vec![
            DirectorySourceMapping::new(source, PathBuf::new())
                .expect("来源根应该可以直接映射到候选根"),
        ],
        Vec::new(),
        Vec::new(),
    )
    .expect("根映射候选请求应该合法");
    let root = TestDirectoryPublisher::new();
    let staged = root.prepare(request).await.expect("根映射候选应该可准备");

    assert_eq!(
        fs::read(staged.staging_root().join("scene.jsonl")).expect("候选根应直接包含顶层文件"),
        b"scene\n"
    );
    assert_eq!(
        fs::read(staged.staging_root().join("nested/extra.jsonl")).expect("候选根应保留嵌套路径"),
        b"extra\n"
    );
    root.publish(staged)
        .await
        .expect("根映射候选应该可整体发布");

    assert_eq!(
        fs::read(target.join("scene.jsonl")).expect("应该可读取已发布顶层文件"),
        b"scene\n"
    );
    assert_eq!(
        fs::read(target.join("nested/extra.jsonl")).expect("应该可读取已发布嵌套文件"),
        b"extra\n"
    );
    assert!(!target.join("stale.jsonl").exists(), "旧输出不得残留");
    root.shutdown().await.expect("文件系统根应该可终结");
}
