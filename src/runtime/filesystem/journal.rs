//! 目录交换 journal 的帧写入、校验和协议解析。

use super::access::read_all_bytes;
use super::error::{SystemFileSystemError, io_error};
use crate::diagnostic::{FileSystemJournalViolation, FileSystemPathViolation, SafeIdentifier};
use crate::json_diagnostic::JsonErrorCategory;
use crate::runtime::windows::{
    FileIdentity, pin_directory_without_reparse, pin_path_without_reparse,
};
use crc32fast::Hasher;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) enum JournalPhase {
    OriginalMoveIntent,
    CandidateMoveIntent,
    CandidateVisible,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JournalRecord {
    pub(super) target_name: Vec<u16>,
    pub(super) original_identity: FileIdentity,
    pub(super) candidate_identity: FileIdentity,
    pub(super) phase: JournalPhase,
}

fn journal_json_coordinates(source: &serde_json::Error) -> (SafeIdentifier, u64, u64) {
    (
        SafeIdentifier::from_validated(JsonErrorCategory::from(source).storage_name()),
        journal_usize_to_u64(source.line()),
        journal_usize_to_u64(source.column()),
    )
}

fn journal_usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).expect("当前目标平台的 journal 数值必须能用 u64 表达")
}

const fn journal_phase_name(phase: JournalPhase) -> &'static str {
    match phase {
        JournalPhase::OriginalMoveIntent => "original_move_intent",
        JournalPhase::CandidateMoveIntent => "candidate_move_intent",
        JournalPhase::CandidateVisible => "candidate_visible",
    }
}

pub(super) fn append_journal(
    path: &Path,
    record: &JournalRecord,
    create_new: bool,
) -> Result<(), SystemFileSystemError> {
    let payload = serde_json::to_vec(record).map_err(|source| {
        let (category, line, column) = journal_json_coordinates(&source);
        SystemFileSystemError::JournalCorrupt {
            path: path.to_path_buf(),
            violation: FileSystemJournalViolation::Serialization {
                category,
                line,
                column,
            },
        }
    })?;
    let payload_len =
        u32::try_from(payload.len()).map_err(|_| SystemFileSystemError::JournalCorrupt {
            path: path.to_path_buf(),
            violation: FileSystemJournalViolation::FrameLengthOverflow {
                actual: journal_usize_to_u64(payload.len()),
                maximum: u64::from(u32::MAX),
            },
        })?;
    let mut hasher = Hasher::new();
    hasher.update(&payload);
    let crc = hasher.finalize();
    let parent = path
        .parent()
        .ok_or_else(|| SystemFileSystemError::InvalidPath {
            path: path.to_path_buf(),
            violation: FileSystemPathViolation::MissingParent,
        })?;
    let _pinned_parent = pin_directory_without_reparse(parent)?;
    let mut options = OpenOptions::new();
    options
        .append(true)
        .create_new(create_new)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    let mut file = options
        .open(path)
        .map_err(|source| io_error("打开目录发布 journal", path, source))?;
    let metadata = file
        .metadata()
        .map_err(|source| io_error("复核目录发布 journal", path, source))?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(SystemFileSystemError::JournalCorrupt {
            path: path.to_path_buf(),
            violation: FileSystemJournalViolation::NotRegularFile,
        });
    }
    file.write_all(&payload_len.to_le_bytes())
        .and_then(|()| file.write_all(&payload))
        .and_then(|()| file.write_all(&crc.to_le_bytes()))
        .map_err(|source| io_error("写入目录发布 journal", path, source))?;
    file.sync_data()
        .map_err(|source| io_error("同步目录发布 journal", path, source))
}

pub(super) fn read_journal(path: &Path) -> Result<Vec<JournalRecord>, SystemFileSystemError> {
    let mut pinned = pin_path_without_reparse(path)?;
    let metadata = pinned.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(SystemFileSystemError::JournalCorrupt {
            path: path.to_path_buf(),
            violation: FileSystemJournalViolation::NotRegularFile,
        });
    }
    let bytes = read_all_bytes(pinned.file_mut())
        .map_err(|source| io_error("读取目录发布 journal", path, source))?;
    let mut offset = 0_usize;
    let mut records: Vec<JournalRecord> = Vec::new();
    while offset < bytes.len() {
        if bytes.len() - offset < std::mem::size_of::<u32>() {
            break;
        }
        let length = u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("已确认 journal 长度头完整"),
        ) as usize;
        let frame_end = offset + 4 + length + 4;
        if frame_end > bytes.len() {
            break;
        }
        let payload = &bytes[offset + 4..offset + 4 + length];
        let expected_crc = u32::from_le_bytes(
            bytes[offset + 4 + length..frame_end]
                .try_into()
                .expect("已确认 journal CRC 完整"),
        );
        let mut hasher = Hasher::new();
        hasher.update(payload);
        if hasher.finalize() != expected_crc {
            return Err(SystemFileSystemError::JournalCorrupt {
                path: path.to_path_buf(),
                violation: FileSystemJournalViolation::CrcMismatch {
                    frame_index: journal_usize_to_u64(records.len() + 1),
                },
            });
        }
        let record: JournalRecord = serde_json::from_slice(payload).map_err(|source| {
            let (category, line, column) = journal_json_coordinates(&source);
            SystemFileSystemError::JournalCorrupt {
                path: path.to_path_buf(),
                violation: FileSystemJournalViolation::InvalidJson {
                    frame_index: journal_usize_to_u64(records.len() + 1),
                    category,
                    line,
                    column,
                },
            }
        })?;
        if let Some(first) = records.first()
            && (first.target_name != record.target_name
                || first.original_identity != record.original_identity
                || first.candidate_identity != record.candidate_identity)
        {
            return Err(SystemFileSystemError::JournalCorrupt {
                path: path.to_path_buf(),
                violation: FileSystemJournalViolation::FrameIdentityMismatch {
                    frame_index: journal_usize_to_u64(records.len() + 1),
                },
            });
        }
        let expected_phase = match records.len() {
            0 => JournalPhase::OriginalMoveIntent,
            1 => JournalPhase::CandidateMoveIntent,
            2 => JournalPhase::CandidateVisible,
            _ => {
                return Err(SystemFileSystemError::JournalCorrupt {
                    path: path.to_path_buf(),
                    violation: FileSystemJournalViolation::ExtraFrame {
                        frame_index: journal_usize_to_u64(records.len() + 1),
                    },
                });
            }
        };
        if record.phase != expected_phase {
            return Err(SystemFileSystemError::JournalCorrupt {
                path: path.to_path_buf(),
                violation: FileSystemJournalViolation::PhaseOrder {
                    frame_index: journal_usize_to_u64(records.len() + 1),
                    expected: SafeIdentifier::from_validated(journal_phase_name(expected_phase)),
                    actual: SafeIdentifier::from_validated(journal_phase_name(record.phase)),
                },
            });
        }
        records.push(record);
        offset = frame_end;
    }
    Ok(records)
}

#[cfg(test)]
mod tests;
