use super::super::DirectoryPublisherConfig;
use super::super::error::SystemFileSystemError;
use super::super::test_faults::{
    TestPublishFaultAction, TestPublishFaultPoint, register_test_publish_faults,
};
use super::super::test_support::*;
use crate::diagnostic::{FileSystemPathViolation, StateEffect};
use crate::runtime::windows::WindowsFsError;
use crate::storage::file_system::{
    DirectoryPrepareError, DirectoryPublishError, DirectoryPublishIntent, DirectoryRecoveryOutcome,
    RecoverableDirectoryPublisher,
};
use std::ffi::OsStr;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

#[test]
fn publisher_configuration_rejects_empty_lock_directory() {
    assert!(DirectoryPublisherConfig::production(PathBuf::new()).is_err());
    let _ = publisher_config();
}

#[tokio::test]
async fn publisher_lock_lives_in_the_configured_lock_directory() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let lock_directory = temporary.path().join("locks/publish");
    let source = temporary.path().join("source");
    fs::create_dir(&source).expect("应该可创建候选来源");
    let target = temporary.path().join("outputs/target");
    fs::create_dir(target.parent().expect("测试目标应该有父目录")).expect("应该可创建目标父目录");
    let root = TestDirectoryPublisher::with_publisher_config(
        2,
        publisher_config_for_lock_directory(lock_directory.clone()),
    );

    let staged = root
        .prepare(stage_request(
            target.clone(),
            source,
            DirectoryPublishIntent::CreateNew,
        ))
        .await
        .expect("候选应该可准备");

    assert!(lock_directory.is_dir());
    let lock_files = fs::read_dir(&lock_directory)
        .expect("应该可列举目录发布锁")
        .collect::<Result<Vec<_>, _>>()
        .expect("应该可读取目录发布锁项");
    assert_eq!(lock_files.len(), 1);
    assert_eq!(lock_files[0].file_name(), OsStr::new("target"));
    assert!(!target.parent().unwrap().join("locks").exists());

    root.discard(staged).await.expect("应该可丢弃候选");
    root.shutdown().await.expect("文件系统根应该可终结");
}

#[tokio::test]
async fn complete_candidate_validation_counters_are_isolated_between_parallel_roots() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let first_source = temporary.path().join("first-source");
    let second_source = temporary.path().join("second-source");
    fs::create_dir(&first_source).expect("应该可创建第一来源");
    fs::create_dir(&second_source).expect("应该可创建第二来源");
    fs::write(first_source.join("value.txt"), b"first").expect("应该可写入第一来源");
    fs::write(second_source.join("value.txt"), b"second").expect("应该可写入第二来源");

    let first = TestDirectoryPublisher::new();
    let second = TestDirectoryPublisher::new();
    let first_staged = first
        .prepare(stage_request(
            temporary.path().join("first-output"),
            first_source,
            DirectoryPublishIntent::CreateNew,
        ))
        .await
        .expect("第一候选应该可准备");
    let second_staged = second
        .prepare(stage_request(
            temporary.path().join("second-output"),
            second_source,
            DirectoryPublishIntent::CreateNew,
        ))
        .await
        .expect("第二候选应该可准备");
    assert_eq!(first.candidate_validation_counts(), (0, 0));
    assert_eq!(second.candidate_validation_counts(), (0, 0));

    let (first_result, second_result) =
        tokio::join!(first.publish(first_staged), second.publish(second_staged));
    first_result.expect("第一候选应该可发布");
    second_result.expect("第二候选应该可发布");
    assert_eq!(first.candidate_validation_counts(), (1, 1));
    assert_eq!(second.candidate_validation_counts(), (1, 1));

    first.shutdown().await.expect("第一文件系统根应该可终结");
    second.shutdown().await.expect("第二文件系统根应该可终结");
}

