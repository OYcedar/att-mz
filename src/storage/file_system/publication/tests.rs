use super::*;

#[test]
fn displayed_path_lists_remove_windows_verbatim_prefixes() {
    assert_eq!(
        display_paths(&[
            PathBuf::from(r"\\?\C:\games\sample"),
            PathBuf::from(r"\\?\UNC\server\share\sample"),
        ]),
        r"C:\games\sample、\\server\share\sample"
    );
}

#[derive(Debug)]
struct TestError(&'static str);

impl fmt::Display for TestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for TestError {}

impl DirectoryPublicationDiagnosticSource for TestError {
    fn publication_diagnostic(&self, step: PublicationStep) -> DirectoryPublicationDiagnostic {
        let operation = match step {
            PublicationStep::Recover => crate::diagnostic::FileSystemOperation::RecoverTarget,
            PublicationStep::PrepareCandidate => {
                crate::diagnostic::FileSystemOperation::PrepareCandidate
            }
            PublicationStep::Publish | PublicationStep::Finalize => {
                crate::diagnostic::FileSystemOperation::Rename
            }
            PublicationStep::DiscardCandidate | PublicationStep::CleanupResidual => {
                crate::diagnostic::FileSystemOperation::Remove
            }
        };
        let projection = DirectoryPublicationDiagnostic::new(PublicationBackendCause::new(
            Diagnostic::file_system(crate::diagnostic::FileSystemIssue::new(
                crate::diagnostic::FileSystemDiagnosticContext::new(
                    crate::diagnostic::FileSystemDiagnosticStage::Publication,
                    operation,
                ),
                crate::diagnostic::FileSystemProblem::ExecutorClosed,
            )),
        ));
        if self.0 == "backend rollback failed" {
            projection.with_related(
                RelatedFailureRelation::Rollback,
                DirectoryPublicationDiagnostic::new(PublicationBackendCause::new(
                    Diagnostic::file_system(crate::diagnostic::FileSystemIssue::new(
                        crate::diagnostic::FileSystemDiagnosticContext::new(
                            crate::diagnostic::FileSystemDiagnosticStage::Publication,
                            crate::diagnostic::FileSystemOperation::Remove,
                        ),
                        crate::diagnostic::FileSystemProblem::ExecutorClosed,
                    )),
                )),
            )
        } else {
            projection
        }
    }
}

#[test]
fn published_residual_wire_keeps_output_residual_and_cleanup_relation() {
    let error = DirectoryPublishError::PublishedWithResiduals {
        target_root: PathBuf::from("D:/output/game"),
        residual_path: PathBuf::from("D:/output/.directory-publish/game/backup"),
        source: TestError("must not enter wire"),
    };

    assert_eq!(
        serde_json::to_value(error.diagnostic_report()).expect("发布诊断必须可序列化"),
        serde_json::json!({
            "effect": "applied_finalization_failed",
            "primary": {
                "code": "publication.finalization_failed",
                "stage": "publication",
                "issue": {
                    "family": "publication",
                    "details": {
                        "step": "finalize",
                        "problem": {
                            "kind": "published_finalization_failed",
                            "output_root": "D:/output/game",
                            "residual_path": "D:/output/.directory-publish/game/backup",
                            "cause": {
                                "diagnostic": {
                                    "code": "filesystem.executor_closed",
                                    "stage": "publication",
                                    "issue": {
                                        "family": "file_system",
                                        "details": {
                                            "context": {
                                                "stage": "publication",
                                                "operation": "remove"
                                            },
                                            "problem": {
                                                "kind": "executor_closed"
                                            }
                                        }
                                    },
                                    "resolution": "retry"
                                }
                            }
                        }
                    }
                },
                "resolution": "preserve_recovery_artifacts"
            },
            "related": []
        })
    );
}

#[test]
fn unpublished_main_and_cleanup_failures_are_one_recursive_report() {
    let error = DirectoryPublishError::NotPublished {
        target_root: PathBuf::from("D:/output/game"),
        source: TestError("publish failed"),
        cleanup_failure: Some(StagingCleanupFailure::new(
            PathBuf::from("D:/output/.directory-publish/game/stage"),
            TestError("cleanup failed"),
        )),
    };
    let value =
        serde_json::to_value(error.diagnostic_report()).expect("主错误和清理错误必须可原子序列化");

    assert_eq!(value["effect"], "recovery_required");
    assert_eq!(value["primary"]["code"], "publication.not_published");
    assert_eq!(
        value["primary"]["issue"]["details"]["problem"]["cause"]["diagnostic"]["issue"]["family"],
        "file_system"
    );
    assert_eq!(value["related"][0]["relation"], "cleanup");
    assert_eq!(
        value["related"][0]["report"]["primary"]["code"],
        "publication.cleanup_failed"
    );
    assert_eq!(
        value["related"][0]["report"]["primary"]["issue"]["details"]["problem"]["residual_path"],
        "D:/output/.directory-publish/game/stage"
    );
    assert_eq!(
        value["related"][0]["report"]["primary"]["issue"]["details"]["problem"]["cause"]["diagnostic"]
            ["issue"]["family"],
        "file_system"
    );
    assert_eq!(
        value["related"][0]["report"]["related"],
        serde_json::json!([])
    );
    assert!(!value.to_string().contains("publish failed"));
    assert!(!value.to_string().contains("cleanup failed"));
}

#[test]
fn backend_related_failure_is_lifted_out_of_publication_issue() {
    let error = DirectoryPublishError::NotPublished {
        target_root: PathBuf::from("D:/output/game"),
        source: TestError("backend rollback failed"),
        cleanup_failure: None,
    };
    let value =
        serde_json::to_value(error.diagnostic_report()).expect("底层相关失败必须提升到报告关系树");

    assert_eq!(value["related"][0]["relation"], "rollback");
    assert_eq!(
        value["related"][0]["report"]["primary"]["issue"]["family"],
        "file_system"
    );
    assert_eq!(
        value["related"][0]["report"]["primary"]["issue"]["details"]["context"]["operation"],
        "remove"
    );
    assert_eq!(
        value["related"][0]["report"]["related"],
        serde_json::json!([])
    );
}

#[test]
fn known_unpublished_states_preserve_candidate_cleanup_failure() {
    let cleanup = || {
        Some(StagingCleanupFailure::new(
            PathBuf::from("target.stage"),
            TestError("cleanup failed"),
        ))
    };
    let errors = [
        DirectoryPublishError::TargetAlreadyExists {
            target_root: PathBuf::from("target"),
            cleanup_failure: cleanup(),
        },
        DirectoryPublishError::TargetMissing {
            target_root: PathBuf::from("target"),
            cleanup_failure: cleanup(),
        },
        DirectoryPublishError::TargetNotDirectory {
            target_root: PathBuf::from("target"),
            cleanup_failure: cleanup(),
        },
    ];

    for error in errors {
        let display = error.to_string();
        assert!(display.contains("target"));
        assert!(display.contains("target.stage"));
        assert!(display.contains("cleanup failed"));
        assert_eq!(
            Error::source(&error).map(ToString::to_string),
            Some("无法清理目录 target.stage：cleanup failed".to_owned())
        );
    }

    let error = DirectoryPublishError::NotPublished {
        target_root: PathBuf::from("target"),
        source: TestError("swap failed"),
        cleanup_failure: cleanup(),
    };
    assert!(error.to_string().contains("target.stage"));
    assert_eq!(
        Error::source(&error).map(ToString::to_string),
        Some("swap failed".to_owned())
    );
}

#[test]
fn terminal_errors_report_published_and_unknown_outcomes_without_collapsing_them() {
    let published = DirectoryPublishError::PublishedWithResiduals {
        target_root: PathBuf::from("target"),
        residual_path: PathBuf::from("target.backup"),
        source: TestError("backup cleanup failed"),
    };
    assert!(published.to_string().contains("已发布"));
    assert!(published.to_string().contains("target.backup"));

    let unknown = DirectoryPublishError::OutcomeUnknown {
        target_root: PathBuf::from("target"),
        recovery_artifacts: vec![
            PathBuf::from("target.stage"),
            PathBuf::from("target.backup"),
        ],
        source: TestError("recovery failed"),
    };
    let display = unknown.to_string();
    assert!(display.contains("结果未知"));
    assert!(display.contains("target.stage"));
    assert!(display.contains("target.backup"));
    assert_eq!(
        Error::source(&unknown).map(ToString::to_string),
        Some("recovery failed".to_owned())
    );
}

#[test]
fn prepare_and_discard_errors_keep_the_exact_residual_paths() {
    let prepare = DirectoryPrepareError::NotPrepared {
        target_root: PathBuf::from("target"),
        source: TestError("copy failed"),
        cleanup_failure: Some(StagingCleanupFailure::new(
            PathBuf::from("target.stage"),
            TestError("cleanup failed"),
        )),
    };
    assert!(prepare.to_string().contains("target.stage"));
    assert_eq!(
        Error::source(&prepare).map(ToString::to_string),
        Some("copy failed".to_owned())
    );

    let discard =
        DirectoryDiscardError::new(PathBuf::from("target.stage"), TestError("delete failed"));
    assert_eq!(discard.staging_root(), Path::new("target.stage"));
    assert_eq!(discard.source().0, "delete failed");
    assert!(discard.to_string().contains("target.stage"));
    assert_eq!(
        Error::source(&discard).map(ToString::to_string),
        Some("delete failed".to_owned())
    );
}
