use super::super::SystemFileSystem;
use super::super::error::SystemFileSystemError;
use super::super::test_faults::{TestObservationFaultPoint, register_test_observation_faults};
use super::*;
use crate::diagnostic::FileSystemRecoveryViolation;
use crate::runtime::performance::RunPerformanceCounters;
use crate::runtime::windows::{WindowsFsError, create_new_atomic_replace_candidate};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fs, io};

#[tokio::test]
async fn terminal_observation_commits_complete_content_without_overwrite() {
    let directory = tempfile::tempdir().expect("应该可建立测试目录");
    let target = directory.path().join("task-000001.md");
    let file_system =
        SystemFileSystem::new_with_worker_threads(1, Arc::new(RunPerformanceCounters::default()))
            .expect("应该可建立文件系统根");

    file_system
        .write_new_terminal_observation_file(target.clone(), b"complete document".to_vec())
        .await
        .expect("完整终态文档应该可原子提交");
    assert_eq!(
        fs::read(&target).expect("应该可读取终态文档"),
        b"complete document"
    );

    let error = file_system
        .write_new_terminal_observation_file(target.clone(), b"replacement".to_vec())
        .await
        .expect_err("终态文档不得覆盖既有目标");
    assert!(matches!(
        error,
        SystemFileSystemError::Windows(WindowsFsError::RenameTargetExists { .. })
    ));
    assert_eq!(
        fs::read(&target).expect("目标冲突后应该仍可读取原文档"),
        b"complete document"
    );
    assert!(
        observation_temporary_files(directory.path()).is_empty(),
        "目标冲突后的临时文件必须清理"
    );

    file_system.shutdown().await.expect("文件系统根应该可终结");
}

#[tokio::test]
async fn terminal_observation_is_admitted_after_business_cancellation() {
    let directory = tempfile::tempdir().expect("应该可建立测试目录");
    let target = directory.path().join("task-000001.md");
    let file_system =
        SystemFileSystem::new_with_worker_threads(1, Arc::new(RunPerformanceCounters::default()))
            .expect("应该可建立文件系统根");

    file_system.cancel_waits();
    file_system
        .write_new_terminal_observation_file(target.clone(), b"cancelled outcome".to_vec())
        .await
        .expect("业务取消后仍应接收已启动任务的终态文档");
    assert_eq!(
        fs::read(target).expect("应该可读取取消终态文档"),
        b"cancelled outcome"
    );

    file_system.shutdown().await.expect("文件系统根应该可终结");
}

#[tokio::test]
async fn terminal_observation_partial_write_never_exposes_a_final_file() {
    let directory = tempfile::tempdir().expect("应该可建立测试目录");
    let target = directory.path().join("task-000001.md");
    register_test_observation_faults(
        target.clone(),
        [TestObservationFaultPoint::AfterPartialWrite],
    );
    let file_system =
        SystemFileSystem::new_with_worker_threads(1, Arc::new(RunPerformanceCounters::default()))
            .expect("应该可建立文件系统根");

    let error = file_system
        .write_new_terminal_observation_file(target.clone(), b"complete document".to_vec())
        .await
        .expect_err("部分写入故障必须可见");
    assert!(matches!(error, SystemFileSystemError::Io { .. }));
    assert!(!target.exists(), "部分内容不得出现在最终路径");
    assert!(
        observation_temporary_files(directory.path()).is_empty(),
        "部分写入故障后的临时文件必须清理"
    );

    file_system.shutdown().await.expect("文件系统根应该可终结");
}

#[tokio::test]
async fn terminal_observation_rename_failure_removes_the_temporary_file() {
    let directory = tempfile::tempdir().expect("应该可建立测试目录");
    let target = directory.path().join("task-000001.md");
    register_test_observation_faults(target.clone(), [TestObservationFaultPoint::BeforeRename]);
    let file_system =
        SystemFileSystem::new_with_worker_threads(1, Arc::new(RunPerformanceCounters::default()))
            .expect("应该可建立文件系统根");

    let error = file_system
        .write_new_terminal_observation_file(target.clone(), b"complete document".to_vec())
        .await
        .expect_err("重命名故障必须可见");
    assert!(
        error.to_string().contains("测试注入的重命名故障"),
        "必须保留重命名主错误"
    );
    assert!(!target.exists(), "提交失败不得出现最终文件");
    assert!(
        observation_temporary_files(directory.path()).is_empty(),
        "重命名失败后的临时文件必须清理"
    );

    file_system.shutdown().await.expect("文件系统根应该可终结");
}

