use super::*;
use crate::diagnostic::{FileSystemDiagnosticContext, FileSystemPathViolation, StateEffect};
use crate::storage::file_system::DirectoryDiscardError;
use std::path::PathBuf;

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

#[test]
fn invalid_path_leaf_uses_typed_violation_without_display_protocol() {
    let source = SystemFileSystemError::InvalidPath {
        path: PathBuf::from("candidate/data"),
        violation: FileSystemPathViolation::HardLink,
    };
    let report = source.diagnostic_report(
        FileSystemDiagnosticContext::new(
            crate::diagnostic::FileSystemDiagnosticStage::Publication,
            crate::diagnostic::FileSystemOperation::PrepareCandidate,
        ),
        StateEffect::Unchanged,
    );
    assert_eq!(
        serde_json::to_value(report).expect("报告应可序列化"),
        serde_json::json!({
            "effect": "unchanged",
            "primary": {
                "code": "filesystem.invalid_path",
                "stage": "publication",
                "issue": {
                    "family": "file_system",
                    "details": {
                        "context": {
                            "stage": "publication",
                            "operation": "prepare_candidate"
                        },
                        "problem": {
                            "kind": "invalid_path",
                            "path": "candidate/data",
                            "violation": "hard_link"
                        }
                    }
                },
                "resolution": "report_bug"
            },
            "related": []
        })
    );
}

#[test]
fn production_discard_error_projects_publication_and_file_system_facts() {
    let error = DirectoryDiscardError::new(
        PathBuf::from("D:/output/.directory-publish/game/stage"),
        Box::new(SystemFileSystemError::Closed),
    );
    let value =
        serde_json::to_value(error.diagnostic_report()).expect("生产目录丢弃诊断必须可序列化");

    assert_eq!(value["effect"], "recovery_required");
    assert_eq!(value["primary"]["code"], "publication.discard_failed");
    assert_eq!(
        value["primary"]["issue"]["details"]["problem"]["candidate_root"],
        "D:/output/.directory-publish/game/stage"
    );
    assert_eq!(
        value["primary"]["issue"]["details"]["problem"]["cause"]["diagnostic"]["issue"]["family"],
        "file_system"
    );
    assert_eq!(
        value["primary"]["issue"]["details"]["problem"]["cause"]["diagnostic"]["issue"]["details"]
            ["context"]["operation"],
        "remove"
    );
    assert_eq!(value["related"], serde_json::json!([]));
}
