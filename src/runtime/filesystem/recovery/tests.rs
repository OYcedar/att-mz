use super::super::error::SystemFileSystemError;
use super::super::test_faults::{
    TestPublishFaultAction, TestPublishFaultPoint, register_test_publish_faults,
};
use super::super::test_support::*;
use super::super::workspace::{
    PUBLICATION_BACKUP_NAME, PUBLICATION_JOURNAL_NAME, PUBLICATION_STAGE_NAME,
    publication_workspace_root,
};
use super::*;
use crate::diagnostic::StateEffect;
use crate::storage::file_system::{DirectoryPublishIntent, RecoverableDirectoryPublisher};
use std::ffi::OsStr;
use std::fs;

#[test]
fn corrupt_recovery_journal_reports_the_complete_observed_inventory() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let target = temporary.path().join("target");
    let workspace = publication_workspace_root(temporary.path(), OsStr::new("target"));
    fs::create_dir_all(&workspace).expect("应该可创建目标恢复目录");
    let journal = workspace.join(PUBLICATION_JOURNAL_NAME);
    let stage = workspace.join(PUBLICATION_STAGE_NAME);
    let backup = workspace.join(PUBLICATION_BACKUP_NAME);
    fs::create_dir(&stage).expect("应该可创建候选恢复产物");
    fs::create_dir(&backup).expect("应该可创建备份恢复产物");
    let payload = b"{}";
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(payload);
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    fs::write(&journal, bytes).expect("应该可写入损坏 journal");
    let mut observed = vec![stage, backup, journal.clone()];
    observed.sort();

    let error =
        recover_journal(&target, &journal, &observed).expect_err("损坏 journal 必须拒绝自动恢复");
    let SystemFileSystemError::RecoveryJournalCorrupt { artifacts, .. } = &error else {
        panic!("恢复入口必须保留完整受管现场，实际为 {error:?}")
    };
    assert_eq!(artifacts, &observed);

    let report = init_recovery_report(&error);
    assert_eq!(report.effect(), StateEffect::RecoveryRequired);
    let wire = serde_json::to_value(report).expect("恢复诊断必须可序列化");
    assert_eq!(
        wire.pointer("/primary/issue/details/problem/artifacts"),
        Some(&serde_json::json!(
            observed
                .iter()
                .map(|path| path.to_string_lossy().to_string())
                .collect::<Vec<_>>()
        ))
    );
}

#[tokio::test]
async fn process_abort_states_are_recovered_idempotently() {
    for (phase, expected) in [
        ("original-journal", b"old".as_slice()),
        ("original-move", b"old".as_slice()),
        ("candidate-intent", b"old".as_slice()),
        ("candidate-move", b"new".as_slice()),
        ("candidate-visible", b"new".as_slice()),
    ] {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let source = temporary.path().join("source");
        fs::create_dir(&source).expect("应该可创建来源");
        fs::write(source.join("value.txt"), b"new").expect("应该可写入新内容");
        let target = temporary.path().join("target");
        fs::create_dir_all(target.join("snapshot/content")).expect("应该可创建旧目标");
        fs::write(target.join("snapshot/content/value.txt"), b"old").expect("应该可写入旧内容");
        let status = subprocess_command(&format!("abort:{phase}"), &target, &source)
            .status()
            .expect("应该可等待故障子进程");
        assert!(!status.success(), "故障点 {phase} 必须终止子进程");

        let root = TestDirectoryPublisher::new();
        if phase == "original-move" {
            register_test_publish_faults(
                canonical_target(&target),
                [(
                    TestPublishFaultPoint::BeforeRecoveryCleanup,
                    TestPublishFaultAction::Error,
                )],
            );
            let recovery = root
                .recover(target.clone())
                .await
                .expect_err("旧目标恢复后的清理故障必须保留恢复现场");
            assert!(matches!(
                recovery.source_error().as_ref(),
                SystemFileSystemError::RecoveryCleanupFailed { .. }
            ));
            let report = init_recovery_report(recovery.source_error());
            assert_eq!(report.effect(), StateEffect::RecoveryRequired);
            let wire = serde_json::to_value(report).expect("恢复失败必须可序列化");
            assert!(
                wire.pointer("/primary/issue/details/problem/artifacts")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|artifacts| artifacts.len() >= 2)
            );
        }
        for _ in 0..2 {
            let staged = root
                .prepare(stage_request(
                    target.clone(),
                    source.clone(),
                    DirectoryPublishIntent::ReplaceExisting,
                ))
                .await
                .expect("恢复应幂等且允许继续准备");
            root.discard(staged).await.expect("恢复后候选应可丢弃");
        }
        assert_eq!(
            fs::read(target.join("snapshot/content/value.txt")).expect("应该可读取恢复目标"),
            expected,
            "故障点 {phase} 恢复了错误一侧"
        );
        root.shutdown().await.expect("恢复根应可终结");
    }
}

