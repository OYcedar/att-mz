use super::super::error::SystemFileSystemError;
use super::*;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

#[test]
fn joining_workers_waits_for_all_workers_after_one_panics() {
    let completed = Arc::new(AtomicBool::new(false));
    let panicked = thread::spawn(|| panic!("测试 worker panic"));
    let remaining_completed = Arc::clone(&completed);
    let remaining = thread::spawn(move || {
        thread::sleep(Duration::from_millis(25));
        remaining_completed.store(true, Ordering::Release);
    });

    let clean = join_file_workers(vec![panicked, remaining]);

    assert!(!clean);
    assert!(
        completed.load(Ordering::Acquire),
        "首个 worker panic 后仍必须等待其余 worker 退出"
    );
}

#[tokio::test]
async fn saturated_file_pool_cancels_admission_without_queueing_extra_work() {
    let pool = Arc::new(FileWorkPool::new(1).expect("单 worker 文件池应可建立"));
    let (started_sender, started_receiver) = async_channel::bounded(1);
    let (release_sender, release_receiver) = async_channel::bounded(1);
    let active_pool = Arc::clone(&pool);
    let active = tokio::spawn(async move {
        active_pool
            .execute("active_test_work", Path::new("C:/active"), move || {
                started_sender
                    .send_blocking(())
                    .expect("测试应能通知 active 工作已开始");
                release_receiver
                    .recv_blocking()
                    .expect("测试应能释放 active 工作");
                7_u8
            })
            .await
    });
    started_receiver
        .recv()
        .await
        .expect("active 工作必须占用唯一执行许可");

    let waiting_pool = Arc::clone(&pool);
    let waiting = tokio::spawn(async move {
        waiting_pool
            .execute("waiting_test_work", Path::new("C:/waiting"), || 9_u8)
            .await
    });
    tokio::task::yield_now().await;
    assert!(!waiting.is_finished(), "饱和时必须等待实际执行许可");

    pool.cancel_waits();
    let error = tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .expect("取消必须立即唤醒 admission 等待")
        .expect("等待任务不应 panic")
        .expect_err("取消必须唤醒尚未取得许可的任务");
    assert!(matches!(
        error,
        SystemFileSystemError::Cancelled { operation, path }
            if operation == "waiting_test_work" && path == Path::new("C:/waiting")
    ));

    release_sender
        .send(())
        .await
        .expect("已接管的 active 工作必须能继续完成");
    assert_eq!(
        active
            .await
            .expect("active 任务不应 panic")
            .expect("已接管的 active 工作不应被 admission 取消"),
        7
    );
    pool.shutdown().await.expect("文件池应干净关闭");
}