#[tokio::test]
async fn create_new_and_replace_existing_publish_complete_directory_snapshots() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source-a");
    fs::create_dir(&source).expect("应该可创建来源目录");
    fs::write(source.join("value.txt"), b"first").expect("应该可写入来源文件");
    let target = temporary.path().join("output");
    let root = TestDirectoryPublisher::new();

    let staged = root
        .prepare(stage_request(
            target.clone(),
            source,
            DirectoryPublishIntent::CreateNew,
        ))
        .await
        .expect("首次候选应该可准备");
    fs::write(staged.staging_root().join("prepared.txt"), b"prepared")
        .expect("非根服务应该可在候选内建立文件");
    let database = rusqlite::Connection::open(staged.staging_root().join("state.db"))
        .expect("应该可在候选内建立真实 SQLite 数据库");
    database
        .execute_batch(
            "CREATE TABLE metadata (value TEXT NOT NULL); INSERT INTO metadata VALUES ('ok');",
        )
        .expect("应该可初始化候选数据库");
    drop(database);
    root.publish(staged).await.expect("首次候选应该可发布");
    assert_eq!(
        fs::read(target.join("snapshot/content/value.txt")).unwrap(),
        b"first"
    );
    assert_eq!(fs::read(target.join("prepared.txt")).unwrap(), b"prepared");
    assert!(target.join("empty").is_dir());

    let replacement = temporary.path().join("source-b");
    fs::create_dir(&replacement).expect("应该可创建替换来源");
    fs::write(replacement.join("value.txt"), b"second").expect("应该可写入替换来源");
    let staged = root
        .prepare(stage_request(
            target.clone(),
            replacement,
            DirectoryPublishIntent::ReplaceExisting,
        ))
        .await
        .expect("替换候选应该可准备");
    root.publish(staged).await.expect("替换候选应该可发布");
    assert_eq!(
        fs::read(target.join("snapshot/content/value.txt")).unwrap(),
        b"second"
    );
    assert!(!target.join("prepared.txt").exists());

    root.shutdown().await.expect("文件系统根应该可终结");
}

async fn assert_publish_revalidates_added_file(contents: Vec<u8>) {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source");
    fs::create_dir(&source).expect("应该可创建来源目录");
    fs::write(source.join("value.txt"), b"small").expect("应该可写入来源文件");
    let target = temporary.path().join("target");
    let root = TestDirectoryPublisher::new();
    let staged = root
        .prepare(stage_request(
            target.clone(),
            source,
            DirectoryPublishIntent::CreateNew,
        ))
        .await
        .expect("候选应可准备");
    let staging_root = staged.staging_root().to_path_buf();
    let expected_len = u64::try_from(contents.len()).expect("测试文件长度应可表示为 u64");
    fs::write(staging_root.join("added.db"), contents).expect("非根服务应可在候选中新增文件");

    root.publish(staged)
        .await
        .expect("完整复核应接受新增的普通文件");
    assert_eq!(
        fs::metadata(target.join("added.db"))
            .expect("新增文件应随候选发布")
            .len(),
        expected_len
    );
    assert!(!staging_root.exists(), "发布后候选路径应被目录交换移走");
    root.shutdown().await.expect("文件系统根应可终结");
}

#[tokio::test]
async fn publish_revalidates_files_added_after_prepare() {
    assert_publish_revalidates_added_file(b"added after prepare".to_vec()).await;
}

#[cfg(feature = "release-stress")]
#[tokio::test]
async fn release_stress_publish_revalidates_and_accepts_large_files_added_after_prepare() {
    assert_publish_revalidates_added_file(vec![0_u8; 512 * 1024 + 1]).await;
}

#[tokio::test]
async fn publish_revalidation_rejects_hardlinks_added_after_prepare() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source");
    fs::create_dir(&source).expect("应该可创建来源目录");
    fs::write(source.join("value.txt"), b"value").expect("应该可写入来源文件");
    let target = temporary.path().join("target");
    let root = TestDirectoryPublisher::new();
    let staged = root
        .prepare(stage_request(
            target.clone(),
            source,
            DirectoryPublishIntent::CreateNew,
        ))
        .await
        .expect("候选应可准备");
    let staging_root = staged.staging_root().to_path_buf();
    fs::hard_link(
        staging_root.join("snapshot/content/value.txt"),
        staging_root.join("same-file.txt"),
    )
    .expect("测试卷应支持硬链接");

    assert!(matches!(
        root.publish(staged).await,
        Err(DirectoryPublishError::NotAttempted { source, .. })
            if matches!(*source, SystemFileSystemError::InvalidPath {
                violation: FileSystemPathViolation::HardLink,
                ..
            })
    ));
    assert_eq!(
        root.candidate_validation_counts(),
        (1, 0),
        "失败的完整候选校验只能记录开始，不能记录完成"
    );
    assert!(!target.exists());
    assert!(!staging_root.exists());
    root.shutdown().await.expect("文件系统根应可终结");
}