#[tokio::test]
async fn residual_identity_changes_do_not_override_a_known_target_state() {
    for (phase, extension, expected_effect) in [
        ("original-move", "stage", StateEffect::RecoveryRequired),
        (
            "candidate-visible",
            "backup",
            StateEffect::AppliedFinalizationFailed,
        ),
    ] {
        let temporary = tempfile::tempdir().expect("应该可创建临时目录");
        let source = temporary.path().join("source");
        fs::create_dir(&source).expect("应该可创建来源");
        fs::write(source.join("value.txt"), b"new").expect("应该可写入新内容");
        let target = temporary.path().join("target");
        fs::create_dir_all(target.join("snapshot/content")).expect("应该可创建旧目标");
        fs::write(target.join("snapshot/content/value.txt"), b"old").expect("应该可写入旧内容");
        let status = subprocess_command(&format!("abort:{phase}"), &target, &source)
            .status()
            .expect("应该可等待故障子进程");
        assert!(!status.success(), "故障点 {phase} 必须终止子进程");

        let changed = single_managed_artifact(&target, extension);
        fs::remove_dir_all(&changed).expect("应该可移除原受管目录");
        fs::create_dir(&changed).expect("应该可建立不同身份的占位目录");

        let root = TestDirectoryPublisher::new();
        let recovery = root
            .recover(target.clone())
            .await
            .expect_err("身份异常必须保留恢复现场");
        let report = init_recovery_report(recovery.source_error());
        assert_eq!(report.effect(), expected_effect);
        match (phase, recovery.source_error().as_ref()) {
            ("original-move", SystemFileSystemError::RecoveryCleanupFailed { .. }) => {}
            ("candidate-visible", SystemFileSystemError::PublishedRecoveryCleanupFailed { .. }) => {
            }
            (_, actual) => panic!("目标终态分类错误：{actual:?}"),
        }
        let expected_contents = if phase == "original-move" {
            b"old".as_slice()
        } else {
            b"new".as_slice()
        };
        assert_eq!(
            fs::read(target.join("snapshot/content/value.txt")).expect("目标内容应可读取"),
            expected_contents
        );
        root.shutdown().await.expect("文件系统根应可终结");
    }
}

fn nested_unknown_cleanup_source(target_root: &Path, artifact: &Path) -> SystemFileSystemError {
    SystemFileSystemError::OutcomeUnknown {
        target_root: target_root.to_path_buf(),
        artifacts: vec![artifact.to_path_buf()],
        violation: FileSystemRecoveryViolation::TargetIdentityUnknown,
    }
}

#[test]
fn nested_outcome_unknown_promotes_terminal_effect() {
    let target = PathBuf::from("C:/projects/game");
    let artifact = PathBuf::from("C:/projects/.directory-publish/test/stage");

    for (error, expected) in [
        (
            recovery_cleanup_failed(
                &target,
                vec![artifact.clone()],
                nested_unknown_cleanup_source(&target, &artifact),
            ),
            StateEffect::OutcomeUnknown,
        ),
        (
            published_recovery_cleanup_failed(
                &target,
                vec![artifact.clone()],
                nested_unknown_cleanup_source(&target, &artifact),
            ),
            StateEffect::OutcomeUnknown,
        ),
    ] {
        let report = init_recovery_report(&error);
        assert_eq!(report.effect(), expected);
        assert_eq!(
            report.primary().resolution(),
            crate::diagnostic::DiagnosticResolution::PreserveRecoveryArtifacts
        );
        assert_eq!(report.related().len(), 1, "清理根因必须作为相关失败保留");
    }
}