#[test]
fn terminal_observation_unknown_rename_outcome_preserves_the_temporary_path() {
    let directory = tempfile::tempdir().expect("应该可建立测试目录");
    let target = directory.path().join("task-000001.md");
    let temporary = directory.path().join(".task-000001.md.tmp");
    fs::write(&temporary, "another writer").expect("应该可建立待保护的临时路径");

    let error = finish_terminal_observation_rename(
        &target,
        &temporary,
        Err(WindowsFsError::RenameTargetUnconfirmed {
            path: target.clone(),
        }),
    )
    .expect_err("重命名结果无法确认时必须保留现场");

    assert!(matches!(
        error,
        SystemFileSystemError::OutcomeUnknown {
            target_root,
            artifacts,
            violation: FileSystemRecoveryViolation::TargetIdentityUnknown,
        } if target_root == target && artifacts == [temporary.clone()]
    ));
    assert_eq!(
        fs::read_to_string(&temporary).expect("结果未知时不得清理临时路径"),
        "another writer"
    );
}

#[test]
fn terminal_observation_identity_failure_deletes_the_exact_open_candidate() {
    let directory = tempfile::tempdir().expect("应该可建立测试目录");
    let temporary = directory.path().join(".task-000001.md.tmp");
    let file = create_new_atomic_replace_candidate(&temporary).expect("应该可独占建立终态记录候选");
    let identity_error = WindowsFsError::Io {
        operation: "测试取得终态记录候选身份",
        path: temporary.clone(),
        source: io::Error::other("forced identity failure"),
    };

    let error = terminal_observation_candidate_identity(&file, &temporary, Err(identity_error))
        .expect_err("取得 file ID 失败时必须清理精确候选");

    assert!(matches!(error, SystemFileSystemError::Windows(_)));
    drop(file);
    assert!(!temporary.exists(), "失败后不得遗留未确认身份的候选");
}

#[tokio::test]
async fn terminal_observation_preserves_primary_and_cleanup_failures() {
    let directory = tempfile::tempdir().expect("应该可建立测试目录");
    let target = directory.path().join("task-000001.md");
    register_test_observation_faults(
        target.clone(),
        [
            TestObservationFaultPoint::BeforeRename,
            TestObservationFaultPoint::BeforeCleanup,
        ],
    );
    let file_system =
        SystemFileSystem::new_with_worker_threads(1, Arc::new(RunPerformanceCounters::default()))
            .expect("应该可建立文件系统根");

    let error = file_system
        .write_new_terminal_observation_file(target.clone(), b"complete document".to_vec())
        .await
        .expect_err("提交与清理的组合故障必须可见");
    let SystemFileSystemError::ObservationCleanupFailed {
        temporary_path,
        operation,
        cleanup,
    } = error
    else {
        panic!("应该同时保留主错误与清理错误");
    };
    assert!(
        operation.to_string().contains("测试注入的重命名故障"),
        "主错误必须保留"
    );
    assert!(
        cleanup.to_string().contains("测试注入的临时文件清理故障"),
        "清理错误必须保留"
    );
    assert!(!target.exists(), "提交失败不得出现最终文件");
    assert!(temporary_path.exists(), "清理失败的临时产物必须可定位");
    fs::remove_file(temporary_path).expect("测试应该可清理注入故障留下的临时文件");

    file_system.shutdown().await.expect("文件系统根应该可终结");
}

fn observation_temporary_files(directory: &Path) -> Vec<PathBuf> {
    fs::read_dir(directory)
        .expect("应该可列举测试目录")
        .map(|entry| entry.expect("测试目录项应该可读取").path())
        .filter(|path| {
            path.file_name()
                .and_then(OsStr::to_str)
                .is_some_and(|name| name.starts_with(".task-") && name.ends_with(".tmp"))
        })
        .collect()
}