#[tokio::test]
async fn concurrent_create_new_has_exactly_one_winner() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let first_source = temporary.path().join("first");
    let second_source = temporary.path().join("second");
    fs::create_dir(&first_source).expect("应该可创建第一来源");
    fs::create_dir(&second_source).expect("应该可创建第二来源");
    fs::write(first_source.join("winner.txt"), b"first").unwrap();
    fs::write(second_source.join("winner.txt"), b"second").unwrap();
    let target = temporary.path().join("destination");
    let root = TestDirectoryPublisher::new();

    let publish_one = |source: PathBuf| {
        let root = root.clone();
        let target = target.clone();
        async move {
            let staged = root
                .prepare(stage_request(
                    target,
                    source,
                    DirectoryPublishIntent::CreateNew,
                ))
                .await?;
            Ok::<_, DirectoryPrepareError<Box<SystemFileSystemError>>>(root.publish(staged).await)
        }
    };
    let (first, second) = tokio::join!(publish_one(first_source), publish_one(second_source));
    let outcomes = [first, second];
    let successes = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, Ok(Ok(()))))
        .count();
    let already_exists = outcomes
        .iter()
        .filter(|outcome| {
            matches!(
                outcome,
                Ok(Err(DirectoryPublishError::TargetAlreadyExists { .. }))
            )
        })
        .count();
    assert_eq!(successes, 1);
    assert_eq!(already_exists, 1);
    root.shutdown().await.expect("文件系统根应该可终结");
}

#[tokio::test]
async fn a_staged_token_cannot_be_finalized_by_another_root_instance() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source");
    fs::create_dir(&source).expect("应该可创建来源目录");
    let target = temporary.path().join("target");
    let owner = TestDirectoryPublisher::new();
    let foreign = TestDirectoryPublisher::new();
    let staged = owner
        .prepare(stage_request(
            target,
            source,
            DirectoryPublishIntent::CreateNew,
        ))
        .await
        .expect("候选应该可准备");
    let staging_root = staged.staging_root().to_path_buf();

    assert!(matches!(
        foreign.publish(staged).await,
        Err(DirectoryPublishError::NotAttempted { source, .. })
            if matches!(*source, SystemFileSystemError::WrongPublisherInstance)
    ));
    assert!(!staging_root.exists());
    owner.shutdown().await.expect("所有者根应该可终结");
    foreign.shutdown().await.expect("外来根应该可终结");
}

#[tokio::test]
async fn directly_dropping_a_staged_token_panics_and_still_cleans_the_candidate() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source");
    fs::create_dir(&source).expect("应该可创建来源目录");
    fs::write(source.join("value.txt"), b"value").expect("应该可创建来源文件");
    let target = temporary.path().join("target");
    let root = TestDirectoryPublisher::new();
    let staged = root
        .prepare(stage_request(
            target,
            source,
            DirectoryPublishIntent::CreateNew,
        ))
        .await
        .expect("候选应该可准备");
    let staging_root = staged.staging_root().to_path_buf();

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(staged)));

    assert!(panic.is_err(), "直接丢弃 token 必须显式违反内部契约");
    assert!(!staging_root.exists(), "panic 展开时仍必须精确清理候选");
    root.shutdown().await.expect("文件系统根应该可终结");
}

