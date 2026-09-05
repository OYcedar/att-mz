use super::super::SystemFileSystem;
use super::super::error::SystemFileSystemError;
use crate::runtime::performance::RunPerformanceCounters;
use crate::runtime::windows::WindowsFsError;
use crate::storage::file_system::{
    ExclusiveFileLeaseError, ExclusiveFileLeaseProvider, ExclusiveFileLeaseRequest,
};
use std::ffi::OsString;
use std::fs;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn exclusive_file_lease_serializes_same_windows_identity_and_allows_other_identities() {
    let temporary = tempfile::tempdir().expect("应该可创建临时项目集合根");
    let owner =
        SystemFileSystem::new_with_worker_threads(2, Arc::new(RunPerformanceCounters::default()))
            .expect("应该可建立锁所有者根");
    let contender =
        SystemFileSystem::new_with_worker_threads(2, Arc::new(RunPerformanceCounters::default()))
            .expect("应该可建立锁竞争者根");
    let lock_directory = temporary.path().join("locks/leases");
    let request = |name: &str| {
        ExclusiveFileLeaseRequest::new(lock_directory.clone(), OsString::from(name))
            .expect("测试文件租约请求应该合法")
    };

    let lease = owner
        .acquire_exclusive_file_lease(request("游戏One"))
        .await
        .expect("首个同项目命令应该取得租约");
    let other_lease = contender
        .acquire_exclusive_file_lease(request("游戏Two"))
        .await
        .expect("不同身份应该可以并行");
    let waiting = tokio::spawn({
        let contender = contender.clone();
        let request = request("游戏one");
        async move { contender.acquire_exclusive_file_lease(request).await }
    });
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(
        !waiting.is_finished(),
        "同身份竞争者应自然等待而不是容量拒绝"
    );
    assert!(lock_directory.is_dir());
    assert_eq!(
        fs::read_dir(&lock_directory)
            .expect("应该可列举锁目录")
            .count(),
        2,
        "两个 Windows 身份应该各自只建立一个摘要锁文件"
    );

    drop(other_lease);
    drop(lease);
    let reacquired = tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .expect("所有者释放后等待者应取得租约")
        .expect("等待任务不应 panic")
        .expect("等待者应取得同身份租约");
    drop(reacquired);
    contender.shutdown().await.expect("竞争者根应该可终结");
    owner.shutdown().await.expect("所有者根应该可终结");
}

#[tokio::test]
async fn shutdown_cancels_a_file_lease_wait_without_an_arbitrary_deadline() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let request = |identity: &str| {
        ExclusiveFileLeaseRequest::new(temporary.path().join("locks"), OsString::from(identity))
            .expect("测试租约请求应合法")
    };
    let owner =
        SystemFileSystem::new_with_worker_threads(2, Arc::new(RunPerformanceCounters::default()))
            .expect("所有者根应建立");
    let contender =
        SystemFileSystem::new_with_worker_threads(2, Arc::new(RunPerformanceCounters::default()))
            .expect("竞争根应建立");
    let lease = owner
        .acquire_exclusive_file_lease(request("Game"))
        .await
        .expect("所有者应取得租约");
    let waiting = tokio::spawn({
        let contender = contender.clone();
        let request = request("game");
        async move { contender.acquire_exclusive_file_lease(request).await }
    });
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(!waiting.is_finished(), "竞争者应等待真实文件锁");

    contender.shutdown().await.expect("shutdown 应中断锁等待");
    let error = match waiting.await.expect("等待任务不应 panic") {
        Ok(_) => panic!("被 shutdown 取消后不得伪造取得租约"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        ExclusiveFileLeaseError::Unavailable { source, .. }
            if matches!(*source, SystemFileSystemError::Windows(
                WindowsFsError::LockCancelled { .. }
            ))
    ));
    drop(lease);
    owner.shutdown().await.expect("所有者根应终结");
}

#[tokio::test]
async fn cancel_waits_interrupts_a_file_lease_without_closing_the_worker_pool() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let request = |identity: &str| {
        ExclusiveFileLeaseRequest::new(temporary.path().join("locks"), OsString::from(identity))
            .expect("测试租约请求应合法")
    };
    let owner =
        SystemFileSystem::new_with_worker_threads(2, Arc::new(RunPerformanceCounters::default()))
            .expect("所有者根应建立");
    let contender =
        SystemFileSystem::new_with_worker_threads(2, Arc::new(RunPerformanceCounters::default()))
            .expect("竞争根应建立");
    let lease = owner
        .acquire_exclusive_file_lease(request("Game"))
        .await
        .expect("所有者应取得租约");
    let waiting = tokio::spawn({
        let contender = contender.clone();
        let request = request("game");
        async move { contender.acquire_exclusive_file_lease(request).await }
    });
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(!waiting.is_finished(), "竞争者应等待真实文件锁");

    contender.cancel_waits();
    let error = match tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .expect("取消应及时唤醒文件租约等待")
        .expect("等待任务不应 panic")
    {
        Ok(_) => panic!("取消等待后不得伪造取得租约"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        ExclusiveFileLeaseError::Unavailable { source, .. }
            if matches!(*source, SystemFileSystemError::Windows(
                WindowsFsError::LockCancelled { .. }
            ))
    ));

    drop(lease);
    contender.shutdown().await.expect("竞争者根应可终结");
    owner.shutdown().await.expect("所有者根应终结");
}
