use super::super::error::SystemFileSystemError;
use super::*;
use crate::diagnostic::FileSystemJournalViolation;
use crate::runtime::windows::FileIdentity;
use std::fs::{self, OpenOptions};
use std::io::Write;

#[test]
fn journal_ignores_only_the_final_incomplete_frame() {
    let temporary = tempfile::tempdir().expect("测试目录应该可创建");
    let path = temporary.path().join("journal");
    let record = JournalRecord {
        target_name: "target".encode_utf16().collect(),
        original_identity: FileIdentity::from_parts(1, [2; 16]),
        candidate_identity: FileIdentity::from_parts(1, [3; 16]),
        phase: JournalPhase::OriginalMoveIntent,
    };
    append_journal(&path, &record, true).expect("完整帧应该可写入");
    OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("journal 应该可追加")
        .write_all(&[5, 0])
        .expect("应该可写入截断帧");
    let records = read_journal(&path).expect("最终不完整帧应该回退");
    assert_eq!(records.len(), 1);
}

#[test]
fn a_complete_corrupt_journal_frame_is_rejected() {
    let temporary = tempfile::tempdir().expect("应该可创建临时目录");
    let path = temporary.path().join("corrupt.journal");
    let payload = b"{}";
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    bytes.extend_from_slice(payload);
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    fs::write(&path, bytes).expect("应该可写入完整损坏帧");

    assert!(matches!(
        read_journal(&path),
        Err(SystemFileSystemError::JournalCorrupt {
            violation: FileSystemJournalViolation::CrcMismatch { frame_index: 1 },
            ..
        })
    ));
}