#[tokio::test]
async fn target_lock_contention_waits_until_the_owner_releases() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source");
    fs::create_dir(&source).expect("应该可创建来源目录");
    let target = temporary.path().join("target");
    let lock_directory = temporary.path().join("locks");
    let owner = TestDirectoryPublisher::with_publisher_config(
        2,
        publisher_config_for_lock_directory(lock_directory.clone()),
    );
    let staged = owner
        .prepare(stage_request(
            target.clone(),
            source.clone(),
            DirectoryPublishIntent::CreateNew,
        ))
        .await
        .expect("所有者应可持有目标锁");
    let contender = TestDirectoryPublisher::with_publisher_config(
        1,
        publisher_config_for_lock_directory(lock_directory),
    );
    let waiting = tokio::spawn({
        let contender = contender.clone();
        async move {
            contender
                .prepare(stage_request(
                    target,
                    source,
                    DirectoryPublishIntent::CreateNew,
                ))
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(!waiting.is_finished(), "目标锁竞争应自然等待而不是超时拒绝");
    owner.discard(staged).await.expect("应该可丢弃所有者候选");
    let contender_stage = tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .expect("所有者释放后竞争者应继续")
        .expect("竞争任务不应 panic")
        .expect("竞争者应准备候选");
    contender
        .discard(contender_stage)
        .await
        .expect("应丢弃竞争者候选");
    owner.shutdown().await.expect("所有者根应可终结");
    contender.shutdown().await.expect("竞争根应可终结");
}

#[tokio::test]
async fn cancel_waits_interrupts_a_directory_publish_lock_before_shutdown() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source");
    fs::create_dir(&source).expect("应该可创建来源目录");
    let target = temporary.path().join("target");
    let lock_directory = temporary.path().join("locks");
    let owner = TestDirectoryPublisher::with_publisher_config(
        2,
        publisher_config_for_lock_directory(lock_directory.clone()),
    );
    let staged = owner
        .prepare(stage_request(
            target.clone(),
            source.clone(),
            DirectoryPublishIntent::CreateNew,
        ))
        .await
        .expect("所有者应可持有目标锁");
    let contender = TestDirectoryPublisher::with_publisher_config(
        1,
        publisher_config_for_lock_directory(lock_directory),
    );
    let waiting = tokio::spawn({
        let contender = contender.clone();
        async move {
            contender
                .prepare(stage_request(
                    target,
                    source,
                    DirectoryPublishIntent::CreateNew,
                ))
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert!(!waiting.is_finished(), "目标锁竞争应持续等待");

    contender.file_system.cancel_waits();
    let error = match tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .expect("取消应及时唤醒目录发布锁等待")
        .expect("竞争任务不应 panic")
    {
        Ok(_) => panic!("取消等待后不得伪造候选已准备"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        DirectoryPrepareError::NotPrepared { source, .. }
            if matches!(*source, SystemFileSystemError::Windows(
                WindowsFsError::LockCancelled { .. }
            ))
    ));

    owner.discard(staged).await.expect("应该可丢弃所有者候选");
    contender.shutdown().await.expect("竞争根应可终结");
    owner.shutdown().await.expect("所有者根应可终结");
}

#[tokio::test]
async fn cancel_waits_does_not_reject_discard_of_a_prepared_candidate() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source");
    fs::create_dir(&source).expect("应该可创建来源目录");
    fs::write(source.join("value.txt"), b"content").expect("应该可写入来源文件");
    let target = temporary.path().join("target");
    let root = TestDirectoryPublisher::new();

    let staged = root
        .prepare(stage_request(
            target,
            source,
            DirectoryPublishIntent::CreateNew,
        ))
        .await
        .expect("候选应可准备");
    let staging_root = staged.staging_root().to_path_buf();

    // 业务取消后，已交付 token 的 discard 必须仍运行至终态并清理候选，
    // 而不是被取消门拒绝后弃置 token。
    root.file_system.cancel_waits();
    root.discard(staged)
        .await
        .expect("取消后 discard 仍应完成候选清理");
    assert!(
        !staging_root.exists(),
        "取消后 discard 应删除已准备的候选目录"
    );
    root.shutdown().await.expect("根应可终结");
}

#[tokio::test]
async fn cancel_waits_does_not_reject_publish_of_a_prepared_candidate() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source");
    fs::create_dir(&source).expect("应该可创建来源目录");
    fs::write(source.join("value.txt"), b"content").expect("应该可写入来源文件");
    let target = temporary.path().join("target");
    let root = TestDirectoryPublisher::new();

    let staged = root
        .prepare(stage_request(
            target.clone(),
            source,
            DirectoryPublishIntent::CreateNew,
        ))
        .await
        .expect("候选应可准备");

    // publish 与 discard 同理：取消只阻止新工作进入，不阻止已交付 token 的收尾。
    root.file_system.cancel_waits();
    root.publish(staged)
        .await
        .expect("取消后 publish 仍应运行至终态");
    assert_eq!(
        fs::read(target.join("snapshot/content/value.txt")).expect("发布结果应可读取"),
        b"content"
    );
    root.shutdown().await.expect("根应可终结");
}

#[tokio::test]
async fn replace_faults_preserve_precise_terminal_states_and_recover() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let source = temporary.path().join("source");
    fs::create_dir(&source).expect("应该可创建来源目录");
    fs::write(source.join("value.txt"), b"new").expect("应该可写入新内容");
    let target = temporary.path().join("target");
    fs::create_dir_all(target.join("snapshot/content")).expect("应该可创建旧目标");
    fs::write(target.join("snapshot/content/value.txt"), b"old").expect("应该可写入旧内容");
    let root = TestDirectoryPublisher::new();
    let trusted_target = canonical_target(&target);

    let staged = root
        .prepare(stage_request(
            target.clone(),
            source.clone(),
            DirectoryPublishIntent::ReplaceExisting,
        ))
        .await
        .expect("替换候选应可准备");
    register_test_publish_faults(
        trusted_target.clone(),
        [(
            TestPublishFaultPoint::BeforeCandidateMove,
            TestPublishFaultAction::Error,
        )],
    );
    assert!(matches!(
        root.publish(staged).await,
        Err(DirectoryPublishError::NotPublished { .. })
    ));
    assert_eq!(
        fs::read(target.join("snapshot/content/value.txt")).expect("旧目标应已恢复"),
        b"old"
    );

    let staged = root
        .prepare(stage_request(
            target.clone(),
            source.clone(),
            DirectoryPublishIntent::ReplaceExisting,
        ))
        .await
        .expect("第二个替换候选应可准备");
    register_test_publish_faults(
        trusted_target,
        [
            (
                TestPublishFaultPoint::BeforeBackupCleanup,
                TestPublishFaultAction::Error,
            ),
            (
                TestPublishFaultPoint::BeforeRecoveryCleanup,
                TestPublishFaultAction::Error,
            ),
        ],
    );
    assert!(matches!(
        root.publish(staged).await,
        Err(DirectoryPublishError::PublishedWithResiduals { .. })
    ));
    assert_eq!(
        fs::read(target.join("snapshot/content/value.txt")).expect("新目标应已可见"),
        b"new"
    );

    let recovery = root
        .recover(target.clone())
        .await
        .expect_err("已发布目标的恢复清理故障必须保留精确终态");
    let published_artifacts = match recovery.source_error().as_ref() {
        SystemFileSystemError::PublishedRecoveryCleanupFailed { artifacts, .. } => {
            assert!(artifacts.iter().all(|path| path.exists()));
            assert!(artifacts.windows(2).all(|paths| paths[0] < paths[1]));
            artifacts.clone()
        }
        source => panic!("恢复必须保留已确认发布终态，实际为 {source:?}"),
    };
    let report = init_recovery_report(recovery.source_error());
    assert_eq!(report.effect(), StateEffect::AppliedFinalizationFailed);
    let wire = serde_json::to_value(report).expect("已发布清理失败必须可序列化");
    assert_eq!(
        wire.pointer("/primary/issue/details/problem/artifacts")
            .and_then(serde_json::Value::as_array)
            .map(Vec::len),
        Some(published_artifacts.len())
    );

    assert_eq!(
        root.recover(target.clone())
            .await
            .expect("故障消失后显式恢复应清理已发布目标的残留"),
        DirectoryRecoveryOutcome::Recovered
    );
    assert_eq!(
        root.recover(target.clone())
            .await
            .expect("没有受管产物时恢复应明确返回未改变"),
        DirectoryRecoveryOutcome::Unchanged
    );

    let staged = root
        .prepare(stage_request(
            target.clone(),
            source,
            DirectoryPublishIntent::ReplaceExisting,
        ))
        .await
        .expect("下次操作应先完成恢复");
    root.discard(staged).await.expect("恢复后候选应可丢弃");
    assert_eq!(
        fs::read(target.join("snapshot/content/value.txt")).expect("恢复不应回退已发布目标"),
        b"new"
    );
    root.shutdown().await.expect("文件系统根应可终结");
}

#[test]
fn publisher_subprocess_entrypoint() {
    let Some(mode) = std::env::var_os("FILESYSTEM_PUBLISHER_CHILD_MODE") else {
        return;
    };
    let target = PathBuf::from(
        std::env::var_os("FILESYSTEM_PUBLISHER_CHILD_TARGET").expect("子进程应提供目标路径"),
    );
    let source = PathBuf::from(
        std::env::var_os("FILESYSTEM_PUBLISHER_CHILD_SOURCE").expect("子进程应提供来源路径"),
    );
    let mode = mode.to_string_lossy();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("应该可建立子进程运行时");
    runtime.block_on(async move {
        let root = TestDirectoryPublisher::new();
        let intent = if mode == "create" {
            DirectoryPublishIntent::CreateNew
        } else {
            DirectoryPublishIntent::ReplaceExisting
        };
        let staged = root
            .prepare(stage_request(target.clone(), source, intent))
            .await
            .expect("子进程应可准备候选");
        if let Some(point) = mode.strip_prefix("abort:") {
            let point = match point {
                "original-journal" => TestPublishFaultPoint::AfterOriginalJournal,
                "original-move" => TestPublishFaultPoint::AfterOriginalMove,
                "candidate-intent" => TestPublishFaultPoint::AfterCandidateIntent,
                "candidate-move" => TestPublishFaultPoint::AfterCandidateMove,
                "candidate-visible" => TestPublishFaultPoint::AfterCandidateVisible,
                _ => panic!("未知子进程故障点：{point}"),
            };
            register_test_publish_faults(
                canonical_target(&target),
                [(point, TestPublishFaultAction::Abort)],
            );
        }
        let result = root.publish(staged).await;
        if mode == "create" {
            let result_path = PathBuf::from(
                std::env::var_os("FILESYSTEM_PUBLISHER_CHILD_RESULT")
                    .expect("新建子进程应提供结果路径"),
            );
            let outcome = match result {
                Ok(()) => "success",
                Err(DirectoryPublishError::TargetAlreadyExists { .. }) => "already-exists",
                Err(error) => panic!("子进程发布结果不可归类：{error}"),
            };
            fs::write(result_path, outcome).expect("应该可写入子进程结果");
            root.shutdown().await.expect("子进程根应可终结");
        } else {
            panic!("故障子进程应在 publish 内直接 abort：{result:?}");
        }
    });
}

#[test]
fn two_processes_create_new_with_exactly_one_winner() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let target = temporary.path().join("target");
    let mut children = Vec::new();
    let mut results = Vec::new();
    for index in 0..2 {
        let source = temporary.path().join(format!("source-{index}"));
        fs::create_dir(&source).expect("应该可创建子进程来源");
        fs::write(source.join("value.txt"), index.to_string()).expect("应该可写入来源");
        let result = temporary.path().join(format!("result-{index}"));
        let mut command = subprocess_command("create", &target, &source);
        command
            .env("FILESYSTEM_PUBLISHER_CHILD_RESULT", &result)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        children.push(command.spawn().expect("应该可启动发布子进程"));
        results.push(result);
    }
    for child in children {
        let output = child.wait_with_output().expect("应该可等待发布子进程");
        assert!(
            output.status.success(),
            "发布子进程异常退出；stdout：{}；stderr：{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let outcomes = results
        .iter()
        .map(|path| fs::read_to_string(path).expect("应该可读取子进程结果"))
        .collect::<Vec<_>>();
    assert_eq!(
        outcomes.iter().filter(|value| *value == "success").count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|value| *value == "already-exists")
            .count(),
        1
    );
}
