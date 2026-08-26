//! MV、MZ 与 Generic 共用的 TOML 人工补译契约。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::diagnostic::{
    Diagnostic, DiagnosticReport, FileSystemDiagnosticContext, FileSystemDiagnosticStage,
    FileSystemIssue, FileSystemOperation, FileSystemPathViolation, FileSystemProblem,
    FileSystemRecoveryViolation, IoFailure, RelatedFailureRelation, RuntimeComponent, RuntimeIssue,
    SafePath, SafeText, StateEffect, public_path, render_diagnostic_report,
    render_state_effect_impact,
};
use crate::execution::CooperativeCancellation;
use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::generic::{
    GenericCompiledPlaceholderRules, GenericPlaceholderService, GenericProjectError,
    validate_current_generic_schema_with_cancellation,
    validate_translation_placeholders_with_cancellation,
};
use crate::i18n::{UiLocalizer, UiMessage};
use crate::language::{LanguageId, LanguageModule, LanguageModuleCatalog, LanguagePair};
use crate::rpg_maker::RpgMakerEngine;
use crate::rpg_maker::asset::RpgMakerAssetOwner;
use crate::rpg_maker::location_codec::{RpgMakerLocationCodec, RpgMakerProjectionCodec};
#[cfg(test)]
use crate::rpg_maker::model::{
    DirectTextPart, DirectTextRecipe, ScalarFieldKey, TextProjectionRecipe,
};
use crate::rpg_maker::model::{TextUnitContent, TextUnitRole};
use crate::rpg_maker::project_database::{
    CurrentRpgMakerSchemaValidationError, validate_current_rpg_maker_schema_with_check,
};
use crate::rpg_maker::semantic_order::RpgMakerSemanticOrderKey;
use crate::rpg_maker::text::{
    RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, TextGroupKind,
};
use crate::rpg_maker::translate::pipeline::{ExpectedLineShape, TranslationUnitIdentity};
use crate::rpg_maker::translate::placeholder::{
    CompiledPlaceholderRules, Pcre2PlaceholderService, RpgMakerBuiltinPlaceholderProfile,
    RpgMakerSourceBoundPlaceholderError,
};
use crate::rpg_maker::translate::planner::expected_line_shape;
use crate::rpg_maker::translate::semantics::{
    PreparedTranslationStatus, ResolvedTranslationSemantics,
};
#[cfg(test)]
use crate::runtime::windows::rename_with_replace_if_identity;
use crate::runtime::windows::{
    FileIdentity, PinnedPath, WindowsFsError, create_new_atomic_replace_candidate,
    delete_open_atomic_replace_candidate, pin_directory_without_reparse, pin_path_without_reparse,
    rename_open_atomic_replace_candidate_with_replace,
    rename_open_atomic_replace_candidate_without_replace,
};
use crate::translation::candidate_validation::{
    CandidateTextShape, ProvenInvariantViolation, validate_candidate_text,
};
use crate::translation::placeholder::PlaceholderRuleDefinition;
use crate::translation::planning_resource::CompiledTerminology;

const MANUAL_SQLITE_CANCELLATION_CHECK_OPERATIONS: i32 = 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManualOperation {
    Export,
    OwnershipExport,
    TranslationExport,
    Check,
    Apply,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ManualExportSelection {
    Pending,
    Rejected,
    All,
    Ids(PathBuf),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ManualTranslationType {
    Fixed,
    Free,
}

impl ManualTranslationType {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::Free => "free",
        }
    }

    fn from_storage_name(value: &str) -> Option<Self> {
        match value {
            "fixed" => Some(Self::Fixed),
            "free" => Some(Self::Free),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManualTranslationOrigin {
    Manual,
    Automatic,
}

impl ManualTranslationOrigin {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Automatic => "automatic",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManualOutdatedTranslation {
    pub(crate) id: String,
    pub(crate) kind: ManualTranslationType,
    pub(crate) source: Vec<String>,
    pub(crate) translation: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManualDetachedTranslation {
    pub(crate) snapshot: ManualOutdatedTranslation,
    pub(crate) locator: ManualTranslationLocator,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StoredManualTranslation {
    id: String,
    kind: ManualTranslationType,
    source: Vec<String>,
    translation: Vec<String>,
    applicability: Sha256Fingerprint,
}

/// 数据库内部的精确位置。该值永不进入 TOML 或用户输出。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ManualTranslationLocator {
    Generic {
        group_id: String,
        unit_id: String,
    },
    RpgMaker {
        owner: String,
        group_location: String,
        unit_role: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManualTranslationEntry {
    pub(crate) id: String,
    pub(crate) kind: ManualTranslationType,
    pub(crate) source: Vec<String>,
    pub(crate) locator: ManualTranslationLocator,
    pub(crate) rpg_maker_owner: Option<ManualRpgMakerOwner>,
    pub(crate) applicability: Sha256Fingerprint,
    pub(crate) needs_translation: bool,
    pub(crate) placeholder_scope: String,
    pub(crate) current_translation: Option<Vec<String>>,
    pub(crate) origin: Option<ManualTranslationOrigin>,
    pub(crate) rejected: Option<ManualRejectedCandidate>,
    pub(crate) outdated_manual: Option<ManualOutdatedTranslation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManualRejectedCandidate {
    pub(crate) origin: ManualTranslationOrigin,
    pub(crate) candidate_json: String,
    pub(crate) translation: Option<Vec<String>>,
    pub(crate) violation: ProvenInvariantViolation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManualRpgMakerOwner {
    Builtin,
    Rules { rule_number: usize },
}

#[derive(Clone, Debug)]
pub(crate) struct ManualTranslationIndex {
    entries: Vec<ManualTranslationEntry>,
    by_id: BTreeMap<String, usize>,
}

impl ManualTranslationIndex {
    pub(crate) fn new(entries: Vec<ManualTranslationEntry>) -> Result<Self, ManualIndexError> {
        let mut by_id = BTreeMap::new();
        for (index, entry) in entries.iter().enumerate() {
            if entry.id.is_empty() || entry.id.chars().any(char::is_control) {
                return Err(ManualIndexError::InvalidId(entry.id.clone()));
            }
            if let Some(previous) = by_id.insert(entry.id.clone(), index) {
                return Err(ManualIndexError::DuplicateId {
                    id: entry.id.clone(),
                    first: entries[previous].locator.clone(),
                    second: entry.locator.clone(),
                });
            }
        }
        Ok(Self { entries, by_id })
    }

    pub(crate) fn entries(&self) -> &[ManualTranslationEntry] {
        &self.entries
    }

    pub(crate) fn get(&self, id: &str) -> Option<&ManualTranslationEntry> {
        self.by_id.get(id).map(|index| &self.entries[*index])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ManualIndexError {
    InvalidId(String),
    DuplicateId {
        id: String,
        first: ManualTranslationLocator,
        second: ManualTranslationLocator,
    },
}

impl fmt::Display for ManualIndexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidId(id) => write!(formatter, "人工译文位置不可读：{id:?}"),
            Self::DuplicateId { id, .. } => write!(
                formatter,
                "人工译文位置 {id} 对应多个当前条目；请修正提取规则使位置唯一"
            ),
        }
    }
}

impl Error for ManualIndexError {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ManualDocument {
    #[serde(default)]
    translation: Vec<ManualDocumentEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ManualDocumentEntry {
    id: String,
    #[serde(rename = "type")]
    kind: ManualTranslationType,
    source: Vec<String>,
    translation: Vec<String>,
}

#[derive(Serialize)]
struct ManualOwnershipRecord<'a> {
    manual_id: &'a str,
    owner: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    rule_number: Option<usize>,
}

#[derive(Serialize)]
struct TranslationExportRecord<'a> {
    manual_id: &'a str,
    source: &'a [String],
    #[serde(flatten)]
    status: TranslationExportStatus<'a>,
    #[serde(rename = "type")]
    kind: ManualTranslationType,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rule_number: Option<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
enum TranslationExportStatus<'a> {
    Current {
        translation: &'a [String],
        origin: &'static str,
    },
    Pending {
        translation: (),
        origin: &'static str,
    },
    Rejected {
        translation: (),
        origin: &'static str,
        rejected_candidate_json: &'a str,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManualIdRecord {
    manual_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ValidatedManualTranslation {
    pub(crate) id: String,
    pub(crate) kind: ManualTranslationType,
    pub(crate) source: Vec<String>,
    pub(crate) translation: Vec<String>,
    pub(crate) locator: ManualTranslationLocator,
    pub(crate) applicability: Sha256Fingerprint,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManualCheckIssue {
    pub(crate) id: String,
    problem: ManualCheckProblem,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ManualCheckProblem {
    DuplicateEntry,
    UnknownId,
    InvalidSourceLine { line: usize },
    SourceChanged,
    TypeMismatch,
    InvalidTranslationLine { line: usize },
    FixedLength { expected: usize, actual: usize },
    FixedBlankSlot { slot: usize },
    PlaceholderMismatch,
    EmptyTranslation,
    InvalidStructure,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ManualCheckReport {
    pub(crate) valid: usize,
    pub(crate) unfilled: usize,
    pub(crate) errors: Vec<ManualCheckIssue>,
    pub(crate) writes: Vec<ValidatedManualTranslation>,
}

impl ManualCheckReport {
    pub(crate) fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }
}

#[derive(Debug)]
pub(crate) enum ManualDocumentError {
    Cancelled,
    Read {
        path: PathBuf,
        source: io::Error,
    },
    InvalidUtf8 {
        path: PathBuf,
    },
    InvalidToml {
        path: PathBuf,
        source: toml::de::Error,
    },
    InvalidIds {
        path: PathBuf,
        line: usize,
        problem: ManualIdsProblem,
    },
    Encode(toml::ser::Error),
    Write {
        path: PathBuf,
        source: io::Error,
    },
    OutputTarget {
        problem: ManualOutputTargetProblem,
    },
    ExistingTemporary {
        path: PathBuf,
    },
    ReplaceOutcomeUnknown {
        target: PathBuf,
        temporary: PathBuf,
    },
    TemporaryCleanup {
        operation: Box<ManualDocumentError>,
        temporary: PathBuf,
        cleanup: io::Error,
    },
}

#[derive(Debug)]
pub(crate) enum ManualIdsProblem {
    InvalidJson,
    InvalidId,
    DuplicateId { id: String },
    UnknownId { id: String },
}

/// Manual 导出在建立输出目标身份时能够确认的闭集问题。
///
/// 这些事实不能退化成普通写入错误，否则 CLI 会把同一目标、目录或 reparse point
/// 错误地解释为权限问题。
#[derive(Debug)]
pub(crate) enum ManualOutputTargetProblem {
    NotRegularFile { path: PathBuf },
    MissingFileName { path: PathBuf },
    ReparsePoint { path: PathBuf },
    NonLocalVolume { path: PathBuf },
    NonNtfsVolume { path: PathBuf, actual: String },
    CaseSensitiveDirectory { path: PathBuf },
    BindingCancelled { path: PathBuf },
    TargetAlreadyExists { path: PathBuf },
    IdentityChanged { path: PathBuf },
    BindingIo { path: PathBuf, failure: IoFailure },
}

impl ManualOutputTargetProblem {
    fn object(&self) -> String {
        match self {
            Self::NotRegularFile { path }
            | Self::MissingFileName { path }
            | Self::ReparsePoint { path }
            | Self::NonLocalVolume { path }
            | Self::NonNtfsVolume { path, .. }
            | Self::CaseSensitiveDirectory { path }
            | Self::BindingCancelled { path }
            | Self::TargetAlreadyExists { path }
            | Self::IdentityChanged { path }
            | Self::BindingIo { path, .. } => public_path(path),
        }
    }

    const fn reason_code(&self) -> &'static str {
        match self {
            Self::NotRegularFile { .. } => "not_regular_file",
            Self::MissingFileName { .. } => "invalid_path",
            Self::ReparsePoint { .. } => "reparse_point_forbidden",
            Self::NonLocalVolume { .. } => "non_local_volume",
            Self::NonNtfsVolume { .. } => "non_ntfs_volume",
            Self::CaseSensitiveDirectory { .. } => "case_sensitive_directory",
            Self::BindingCancelled { .. } => "lock_cancelled",
            Self::TargetAlreadyExists { .. } => "target_already_exists",
            Self::IdentityChanged { .. } => "file_identity_changed",
            Self::BindingIo { failure, .. } => failure.summary_code(),
        }
    }

    const fn help_code(&self) -> &'static str {
        match self {
            Self::NotRegularFile { .. }
            | Self::MissingFileName { .. }
            | Self::ReparsePoint { .. }
            | Self::NonLocalVolume { .. }
            | Self::NonNtfsVolume { .. }
            | Self::CaseSensitiveDirectory { .. } => "fix_input",
            Self::BindingCancelled { .. } => "retry",
            Self::TargetAlreadyExists { .. } | Self::IdentityChanged { .. } => "resolve_contention",
            Self::BindingIo { .. } => "check_path_and_permissions",
        }
    }
}

impl fmt::Display for ManualDocumentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("人工补译操作已取消"),
            Self::Read { path, .. } => write!(formatter, "无法读取 {}", public_path(path)),
            Self::InvalidUtf8 { path } => {
                write!(formatter, "{} 不是 UTF-8 TOML 文件", public_path(path))
            }
            Self::InvalidToml { path, .. } => {
                write!(formatter, "{} 不是有效的人工译文 TOML", public_path(path))
            }
            Self::InvalidIds {
                path,
                line,
                problem,
            } => write!(
                formatter,
                "{} 第 {line} 行的 Manual ID 无效：{problem}",
                public_path(path)
            ),
            Self::Encode(_) => formatter.write_str("无法生成人工译文 TOML"),
            Self::Write { path, .. } => write!(formatter, "无法写入 {}", public_path(path)),
            Self::OutputTarget { problem } => {
                write!(formatter, "Manual 输出目标无效：{}", problem.object())
            }
            Self::ExistingTemporary { path } => {
                write!(formatter, "固定临时文件已经存在：{}", public_path(path))
            }
            Self::ReplaceOutcomeUnknown { target, temporary } => write!(
                formatter,
                "无法确认 {} 是否已经替换；请保留并检查 {}",
                public_path(target),
                public_path(temporary)
            ),
            Self::TemporaryCleanup {
                operation,
                temporary,
                cleanup,
            } => write!(
                formatter,
                "{operation}；清理人工译文临时文件 {} 失败：{cleanup}",
                public_path(temporary)
            ),
        }
    }
}

impl Error for ManualDocumentError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } | Self::Write { source, .. } => Some(source),
            Self::InvalidToml { source, .. } => Some(source),
            Self::Encode(source) => Some(source),
            Self::TemporaryCleanup { operation, .. } => Some(operation.as_ref()),
            Self::Cancelled
            | Self::InvalidUtf8 { .. }
            | Self::InvalidIds { .. }
            | Self::OutputTarget { .. }
            | Self::ExistingTemporary { .. }
            | Self::ReplaceOutcomeUnknown { .. } => None,
        }
    }
}

impl fmt::Display for ManualIdsProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson => formatter.write_str("必须是且只能是 {\"manual_id\":\"自然ID\"}"),
            Self::InvalidId => formatter.write_str("manual_id 不能为空或包含控制字符"),
            Self::DuplicateId { id } => write!(formatter, "{id} 重复出现"),
            Self::UnknownId { id } => write!(formatter, "当前项目中不存在 {id}"),
        }
    }
}

#[cfg(test)]
pub(crate) fn export_manual_document(
    path: &Path,
    index: &ManualTranslationIndex,
) -> Result<usize, ManualDocumentError> {
    export_manual_document_with_cancellation(
        path,
        index,
        &ManualExportSelection::Pending,
        &CooperativeCancellation::default(),
    )
}

fn export_manual_document_with_cancellation(
    path: &Path,
    index: &ManualTranslationIndex,
    selection: &ManualExportSelection,
    cancellation: &CooperativeCancellation,
) -> Result<usize, ManualDocumentError> {
    ensure_manual_document_running(cancellation)?;
    let entries = select_manual_entries(index, selection, cancellation)?;
    let count = entries.len();
    let encoded = encode_manual_entries(&entries)?;
    ensure_manual_document_running(cancellation)?;
    atomic_replace(path, encoded.as_bytes(), cancellation)?;
    Ok(count)
}

fn select_manual_entries<'a>(
    index: &'a ManualTranslationIndex,
    selection: &ManualExportSelection,
    cancellation: &CooperativeCancellation,
) -> Result<Vec<&'a ManualTranslationEntry>, ManualDocumentError> {
    let selected_ids = match selection {
        ManualExportSelection::Ids(path) => Some(load_manual_ids(path, index, cancellation)?),
        ManualExportSelection::Pending
        | ManualExportSelection::Rejected
        | ManualExportSelection::All => None,
    };
    let mut entries = Vec::new();
    for entry in index.entries() {
        ensure_manual_document_running(cancellation)?;
        let selected = match selection {
            ManualExportSelection::Pending => entry.needs_translation && entry.rejected.is_none(),
            ManualExportSelection::Rejected => {
                entry.current_translation.is_none() && entry.rejected.is_some()
            }
            ManualExportSelection::All => true,
            ManualExportSelection::Ids(_) => selected_ids
                .as_ref()
                .is_some_and(|ids| ids.contains(entry.id.as_str())),
        };
        if selected {
            entries.push(entry);
        }
    }
    ensure_manual_document_running(cancellation)?;
    Ok(entries)
}

fn export_ownership_document_with_cancellation(
    path: &Path,
    index: &ManualTranslationIndex,
    cancellation: &CooperativeCancellation,
) -> Result<usize, ManualDocumentError> {
    ensure_manual_document_running(cancellation)?;
    let mut encoded = String::new();
    for entry in index.entries() {
        ensure_manual_document_running(cancellation)?;
        let (owner, rule_number) = match entry.rpg_maker_owner {
            Some(ManualRpgMakerOwner::Builtin) => ("builtin", None),
            Some(ManualRpgMakerOwner::Rules { rule_number }) => ("rules", Some(rule_number)),
            None => {
                return Err(ManualDocumentError::Write {
                    path: path.to_path_buf(),
                    source: io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Ownership export 只接受 RPG Maker 项目",
                    ),
                });
            }
        };
        encoded.push_str(
            &serde_json::to_string(&ManualOwnershipRecord {
                manual_id: &entry.id,
                owner,
                rule_number,
            })
            .expect("自然 ID、owner 和自然规则序号必须可编码"),
        );
        encoded.push('\n');
    }
    ensure_manual_document_running(cancellation)?;
    atomic_replace(path, encoded.as_bytes(), cancellation)?;
    Ok(index.entries().len())
}

fn export_translation_document_with_cancellation(
    path: &Path,
    index: &ManualTranslationIndex,
    cancellation: &CooperativeCancellation,
) -> Result<usize, ManualDocumentError> {
    ensure_manual_document_running(cancellation)?;
    let mut encoded = String::new();
    for entry in index.entries() {
        ensure_manual_document_running(cancellation)?;
        let status = match (
            entry.current_translation.as_deref(),
            entry.origin,
            entry.rejected.as_ref(),
        ) {
            (Some(translation), Some(origin), _) => TranslationExportStatus::Current {
                translation,
                origin: origin.as_str(),
            },
            (None, None, Some(rejected)) => TranslationExportStatus::Rejected {
                translation: (),
                origin: rejected.origin.as_str(),
                rejected_candidate_json: &rejected.candidate_json,
            },
            (None, None, None) => TranslationExportStatus::Pending {
                translation: (),
                origin: "none",
            },
            _ => unreachable!("Manual 快照的译文、来源和 Rejected 状态必须一致"),
        };
        let (owner, rule_number) = match entry.rpg_maker_owner {
            Some(ManualRpgMakerOwner::Builtin) => (Some("builtin"), None),
            Some(ManualRpgMakerOwner::Rules { rule_number }) => (Some("rules"), Some(rule_number)),
            None => (None, None),
        };
        encoded.push_str(
            &serde_json::to_string(&TranslationExportRecord {
                manual_id: &entry.id,
                source: &entry.source,
                status,
                kind: entry.kind,
                owner,
                rule_number,
            })
            .expect("已校验的 Translation export 记录必须可编码"),
        );
        encoded.push('\n');
    }
    ensure_manual_document_running(cancellation)?;
    atomic_replace(path, encoded.as_bytes(), cancellation)?;
    Ok(index.entries().len())
}

fn load_manual_ids(
    path: &Path,
    index: &ManualTranslationIndex,
    cancellation: &CooperativeCancellation,
) -> Result<BTreeSet<String>, ManualDocumentError> {
    ensure_manual_document_running(cancellation)?;
    let bytes = fs::read(path).map_err(|source| ManualDocumentError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let source = std::str::from_utf8(&bytes).map_err(|_| ManualDocumentError::InvalidIds {
        path: path.to_path_buf(),
        line: 1,
        problem: ManualIdsProblem::InvalidJson,
    })?;
    let mut ids = BTreeSet::new();
    for (line_index, line) in source.lines().enumerate() {
        ensure_manual_document_running(cancellation)?;
        let line_number = line_index + 1;
        let record = serde_json::from_str::<ManualIdRecord>(line).map_err(|_| {
            ManualDocumentError::InvalidIds {
                path: path.to_path_buf(),
                line: line_number,
                problem: ManualIdsProblem::InvalidJson,
            }
        })?;
        if record.manual_id.is_empty() || record.manual_id.chars().any(char::is_control) {
            return Err(ManualDocumentError::InvalidIds {
                path: path.to_path_buf(),
                line: line_number,
                problem: ManualIdsProblem::InvalidId,
            });
        }
        if !ids.insert(record.manual_id.clone()) {
            return Err(ManualDocumentError::InvalidIds {
                path: path.to_path_buf(),
                line: line_number,
                problem: ManualIdsProblem::DuplicateId {
                    id: record.manual_id,
                },
            });
        }
        if index.get(&record.manual_id).is_none() {
            return Err(ManualDocumentError::InvalidIds {
                path: path.to_path_buf(),
                line: line_number,
                problem: ManualIdsProblem::UnknownId {
                    id: record.manual_id,
                },
            });
        }
    }
    ensure_manual_document_running(cancellation)?;
    Ok(ids)
}

fn encode_manual_entries(
    entries: &[&ManualTranslationEntry],
) -> Result<String, ManualDocumentError> {
    let mut encoded = String::new();
    for entry in entries {
        if entry.current_translation.is_none()
            && let Some(rejected) = &entry.rejected
        {
            append_manual_comment(
                &mut encoded,
                "rejected_candidate_json",
                &rejected.candidate_json,
            );
            let violation =
                serde_json::to_string(&rejected.violation).expect("闭集 Rejected 违反项必须可编码");
            append_manual_comment(&mut encoded, "rejected_reason", &violation);
        }
        let translation = entry.current_translation.clone().or_else(|| {
            entry
                .rejected
                .as_ref()
                .and_then(|rejected| rejected.translation.clone())
        });
        encoded.push_str(
            &toml::to_string_pretty(&ManualDocument {
                translation: vec![ManualDocumentEntry {
                    id: entry.id.clone(),
                    kind: entry.kind,
                    source: entry.source.clone(),
                    translation: translation.unwrap_or_default(),
                }],
            })
            .map_err(ManualDocumentError::Encode)?,
        );
    }
    Ok(encoded)
}

fn append_manual_comment(output: &mut String, name: &str, value: &str) {
    for (index, line) in value.lines().enumerate() {
        if index == 0 {
            output.push_str("# ");
            output.push_str(name);
            output.push_str(": ");
        } else {
            output.push_str("#   ");
        }
        output.push_str(line);
        output.push('\n');
    }
}

#[cfg(test)]
pub(crate) fn check_manual_document(
    path: &Path,
    index: &ManualTranslationIndex,
    validate_placeholders: impl FnMut(&ManualTranslationEntry, &[String]) -> Result<(), String>,
) -> Result<ManualCheckReport, ManualDocumentError> {
    check_manual_document_with_cancellation(
        path,
        index,
        &CooperativeCancellation::default(),
        validate_placeholders,
    )
}

fn check_manual_document_with_cancellation(
    path: &Path,
    index: &ManualTranslationIndex,
    cancellation: &CooperativeCancellation,
    mut validate_placeholders: impl FnMut(&ManualTranslationEntry, &[String]) -> Result<(), String>,
) -> Result<ManualCheckReport, ManualDocumentError> {
    ensure_manual_document_running(cancellation)?;
    let bytes = fs::read(path).map_err(|source| ManualDocumentError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    ensure_manual_document_running(cancellation)?;
    let source = std::str::from_utf8(&bytes).map_err(|_| ManualDocumentError::InvalidUtf8 {
        path: path.to_path_buf(),
    })?;
    let document = toml::from_str::<ManualDocument>(source).map_err(|source| {
        ManualDocumentError::InvalidToml {
            path: path.to_path_buf(),
            source,
        }
    })?;
    check_document_with_cancellation(document, index, cancellation, &mut validate_placeholders)
}

fn check_document(
    document: ManualDocument,
    index: &ManualTranslationIndex,
    validate_placeholders: &mut impl FnMut(&ManualTranslationEntry, &[String]) -> Result<(), String>,
) -> ManualCheckReport {
    check_document_with_cancellation(
        document,
        index,
        &CooperativeCancellation::default(),
        validate_placeholders,
    )
    .expect("未请求取消的内存检查不能取消")
}

fn check_document_with_cancellation(
    document: ManualDocument,
    index: &ManualTranslationIndex,
    cancellation: &CooperativeCancellation,
    validate_placeholders: &mut impl FnMut(&ManualTranslationEntry, &[String]) -> Result<(), String>,
) -> Result<ManualCheckReport, ManualDocumentError> {
    let mut report = ManualCheckReport::default();
    let mut seen = BTreeSet::new();
    for item in document.translation {
        ensure_manual_document_running(cancellation)?;
        let id = item.id.clone();
        if !seen.insert(id.clone()) {
            push_issue(&mut report, id, ManualCheckProblem::DuplicateEntry);
            continue;
        }
        let Some(current) = index.get(&item.id) else {
            push_issue(&mut report, id, ManualCheckProblem::UnknownId);
            continue;
        };
        if let Some(line) = invalid_line(&item.source) {
            push_issue(
                &mut report,
                id,
                ManualCheckProblem::InvalidSourceLine { line: line + 1 },
            );
            continue;
        }
        if item.source != current.source {
            push_issue(&mut report, id, ManualCheckProblem::SourceChanged);
            continue;
        }
        if item.kind != current.kind {
            push_issue(&mut report, id, ManualCheckProblem::TypeMismatch);
            continue;
        }
        if item.translation.is_empty() {
            report.unfilled += 1;
            continue;
        }
        let shape = match current.kind {
            ManualTranslationType::Fixed => CandidateTextShape::Fixed,
            ManualTranslationType::Free => CandidateTextShape::Free,
        };
        if let Err(violation) = validate_candidate_text(&current.source, &item.translation, shape) {
            push_issue(&mut report, id, manual_text_violation_problem(violation));
            continue;
        }
        if validate_placeholders(current, &item.translation).is_err() {
            push_issue(&mut report, id, ManualCheckProblem::PlaceholderMismatch);
            continue;
        }
        report.valid += 1;
        report.writes.push(ValidatedManualTranslation {
            id: item.id,
            kind: item.kind,
            source: item.source,
            translation: item.translation,
            locator: current.locator.clone(),
            applicability: current.applicability,
        });
    }
    ensure_manual_document_running(cancellation)?;
    Ok(report)
}

fn manual_text_violation_problem(violation: ProvenInvariantViolation) -> ManualCheckProblem {
    match violation {
        ProvenInvariantViolation::LineCountMismatch { expected, actual } => {
            ManualCheckProblem::FixedLength { expected, actual }
        }
        ProvenInvariantViolation::InvalidLineText { line_index }
        | ProvenInvariantViolation::ContainsByteOrderMark { line_index } => {
            ManualCheckProblem::InvalidTranslationLine {
                line: line_index + 1,
            }
        }
        ProvenInvariantViolation::BlankTranslation => ManualCheckProblem::EmptyTranslation,
        ProvenInvariantViolation::FixedBlankSlotChanged { line_index } => {
            ManualCheckProblem::FixedBlankSlot {
                slot: line_index + 1,
            }
        }
        ProvenInvariantViolation::FixedNonBlankSlotEmptied { .. } => {
            ManualCheckProblem::EmptyTranslation
        }
        ProvenInvariantViolation::PlaceholderMismatch
        | ProvenInvariantViolation::UnexpectedPlaceholderToken
        | ProvenInvariantViolation::PlaceholderBoundaryChanged
        | ProvenInvariantViolation::ReservedPlaceholderToken
        | ProvenInvariantViolation::InvalidCandidateShape => {
            ManualCheckProblem::PlaceholderMismatch
        }
    }
}

fn ensure_manual_document_running(
    cancellation: &CooperativeCancellation,
) -> Result<(), ManualDocumentError> {
    if cancellation.is_requested() {
        Err(ManualDocumentError::Cancelled)
    } else {
        Ok(())
    }
}

fn invalid_line(lines: &[String]) -> Option<usize> {
    lines.iter().position(|line| {
        line.chars()
            .any(|character| matches!(character, '\r' | '\n' | '\0'))
    })
}

fn push_issue(report: &mut ManualCheckReport, id: String, problem: ManualCheckProblem) {
    report.errors.push(ManualCheckIssue { id, problem });
}

fn atomic_replace(
    path: &Path,
    bytes: &[u8],
    cancellation: &CooperativeCancellation,
) -> Result<(), ManualDocumentError> {
    let target = bind_manual_output_target(path)?;
    let temporary = manual_temporary_path(target.resolved_path());
    ensure_manual_document_running(cancellation)?;
    let mut file = create_new_atomic_replace_candidate(&temporary)
        .map_err(|source| manual_temporary_open_error(path, &temporary, source))?;
    let identity =
        manual_temporary_identity(&file, path, &temporary, FileIdentity::of(&file, &temporary))?;
    if let Err(source) = file.write_all(bytes) {
        let operation = ManualDocumentError::Write {
            path: path.to_path_buf(),
            source,
        };
        return Err(cleanup_open_manual_temporary(&file, &temporary, operation));
    }
    if let Err(operation) = ensure_manual_document_running(cancellation) {
        return Err(cleanup_open_manual_temporary(&file, &temporary, operation));
    }
    if let Err(source) = file.sync_all() {
        let operation = ManualDocumentError::Write {
            path: path.to_path_buf(),
            source,
        };
        return Err(cleanup_open_manual_temporary(&file, &temporary, operation));
    }
    if let Err(operation) = ensure_manual_document_running(cancellation) {
        return Err(cleanup_open_manual_temporary(&file, &temporary, operation));
    }
    let replaced = match target.initial_identity() {
        Some(target_identity) => rename_open_atomic_replace_candidate_with_replace(
            file,
            &temporary,
            target.resolved_path(),
            identity,
            target_identity,
        ),
        None => rename_open_atomic_replace_candidate_without_replace(
            file,
            &temporary,
            target.resolved_path(),
            identity,
        ),
    };
    match replaced {
        Ok(()) => Ok(()),
        Err(failure) => {
            let (source, candidate) = failure.into_parts();
            let operation = manual_replace_error(path, source);
            match candidate {
                Some(candidate) => Err(cleanup_open_manual_temporary(
                    &candidate, &temporary, operation,
                )),
                None => Err(operation),
            }
        }
    }
}

struct BoundManualOutputTarget {
    _parent: PinnedPath,
    resolved_path: PathBuf,
    initial_identity: Option<FileIdentity>,
}

impl BoundManualOutputTarget {
    fn resolved_path(&self) -> &Path {
        &self.resolved_path
    }

    const fn initial_identity(&self) -> Option<FileIdentity> {
        self.initial_identity
    }
}

fn bind_manual_output_target(path: &Path) -> Result<BoundManualOutputTarget, ManualDocumentError> {
    let file_name = path
        .file_name()
        .ok_or_else(|| ManualDocumentError::OutputTarget {
            problem: ManualOutputTargetProblem::MissingFileName {
                path: path.to_path_buf(),
            },
        })?
        .to_owned();
    let parent_path = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = pin_directory_without_reparse(parent_path)
        .map_err(|source| manual_target_error(path, source))?;
    let resolved_path = parent.resolved_path().join(file_name);
    let initial_identity = match pin_path_without_reparse(&resolved_path) {
        Ok(pinned) => {
            let metadata = pinned
                .metadata()
                .map_err(|source| manual_target_error(path, source))?;
            if !metadata.is_file() {
                return Err(ManualDocumentError::OutputTarget {
                    problem: ManualOutputTargetProblem::NotRegularFile {
                        path: path.to_path_buf(),
                    },
                });
            }
            Some(
                FileIdentity::of(pinned.file(), &resolved_path)
                    .map_err(|source| manual_target_error(path, source))?,
            )
        }
        Err(WindowsFsError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => None,
        Err(source) => return Err(manual_target_error(path, source)),
    };
    Ok(BoundManualOutputTarget {
        _parent: parent,
        resolved_path,
        initial_identity,
    })
}

fn manual_output_identity_changed(path: &Path) -> ManualDocumentError {
    ManualDocumentError::OutputTarget {
        problem: ManualOutputTargetProblem::IdentityChanged {
            path: path.to_path_buf(),
        },
    }
}

fn manual_replace_error(path: &Path, source: WindowsFsError) -> ManualDocumentError {
    match source {
        WindowsFsError::RenameTargetUnconfirmed { path: target } => {
            ManualDocumentError::ReplaceOutcomeUnknown {
                temporary: manual_temporary_path(&target),
                target,
            }
        }
        WindowsFsError::FileIdentityChanged { .. } => manual_output_identity_changed(path),
        source => manual_target_error(path, source),
    }
}

fn manual_target_error(path: &Path, source: WindowsFsError) -> ManualDocumentError {
    let problem = match source {
        WindowsFsError::Io { source, .. } => ManualOutputTargetProblem::BindingIo {
            path: path.to_path_buf(),
            failure: IoFailure::from_error(&source),
        },
        WindowsFsError::ReparsePoint { path } => ManualOutputTargetProblem::ReparsePoint { path },
        WindowsFsError::NonLocalVolume { path } => {
            ManualOutputTargetProblem::NonLocalVolume { path }
        }
        WindowsFsError::NonNtfsVolume { path, actual } => {
            ManualOutputTargetProblem::NonNtfsVolume { path, actual }
        }
        WindowsFsError::CaseSensitiveDirectory { path } => {
            ManualOutputTargetProblem::CaseSensitiveDirectory { path }
        }
        WindowsFsError::LockCancelled { path } => {
            ManualOutputTargetProblem::BindingCancelled { path }
        }
        WindowsFsError::RenameTargetExists { path } => {
            ManualOutputTargetProblem::TargetAlreadyExists { path }
        }
        WindowsFsError::RenameTargetUnconfirmed { path } => {
            ManualOutputTargetProblem::IdentityChanged { path }
        }
        WindowsFsError::FileIdentityChanged { path } => {
            ManualOutputTargetProblem::IdentityChanged { path }
        }
    };
    ManualDocumentError::OutputTarget { problem }
}

fn manual_temporary_path(path: &Path) -> PathBuf {
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{file_name}.tmp"))
}

fn manual_temporary_open_error(
    target: &Path,
    temporary: &Path,
    source: WindowsFsError,
) -> ManualDocumentError {
    match source {
        WindowsFsError::Io { source, .. } if source.kind() == io::ErrorKind::AlreadyExists => {
            ManualDocumentError::ExistingTemporary {
                path: temporary.to_path_buf(),
            }
        }
        source => ManualDocumentError::Write {
            path: target.to_path_buf(),
            source: io::Error::other(source),
        },
    }
}

fn manual_temporary_identity(
    file: &fs::File,
    target: &Path,
    temporary: &Path,
    identity: Result<FileIdentity, WindowsFsError>,
) -> Result<FileIdentity, ManualDocumentError> {
    identity.map_err(|source| {
        let operation = ManualDocumentError::Write {
            path: target.to_path_buf(),
            source: io::Error::other(source),
        };
        cleanup_open_manual_temporary(file, temporary, operation)
    })
}

fn cleanup_open_manual_temporary(
    file: &fs::File,
    temporary: &Path,
    operation: ManualDocumentError,
) -> ManualDocumentError {
    match delete_open_atomic_replace_candidate(file, temporary) {
        Ok(()) => operation,
        Err(cleanup) => ManualDocumentError::TemporaryCleanup {
            operation: Box::new(operation),
            temporary: temporary.to_path_buf(),
            cleanup: io::Error::other(cleanup),
        },
    }
}

#[derive(Clone)]
pub(crate) enum ManualPlaceholderValidator {
    Generic {
        service: GenericPlaceholderService,
        compiled: GenericCompiledPlaceholderRules,
    },
    RpgMaker {
        engine: RpgMakerEngine,
        service: Pcre2PlaceholderService,
        compiled: CompiledPlaceholderRules,
        profiles: HashMap<String, RpgMakerBuiltinPlaceholderProfile>,
    },
}

impl ManualPlaceholderValidator {
    pub(crate) fn validate(
        &self,
        entry: &ManualTranslationEntry,
        translation: &[String],
    ) -> Result<(), String> {
        let source = entry.source.join("\n");
        let translation_text = translation.join("\n");
        match self {
            Self::Generic { service, compiled } => {
                match validate_translation_placeholders_with_cancellation(
                    service,
                    compiled,
                    &entry.id,
                    &entry.placeholder_scope,
                    &source,
                    &translation_text,
                    || Ok::<_, std::convert::Infallible>(()),
                ) {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(_)) => Err("译文没有保留原文中的 Placeholder".to_owned()),
                    Err(unreachable) => match unreachable {},
                }
            }
            Self::RpgMaker {
                engine,
                service,
                compiled,
                profiles,
            } => {
                let Some(kind) = TextGroupKind::from_storage_name(&entry.placeholder_scope) else {
                    return Err("当前位置的 Placeholder 范围无效".to_owned());
                };
                let Some(&profile) = profiles.get(&entry.id) else {
                    return Err("当前位置不属于当前 RPG Maker Unit".to_owned());
                };
                let validate_slot = |source: &str, candidate: &str| {
                    let source = service
                        .protect_profile_with_cancellation(
                            *engine,
                            kind,
                            &entry.id,
                            profile,
                            source,
                            &[],
                            compiled,
                            || Ok::<_, std::convert::Infallible>(()),
                        )
                        .map_err(|unreachable| match unreachable {})
                        .and_then(|result| {
                            result.map_err(|_| "无法读取原文 Placeholder".to_owned())
                        })?;
                    let bound = match service.bind_profile_candidate_with_cancellation(
                        &source,
                        *engine,
                        kind,
                        &entry.id,
                        profile,
                        candidate,
                        compiled,
                        || Ok::<_, std::convert::Infallible>(()),
                    ) {
                        Ok(bound) => bound,
                        Err(unreachable) => match unreachable {},
                    };
                    match bound {
                        Ok(_) => Ok(()),
                        Err(RpgMakerSourceBoundPlaceholderError::Binding(
                            crate::translation::placeholder_projection::SourceBoundPlaceholderError::Multiset(_)
                            | crate::translation::placeholder_projection::SourceBoundPlaceholderError::AmbiguousOriginal { .. }
                            | crate::translation::placeholder_projection::SourceBoundPlaceholderError::UnexpectedPlaceholder,
                        )) => Err(
                            "译文没有保留原文中的控制码或 Placeholder".to_owned(),
                        ),
                        Err(RpgMakerSourceBoundPlaceholderError::Protection(_)
                        | RpgMakerSourceBoundPlaceholderError::Binding(
                            crate::translation::placeholder_projection::SourceBoundPlaceholderError::Projection(_),
                        )) => {
                            Err("无法验证译文 Placeholder".to_owned())
                        }
                    }
                };
                match entry.kind {
                    ManualTranslationType::Fixed => {
                        if entry.source.len() != translation.len() {
                            return Err("译文行数与原文不一致".to_owned());
                        }
                        for (source, candidate) in entry.source.iter().zip(translation) {
                            validate_slot(source, candidate)?;
                        }
                        Ok(())
                    }
                    ManualTranslationType::Free => validate_slot(&source, &translation_text),
                }
            }
        }
    }
}

pub(crate) struct ManualProjectSnapshot {
    pub(crate) index: ManualTranslationIndex,
    pub(crate) placeholders: ManualPlaceholderValidator,
}

/// Lua 高级接口额外读取已失去当前位置的人工译文。
pub(crate) struct ManualProjectLuaSnapshot {
    pub(crate) current: ManualProjectSnapshot,
    pub(crate) detached: Vec<ManualDetachedTranslation>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManualClearLocatorError {
    NotFound,
    Ambiguous,
}

impl ManualProjectLuaSnapshot {
    pub(crate) fn clear_locator(
        &self,
        id: &str,
    ) -> Result<&ManualTranslationLocator, ManualClearLocatorError> {
        let current = self.current.index.get(id);
        let mut detached = self.detached.iter().filter(|entry| entry.snapshot.id == id);
        let first_detached = detached.next();
        if detached.next().is_some() || (current.is_some() && first_detached.is_some()) {
            return Err(ManualClearLocatorError::Ambiguous);
        }
        current
            .map(|entry| &entry.locator)
            .or_else(|| first_detached.map(|entry| &entry.locator))
            .ok_or(ManualClearLocatorError::NotFound)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ManualCommandSummary {
    Exported {
        entries: usize,
        file: PathBuf,
    },
    Checked {
        report: ManualCheckReport,
    },
    Applied {
        report: ManualCheckReport,
        applied: usize,
    },
}

pub(crate) fn execute_generic_manual_command(
    database_path: &Path,
    operation: ManualOperation,
    file: &Path,
    export_selection: Option<&ManualExportSelection>,
    language_modules: Option<&LanguageModuleCatalog>,
    cancellation: &CooperativeCancellation,
) -> Result<ManualCommandSummary, ManualCommandError> {
    assert_eq!(
        matches!(operation, ManualOperation::Export),
        language_modules.is_some(),
        "只有 Manual export 应获得语言模块"
    );
    assert_eq!(
        matches!(operation, ManualOperation::Export),
        export_selection.is_some(),
        "只有 Manual export 应获得导出选择"
    );
    execute_manual_database_command(
        database_path,
        operation,
        file,
        export_selection,
        cancellation,
        ManualDatabaseOperations {
            validate_schema: |connection: &Connection| {
                validate_current_generic_schema_with_cancellation(connection, cancellation)
                    .map_err(manual_generic_schema_error)
            },
            load_snapshot: |connection: &Connection| {
                load_generic_manual_command_snapshot(connection, language_modules)
            },
            apply: |connection: &Connection, writes: &[ValidatedManualTranslation]| {
                apply_generic_manual_translations_with_cancellation(
                    connection,
                    writes,
                    cancellation,
                )
            },
        },
    )
}

fn manual_generic_schema_error(source: GenericProjectError) -> ManualDatabaseError {
    match source {
        GenericProjectError::Cancelled => ManualDatabaseError::Cancelled,
        GenericProjectError::Sqlite { source, .. } => ManualDatabaseError::Sqlite(source),
        source => ManualDatabaseError::InvalidProject(source.to_string()),
    }
}

pub(crate) fn execute_rpg_maker_manual_command(
    database_path: &Path,
    engine: RpgMakerEngine,
    operation: ManualOperation,
    file: &Path,
    export_selection: Option<&ManualExportSelection>,
    language_modules: Option<&LanguageModuleCatalog>,
    cancellation: &CooperativeCancellation,
) -> Result<ManualCommandSummary, ManualCommandError> {
    assert_eq!(
        matches!(operation, ManualOperation::Export),
        language_modules.is_some(),
        "只有 Manual export 应获得语言模块"
    );
    assert_eq!(
        matches!(operation, ManualOperation::Export),
        export_selection.is_some(),
        "只有 Manual export 应获得导出选择"
    );
    execute_manual_database_command(
        database_path,
        operation,
        file,
        export_selection,
        cancellation,
        ManualDatabaseOperations {
            validate_schema: |connection: &Connection| {
                let mut is_cancelled = || cancellation.is_requested();
                validate_current_rpg_maker_schema_with_check(connection, &mut is_cancelled).map_err(
                    |source| match source {
                        CurrentRpgMakerSchemaValidationError::Cancelled => {
                            ManualDatabaseError::Cancelled
                        }
                        CurrentRpgMakerSchemaValidationError::Database(source) => {
                            ManualDatabaseError::Sqlite(source)
                        }
                        CurrentRpgMakerSchemaValidationError::Invalid(source) => {
                            ManualDatabaseError::InvalidProject(source.to_string())
                        }
                    },
                )
            },
            load_snapshot: |connection: &Connection| {
                load_rpg_maker_manual_command_snapshot(connection, engine, language_modules)
            },
            apply: |connection: &Connection, writes: &[ValidatedManualTranslation]| {
                apply_rpg_maker_manual_translations_with_cancellation(
                    connection,
                    writes,
                    cancellation,
                )
            },
        },
    )
}

struct ManualDatabaseOperations<ValidateSchema, LoadSnapshot, Apply> {
    validate_schema: ValidateSchema,
    load_snapshot: LoadSnapshot,
    apply: Apply,
}

fn execute_manual_database_command(
    database_path: &Path,
    operation: ManualOperation,
    file: &Path,
    export_selection: Option<&ManualExportSelection>,
    cancellation: &CooperativeCancellation,
    operations: ManualDatabaseOperations<
        impl FnMut(&Connection) -> Result<(), ManualDatabaseError>,
        impl FnMut(&Connection) -> Result<ManualProjectSnapshot, ManualDatabaseError>,
        impl FnMut(&Connection, &[ValidatedManualTranslation]) -> Result<usize, ManualDatabaseError>,
    >,
) -> Result<ManualCommandSummary, ManualCommandError> {
    let ManualDatabaseOperations {
        mut validate_schema,
        mut load_snapshot,
        mut apply,
    } = operations;
    ensure_manual_command_running(cancellation)?;
    match operation {
        ManualOperation::Export
        | ManualOperation::OwnershipExport
        | ManualOperation::TranslationExport
        | ManualOperation::Check => {
            let mut connection = open_read_only(database_path, cancellation)
                .map_err(|source| manual_command_database_error(source, cancellation))?;
            ensure_manual_command_running(cancellation)?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Deferred)
                .map_err(ManualDatabaseError::from)
                .map_err(|source| manual_command_database_error(source, cancellation))?;
            validate_schema(&transaction)
                .map_err(|source| manual_command_database_error(source, cancellation))?;
            let snapshot = load_snapshot(&transaction)
                .map_err(|source| manual_command_database_error(source, cancellation))?;
            ensure_manual_command_running(cancellation)?;
            transaction
                .commit()
                .map_err(ManualDatabaseError::from)
                .map_err(|source| manual_command_database_error(source, cancellation))?;
            ensure_manual_command_running(cancellation)?;
            execute_manual_read_operation(
                operation,
                file,
                export_selection,
                &snapshot,
                cancellation,
            )
        }
        ManualOperation::Apply => {
            assert!(export_selection.is_none(), "Manual apply 不得获得导出选择");
            let mut connection = open_read_write(database_path, cancellation)
                .map_err(|source| manual_command_database_error(source, cancellation))?;
            ensure_manual_command_running(cancellation)?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(ManualDatabaseError::from)
                .map_err(|source| manual_command_database_error(source, cancellation))?;
            validate_schema(&transaction)
                .map_err(|source| manual_command_database_error(source, cancellation))?;
            let snapshot = load_snapshot(&transaction)
                .map_err(|source| manual_command_database_error(source, cancellation))?;
            ensure_manual_command_running(cancellation)?;
            let summary = apply_manual_snapshot(file, &snapshot, cancellation, |writes| {
                apply(&transaction, writes)
            })?;
            ensure_manual_command_running(cancellation)?;
            transaction
                .commit()
                .map_err(ManualDatabaseError::from)
                .map_err(|source| manual_command_database_error(source, cancellation))?;
            Ok(summary)
        }
    }
}

fn execute_manual_read_operation(
    operation: ManualOperation,
    file: &Path,
    export_selection: Option<&ManualExportSelection>,
    snapshot: &ManualProjectSnapshot,
    cancellation: &CooperativeCancellation,
) -> Result<ManualCommandSummary, ManualCommandError> {
    match operation {
        ManualOperation::Export => {
            let selection = export_selection.expect("Manual export 必须获得选择");
            let exported = export_manual_document_with_cancellation(
                file,
                &snapshot.index,
                selection,
                cancellation,
            );
            exported
                .map(|entries| ManualCommandSummary::Exported {
                    entries,
                    file: file.to_path_buf(),
                })
                .map_err(ManualCommandError::from_document)
        }
        ManualOperation::OwnershipExport => {
            assert!(
                export_selection.is_none(),
                "Ownership export 不得获得 Manual 选择"
            );
            export_ownership_document_with_cancellation(file, &snapshot.index, cancellation)
                .map(|entries| ManualCommandSummary::Exported {
                    entries,
                    file: file.to_path_buf(),
                })
                .map_err(ManualCommandError::from_document)
        }
        ManualOperation::TranslationExport => {
            assert!(
                export_selection.is_none(),
                "Translation export 不得获得 Manual 选择"
            );
            export_translation_document_with_cancellation(file, &snapshot.index, cancellation)
                .map(|entries| ManualCommandSummary::Exported {
                    entries,
                    file: file.to_path_buf(),
                })
                .map_err(ManualCommandError::from_document)
        }
        ManualOperation::Check => {
            assert!(export_selection.is_none(), "Manual check 不得获得导出选择");
            let report = check_manual_snapshot(file, snapshot, cancellation)?;
            if report.is_valid() {
                Ok(ManualCommandSummary::Checked { report })
            } else {
                Err(ManualCommandError::InvalidEntries(report))
            }
        }
        ManualOperation::Apply => {
            unreachable!("apply 必须在写事务中执行")
        }
    }
}

fn check_manual_snapshot(
    file: &Path,
    snapshot: &ManualProjectSnapshot,
    cancellation: &CooperativeCancellation,
) -> Result<ManualCheckReport, ManualCommandError> {
    check_manual_document_with_cancellation(
        file,
        &snapshot.index,
        cancellation,
        |entry, translation| snapshot.placeholders.validate(entry, translation),
    )
    .map_err(ManualCommandError::from_document)
}

fn apply_manual_snapshot(
    file: &Path,
    snapshot: &ManualProjectSnapshot,
    cancellation: &CooperativeCancellation,
    apply: impl FnOnce(&[ValidatedManualTranslation]) -> Result<usize, ManualDatabaseError>,
) -> Result<ManualCommandSummary, ManualCommandError> {
    let report = check_manual_snapshot(file, snapshot, cancellation)?;
    if !report.is_valid() {
        return Err(ManualCommandError::InvalidEntries(report));
    }
    ensure_manual_command_running(cancellation)?;
    let applied = apply(&report.writes)
        .map_err(|source| manual_command_database_error(source, cancellation))?;
    ensure_manual_command_running(cancellation)?;
    Ok(ManualCommandSummary::Applied { report, applied })
}

#[derive(Debug)]
pub(crate) enum ManualCommandError {
    Cancelled,
    Document(ManualDocumentError),
    Database(ManualDatabaseError),
    InvalidEntries(ManualCheckReport),
}

impl fmt::Display for ManualCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("人工补译操作已取消"),
            Self::Document(source) => source.fmt(formatter),
            Self::Database(source) => source.fmt(formatter),
            Self::InvalidEntries(report) => write!(
                formatter,
                "人工译文检查发现 {} 个错误；请按位置修正后重试",
                report.errors.len()
            ),
        }
    }
}

impl Error for ManualCommandError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Document(source) => Some(source),
            Self::Database(source) => Some(source),
            Self::Cancelled | Self::InvalidEntries(_) => None,
        }
    }
}

impl ManualCommandError {
    pub(crate) const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    fn from_document(source: ManualDocumentError) -> Self {
        if matches!(source, ManualDocumentError::Cancelled) {
            Self::Cancelled
        } else {
            Self::Document(source)
        }
    }

    /// 为 CLI 与项目 JSONL 建立同一份安全、类型化 Manual 失败事实。
    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        match self {
            Self::Document(source) => manual_document_diagnostic_report(source),
            Self::Cancelled => manual_fallback_diagnostic_report(StateEffect::ProgressPreserved),
            Self::Database(_) | Self::InvalidEntries(_) => {
                manual_fallback_diagnostic_report(StateEffect::Unchanged)
            }
        }
    }
}

fn manual_fallback_diagnostic_report(effect: StateEffect) -> DiagnosticReport {
    DiagnosticReport::new(
        effect,
        Diagnostic::runtime(RuntimeIssue::InvalidConfiguration {
            component: RuntimeComponent::Process,
        }),
    )
}

fn manual_file_system_diagnostic(
    operation: FileSystemOperation,
    problem: FileSystemProblem,
) -> Diagnostic {
    Diagnostic::file_system(FileSystemIssue::new(
        FileSystemDiagnosticContext::new(FileSystemDiagnosticStage::CommandPreparation, operation),
        problem,
    ))
}

fn manual_io_diagnostic(
    path: &Path,
    operation: FileSystemOperation,
    source: &io::Error,
) -> Diagnostic {
    manual_file_system_diagnostic(
        operation,
        FileSystemProblem::Io {
            path: SafePath::new(path),
            failure: IoFailure::from_error(source),
        },
    )
}

fn manual_output_target_diagnostic(problem: &ManualOutputTargetProblem) -> Diagnostic {
    let problem = match problem {
        ManualOutputTargetProblem::NotRegularFile { path } => FileSystemProblem::InvalidPath {
            path: SafePath::new(path),
            violation: FileSystemPathViolation::NotRegularFile,
        },
        ManualOutputTargetProblem::MissingFileName { path } => FileSystemProblem::InvalidPath {
            path: SafePath::new(path),
            violation: FileSystemPathViolation::MissingFileName,
        },
        ManualOutputTargetProblem::ReparsePoint { path } => FileSystemProblem::ReparsePoint {
            path: SafePath::new(path),
        },
        ManualOutputTargetProblem::NonLocalVolume { path } => FileSystemProblem::NonLocalVolume {
            path: SafePath::new(path),
        },
        ManualOutputTargetProblem::NonNtfsVolume { path, actual } => {
            FileSystemProblem::NonNtfsVolume {
                path: SafePath::new(path),
                actual: SafeText::new(actual),
            }
        }
        ManualOutputTargetProblem::CaseSensitiveDirectory { path } => {
            FileSystemProblem::CaseSensitiveDirectory {
                path: SafePath::new(path),
            }
        }
        ManualOutputTargetProblem::BindingCancelled { path } => FileSystemProblem::Cancelled {
            path: SafePath::new(path),
        },
        ManualOutputTargetProblem::TargetAlreadyExists { path } => {
            FileSystemProblem::TargetExists {
                path: SafePath::new(path),
            }
        }
        ManualOutputTargetProblem::IdentityChanged { path } => FileSystemProblem::IdentityChanged {
            path: SafePath::new(path),
        },
        ManualOutputTargetProblem::BindingIo { path, failure } => FileSystemProblem::Io {
            path: SafePath::new(path),
            failure: failure.clone(),
        },
    };
    manual_file_system_diagnostic(FileSystemOperation::Write, problem)
}

fn manual_recovery_artifact_diagnostic(path: &Path) -> Diagnostic {
    manual_file_system_diagnostic(
        FileSystemOperation::RecoverTarget,
        FileSystemProblem::RecoveryRequired {
            target_root: SafePath::new(path),
            artifacts: vec![SafePath::new(path)],
            violation: FileSystemRecoveryViolation::UnexpectedResidualArtifact,
        },
    )
}

fn manual_replace_outcome_unknown_diagnostic(target: &Path, temporary: &Path) -> Diagnostic {
    manual_file_system_diagnostic(
        FileSystemOperation::Write,
        FileSystemProblem::OutcomeUnknown {
            target_root: SafePath::new(target),
            artifacts: vec![SafePath::new(temporary)],
            violation: FileSystemRecoveryViolation::TargetIdentityUnknown,
        },
    )
}

fn manual_recovery_io_diagnostic(
    path: &Path,
    operation: FileSystemOperation,
    source: &io::Error,
) -> Diagnostic {
    manual_file_system_diagnostic(
        operation,
        FileSystemProblem::RecoveryArtifactIo {
            path: SafePath::new(path),
            failure: IoFailure::from_error(source),
        },
    )
}

fn manual_document_diagnostic_report(source: &ManualDocumentError) -> DiagnosticReport {
    match source {
        ManualDocumentError::Cancelled => DiagnosticReport::new(
            StateEffect::ProgressPreserved,
            Diagnostic::runtime(RuntimeIssue::Cancelled {
                component: RuntimeComponent::Process,
                operation: crate::diagnostic::RuntimeOperation::ExecuteTask,
            }),
        ),
        ManualDocumentError::Read { path, source } => DiagnosticReport::new(
            StateEffect::Unchanged,
            manual_io_diagnostic(path, FileSystemOperation::Read, source),
        ),
        ManualDocumentError::Write { path, source } => DiagnosticReport::new(
            StateEffect::Unchanged,
            manual_io_diagnostic(path, FileSystemOperation::Write, source),
        ),
        ManualDocumentError::OutputTarget { problem } => DiagnosticReport::new(
            StateEffect::Unchanged,
            manual_output_target_diagnostic(problem),
        ),
        ManualDocumentError::ExistingTemporary { path } => DiagnosticReport::new(
            StateEffect::RecoveryRequired,
            manual_recovery_artifact_diagnostic(path),
        ),
        ManualDocumentError::ReplaceOutcomeUnknown { target, temporary } => DiagnosticReport::new(
            StateEffect::OutcomeUnknown,
            manual_replace_outcome_unknown_diagnostic(target, temporary),
        ),
        ManualDocumentError::TemporaryCleanup {
            operation,
            temporary,
            cleanup,
        } => manual_document_diagnostic_report(operation)
            .with_effect(StateEffect::RecoveryRequired)
            .with_related(
                RelatedFailureRelation::Cleanup,
                DiagnosticReport::new(
                    StateEffect::RecoveryRequired,
                    manual_recovery_io_diagnostic(temporary, FileSystemOperation::Remove, cleanup),
                ),
            ),
        ManualDocumentError::InvalidUtf8 { .. }
        | ManualDocumentError::InvalidToml { .. }
        | ManualDocumentError::InvalidIds { .. }
        | ManualDocumentError::Encode(_) => {
            manual_fallback_diagnostic_report(StateEffect::Unchanged)
        }
    }
}

fn manual_command_database_error(
    source: ManualDatabaseError,
    _cancellation: &CooperativeCancellation,
) -> ManualCommandError {
    let cancelled = matches!(&source, ManualDatabaseError::Cancelled)
        || matches!(
            &source,
            ManualDatabaseError::Sqlite(source)
                if source.sqlite_error_code()
                    == Some(rusqlite::ErrorCode::OperationInterrupted)
        );
    if cancelled {
        ManualCommandError::Cancelled
    } else {
        ManualCommandError::Database(source)
    }
}

fn ensure_manual_command_running(
    cancellation: &CooperativeCancellation,
) -> Result<(), ManualCommandError> {
    if cancellation.is_requested() {
        Err(ManualCommandError::Cancelled)
    } else {
        Ok(())
    }
}

pub(crate) fn render_manual_command_summary(
    summary: &ManualCommandSummary,
    localizer: &UiLocalizer,
    stdout: &mut dyn Write,
) -> io::Result<()> {
    let messages = match summary {
        ManualCommandSummary::Exported { entries, file } => {
            let path = public_path(file);
            vec![localizer.format(UiMessage::ManualExported {
                entries: manual_count(*entries),
                path: &path,
            })]
        }
        ManualCommandSummary::Checked { report } => {
            vec![localizer.format(UiMessage::ManualChecked {
                valid: manual_count(report.valid),
                unfilled: manual_count(report.unfilled),
                errors: manual_count(report.errors.len()),
            })]
        }
        ManualCommandSummary::Applied { report, applied } => {
            vec![localizer.format(UiMessage::ManualApplied {
                applied: manual_count(*applied),
                unfilled: manual_count(report.unfilled),
                errors: manual_count(report.errors.len()),
            })]
        }
    };
    for message in messages {
        writeln!(stdout, "{message}")?;
    }
    Ok(())
}

pub(crate) fn render_manual_command_error(
    error: &ManualCommandError,
    localizer: &UiLocalizer,
    stderr: &mut dyn Write,
) -> io::Result<()> {
    match error {
        ManualCommandError::Cancelled => Ok(()),
        ManualCommandError::InvalidEntries(report) => {
            writeln!(
                stderr,
                "{}",
                localizer.format(UiMessage::ManualChecked {
                    valid: manual_count(report.valid),
                    unfilled: manual_count(report.unfilled),
                    errors: manual_count(report.errors.len()),
                })
            )?;
            for issue in &report.errors {
                let (reason, help) = render_manual_check_problem(&issue.problem, localizer);
                render_manual_issue(&issue.id, &reason, &help, localizer, stderr)?;
            }
            Ok(())
        }
        ManualCommandError::Document(source) => {
            writeln!(
                stderr,
                "{}",
                localizer.format(UiMessage::ManualChecked {
                    valid: 0,
                    unfilled: 0,
                    errors: manual_count(manual_document_issue_count(source)),
                })
            )?;
            if matches!(
                source,
                ManualDocumentError::OutputTarget { .. }
                    | ManualDocumentError::ExistingTemporary { .. }
                    | ManualDocumentError::ReplaceOutcomeUnknown { .. }
                    | ManualDocumentError::TemporaryCleanup { .. }
            ) {
                writeln!(
                    stderr,
                    "{}",
                    localizer.format(UiMessage::DiagnosticErrorHeading)
                )?;
                writeln!(
                    stderr,
                    "{}",
                    render_diagnostic_report(&error.diagnostic_report(), localizer)
                )
            } else {
                render_manual_document_issues(source, localizer, stderr)
            }
        }
        ManualCommandError::Database(source) => {
            writeln!(
                stderr,
                "{}",
                localizer.format(UiMessage::ManualChecked {
                    valid: 0,
                    unfilled: 0,
                    errors: 1,
                })
            )?;
            let (reason_code, help_code) = manual_database_issue(source);
            let reason = render_manual_value(localizer, reason_code, 0, 0, 0);
            let help = render_manual_value(localizer, help_code, 0, 0, 0);
            render_manual_issue("project.db", &reason, &help, localizer, stderr)
        }
    }
}

fn manual_document_issue_count(source: &ManualDocumentError) -> usize {
    match source {
        ManualDocumentError::TemporaryCleanup { operation, .. } => {
            manual_document_issue_count(operation).saturating_add(1)
        }
        ManualDocumentError::Cancelled
        | ManualDocumentError::Read { .. }
        | ManualDocumentError::InvalidUtf8 { .. }
        | ManualDocumentError::InvalidToml { .. }
        | ManualDocumentError::InvalidIds { .. }
        | ManualDocumentError::Encode(_)
        | ManualDocumentError::Write { .. }
        | ManualDocumentError::OutputTarget { .. }
        | ManualDocumentError::ExistingTemporary { .. }
        | ManualDocumentError::ReplaceOutcomeUnknown { .. } => 1,
    }
}

fn render_manual_document_issues(
    source: &ManualDocumentError,
    localizer: &UiLocalizer,
    stderr: &mut dyn Write,
) -> io::Result<()> {
    render_manual_document_issues_with_effect(source, StateEffect::Unchanged, localizer, stderr)
}

fn render_manual_document_issues_with_effect(
    source: &ManualDocumentError,
    primary_effect: StateEffect,
    localizer: &UiLocalizer,
    stderr: &mut dyn Write,
) -> io::Result<()> {
    if let ManualDocumentError::TemporaryCleanup {
        operation,
        temporary,
        cleanup,
    } = source
    {
        render_manual_document_issues_with_effect(operation, primary_effect, localizer, stderr)?;
        let reason = manual_temporary_cleanup_reason(cleanup, localizer);
        let help = render_manual_value(localizer, "resolve_temporary_then_rerun_export", 0, 0, 0);
        writeln!(stderr)?;
        writeln!(
            stderr,
            "{}",
            localizer.format(UiMessage::DiagnosticRelated {
                relation: RelatedFailureRelation::Cleanup.as_str(),
            })
        )?;
        return render_manual_issue_body(
            &public_path(temporary),
            &reason,
            &help,
            StateEffect::RecoveryRequired,
            localizer,
            stderr,
        );
    }
    if let ManualDocumentError::OutputTarget { problem } = source {
        let reason = localizer.format(UiMessage::DiagnosticFailureValue {
            code: problem.reason_code(),
        });
        let help = localizer.format(UiMessage::DiagnosticResolutionValue {
            code: problem.help_code(),
        });
        writeln!(
            stderr,
            "{}",
            localizer.format(UiMessage::DiagnosticErrorHeading)
        )?;
        return render_manual_issue_body(
            &problem.object(),
            &reason,
            &help,
            primary_effect,
            localizer,
            stderr,
        );
    }

    let (object, reason_code, help_code) = manual_document_issue(source);
    let reason = render_manual_value(localizer, reason_code, 0, 0, 0);
    let help = render_manual_value(localizer, help_code, 0, 0, 0);
    writeln!(
        stderr,
        "{}",
        localizer.format(UiMessage::DiagnosticErrorHeading)
    )?;
    render_manual_issue_body(
        &object,
        &reason,
        &help,
        match source {
            ManualDocumentError::ExistingTemporary { .. } => StateEffect::RecoveryRequired,
            ManualDocumentError::ReplaceOutcomeUnknown { .. } => StateEffect::OutcomeUnknown,
            _ => primary_effect,
        },
        localizer,
        stderr,
    )
}

fn manual_temporary_cleanup_reason(cleanup: &io::Error, localizer: &UiLocalizer) -> String {
    let failure = IoFailure::from_error(cleanup);
    localizer.format(UiMessage::DiagnosticFailureValue {
        code: failure.summary_code(),
    })
}

fn render_manual_issue(
    object: &str,
    reason: &str,
    help: &str,
    localizer: &UiLocalizer,
    output: &mut dyn Write,
) -> io::Result<()> {
    writeln!(
        output,
        "{}",
        localizer.format(UiMessage::DiagnosticErrorHeading)
    )?;
    render_manual_issue_body(
        object,
        reason,
        help,
        StateEffect::Unchanged,
        localizer,
        output,
    )
}

fn render_manual_issue_body(
    object: &str,
    reason: &str,
    help: &str,
    effect: StateEffect,
    localizer: &UiLocalizer,
    output: &mut dyn Write,
) -> io::Result<()> {
    let impact = render_state_effect_impact(effect, localizer);
    writeln!(
        output,
        "{}",
        localizer.format(UiMessage::DiagnosticObject { subject: object })
    )?;
    writeln!(
        output,
        "{}",
        localizer.format(UiMessage::DiagnosticExplanation { reason })
    )?;
    writeln!(
        output,
        "{}",
        localizer.format(UiMessage::DiagnosticImpact { impact: &impact })
    )?;
    writeln!(
        output,
        "{}",
        localizer.format(UiMessage::DiagnosticResolution { action: help })
    )
}

fn render_manual_check_problem(
    problem: &ManualCheckProblem,
    localizer: &UiLocalizer,
) -> (String, String) {
    let (reason, help, line, expected, actual) = match problem {
        ManualCheckProblem::DuplicateEntry => ("duplicate_entry", "keep_one_entry", 0, 0, 0),
        ManualCheckProblem::UnknownId => ("unknown_id", "rerun_export", 0, 0, 0),
        ManualCheckProblem::InvalidSourceLine { line } => (
            "invalid_source_line",
            "rerun_export_without_controls",
            manual_count(*line),
            0,
            0,
        ),
        ManualCheckProblem::SourceChanged => ("source_changed", "rerun_export_then_fill", 0, 0, 0),
        ManualCheckProblem::TypeMismatch => ("type_mismatch", "keep_exported_type", 0, 0, 0),
        ManualCheckProblem::InvalidTranslationLine { line } => (
            "invalid_translation_line",
            "use_array_lines",
            manual_count(*line),
            0,
            0,
        ),
        ManualCheckProblem::FixedLength { expected, actual } => (
            "fixed_length",
            "keep_array_length",
            0,
            manual_count(*expected),
            manual_count(*actual),
        ),
        ManualCheckProblem::FixedBlankSlot { slot } => (
            "fixed_blank_slot",
            "keep_blank_slot",
            manual_count(*slot),
            0,
            0,
        ),
        ManualCheckProblem::PlaceholderMismatch => {
            ("placeholder_mismatch", "keep_placeholders", 0, 0, 0)
        }
        ManualCheckProblem::EmptyTranslation => {
            ("empty_translation", "provide_translation", 0, 0, 0)
        }
        ManualCheckProblem::InvalidStructure => {
            ("invalid_structure", "fix_translation_structure", 0, 0, 0)
        }
    };
    (
        render_manual_value(localizer, reason, line, expected, actual),
        render_manual_value(localizer, help, line, expected, actual),
    )
}

fn render_manual_value(
    localizer: &UiLocalizer,
    code: &str,
    line: u64,
    expected: u64,
    actual: u64,
) -> String {
    let failure = |code| localizer.format(UiMessage::DiagnosticFailureValue { code });
    let resolution = |code| localizer.format(UiMessage::DiagnosticResolutionValue { code });
    match code {
        "invalid_source_line"
        | "invalid_translation_line"
        | "fixed_length"
        | "fixed_blank_slot"
        | "rerun_export"
        | "rerun_export_without_controls"
        | "rerun_export_then_fill"
        | "resolve_temporary_then_rerun_export"
        | "resolve_published_backup_cleanup"
        | "keep_exported_type" => localizer.format(UiMessage::ManualValue {
            code,
            line,
            expected,
            actual,
        }),
        "duplicate_entry" | "duplicate_readable_id" => failure("duplicate_identifier"),
        "unknown_id" => failure("not_found"),
        "source_changed" => failure("source_snapshot_mismatch"),
        "type_mismatch" | "invalid_structure" | "invalid_project" => failure("invalid_value"),
        "placeholder_mismatch" => failure("placeholder_projection_failed"),
        "empty_translation" => failure("missing_required_value"),
        "cancelled" => failure("cancelled"),
        "document_read" | "document_write" | "database_access" => failure("operation_failed"),
        "publication_artifact_exists" => failure("recovery_required"),
        "transaction_outcome_unknown" => failure("transaction_outcome_unknown"),
        "document_invalid_utf8" => failure("invalid_encoding"),
        "document_invalid_toml" => failure("invalid_syntax"),
        "document_encode" => failure("internal_invariant"),
        "keep_placeholders" => resolution("fix_placeholder_rules"),
        "check_read_access" | "check_write_access" | "check_database_access" => {
            resolution("check_path_and_permissions")
        }
        "fix_project_then_export" => resolution("check_project_state"),
        "retry_if_needed" | "retry_or_report" => resolution("retry"),
        "preserve_recovery_artifacts" => resolution("preserve_recovery_artifacts"),
        "keep_one_entry"
        | "use_array_lines"
        | "keep_array_length"
        | "keep_blank_slot"
        | "provide_translation"
        | "fix_translation_structure"
        | "fix_toml_contract"
        | "save_as_utf8"
        | "fix_extract_then_export" => resolution("fix_input"),
        _ => failure("invalid_value"),
    }
}

fn manual_count(value: usize) -> u64 {
    u64::try_from(value).expect("当前支持平台的 Manual 计数必须能用 u64 表达")
}

fn manual_document_issue(source: &ManualDocumentError) -> (String, &'static str, &'static str) {
    match source {
        ManualDocumentError::Cancelled => ("Manual".to_owned(), "cancelled", "retry_if_needed"),
        ManualDocumentError::Read { path, .. } => {
            (public_path(path), "document_read", "check_read_access")
        }
        ManualDocumentError::InvalidUtf8 { path } => {
            (public_path(path), "document_invalid_utf8", "save_as_utf8")
        }
        ManualDocumentError::InvalidToml { path, .. } => (
            public_path(path),
            "document_invalid_toml",
            "fix_toml_contract",
        ),
        ManualDocumentError::InvalidIds { path, .. } => (
            public_path(path),
            "invalid_structure",
            "fix_translation_structure",
        ),
        ManualDocumentError::Encode(_) => (
            "Manual TOML".to_owned(),
            "document_encode",
            "retry_or_report",
        ),
        ManualDocumentError::Write { path, .. } => {
            (public_path(path), "document_write", "check_write_access")
        }
        ManualDocumentError::OutputTarget { .. } => {
            unreachable!("输出目标问题由专用的类型化诊断分支呈现")
        }
        ManualDocumentError::ExistingTemporary { path } => (
            public_path(path),
            "publication_artifact_exists",
            "resolve_temporary_then_rerun_export",
        ),
        ManualDocumentError::ReplaceOutcomeUnknown { target, .. } => (
            public_path(target),
            "transaction_outcome_unknown",
            "preserve_recovery_artifacts",
        ),
        ManualDocumentError::TemporaryCleanup { operation, .. } => manual_document_issue(operation),
    }
}

fn manual_database_issue(source: &ManualDatabaseError) -> (&'static str, &'static str) {
    match source {
        ManualDatabaseError::Cancelled => ("cancelled", "retry_if_needed"),
        ManualDatabaseError::Sqlite(_) => ("database_access", "check_database_access"),
        ManualDatabaseError::InvalidProject(_) => ("invalid_project", "fix_project_then_export"),
        ManualDatabaseError::Index(_) => ("duplicate_readable_id", "fix_extract_then_export"),
    }
}

#[derive(Debug)]
pub(crate) enum ManualDatabaseError {
    Cancelled,
    Sqlite(rusqlite::Error),
    InvalidProject(String),
    Index(ManualIndexError),
}

impl fmt::Display for ManualDatabaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("人工补译操作已取消"),
            Self::Sqlite(_) => formatter.write_str("无法读取或修改项目数据库"),
            Self::InvalidProject(reason) => formatter.write_str(reason),
            Self::Index(source) => source.fmt(formatter),
        }
    }
}

impl Error for ManualDatabaseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(source) => Some(source),
            Self::Index(source) => Some(source),
            Self::Cancelled | Self::InvalidProject(_) => None,
        }
    }
}

impl From<rusqlite::Error> for ManualDatabaseError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Sqlite(source)
    }
}

impl From<ManualIndexError> for ManualDatabaseError {
    fn from(source: ManualIndexError) -> Self {
        Self::Index(source)
    }
}

fn load_generic_source_language(
    connection: &Connection,
    language_modules: &LanguageModuleCatalog,
) -> Result<Arc<dyn LanguageModule>, ManualDatabaseError> {
    let source: String = connection.query_row(
        "SELECT source_language FROM generic_project WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    let source = parse_project_language(&source, "源语言")?;
    resolve_source_language(language_modules, &source)
}

fn load_rpg_maker_language_context(
    connection: &Connection,
    language_modules: &LanguageModuleCatalog,
) -> Result<(LanguagePair, Arc<dyn LanguageModule>), ManualDatabaseError> {
    let (source, target): (String, String) = connection.query_row(
        "SELECT source_language, target_language FROM metadata",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let source = parse_project_language(&source, "源语言")?;
    let target = parse_project_language(&target, "目标语言")?;
    let module = resolve_source_language(language_modules, &source)?;
    Ok((LanguagePair::new(source, target), module))
}

fn parse_project_language(value: &str, field: &str) -> Result<LanguageId, ManualDatabaseError> {
    LanguageId::parse(value)
        .map_err(|_| ManualDatabaseError::InvalidProject(format!("项目{field}无效")))
}

fn resolve_source_language(
    language_modules: &LanguageModuleCatalog,
    source: &LanguageId,
) -> Result<Arc<dyn LanguageModule>, ManualDatabaseError> {
    language_modules.resolve(source).map_err(|_| {
        ManualDatabaseError::InvalidProject(format!(
            "配置中没有项目源语言 {} 的语言模块；请在 languages 中添加该语言",
            source.as_str()
        ))
    })
}

fn load_generic_manual_command_snapshot(
    connection: &Connection,
    language_modules: Option<&LanguageModuleCatalog>,
) -> Result<ManualProjectSnapshot, ManualDatabaseError> {
    let source_language = language_modules
        .map(|modules| load_generic_source_language(connection, modules))
        .transpose()?;
    let canonical_json: String = connection.query_row(
        "SELECT canonical_json FROM translation_resource WHERE resource_kind = 'placeholder_rules'",
        [],
        |row| row.get(0),
    )?;
    let service = GenericPlaceholderService::default();
    let definitions = service
        .parse_canonical_json_with_cancellation(&canonical_json, || {
            Ok::<_, std::convert::Infallible>(())
        })
        .map_err(|unreachable| match unreachable {})
        .and_then(|result| {
            result.map_err(|_| {
                ManualDatabaseError::InvalidProject("项目 Placeholder 规则无效".to_owned())
            })
        })?;
    let valid_ids = load_generic_manual_unit_ids(connection)?;
    let compiled = service
        .compile_for_ids_with_cancellation(definitions, &valid_ids, || {
            Ok::<_, std::convert::Infallible>(())
        })
        .map_err(|unreachable| match unreachable {})
        .and_then(|result| {
            result.map_err(|_| {
                ManualDatabaseError::InvalidProject("项目 Placeholder 规则无法编译".to_owned())
            })
        })?;
    let entries =
        load_generic_entries(connection, &service, &compiled, source_language.as_deref())?;
    Ok(ManualProjectSnapshot {
        index: ManualTranslationIndex::new(entries)?,
        placeholders: ManualPlaceholderValidator::Generic { service, compiled },
    })
}

fn load_generic_manual_unit_ids(
    connection: &Connection,
) -> Result<HashSet<String>, ManualDatabaseError> {
    let mut statement = connection.prepare(
        "SELECT f.relative_path, g.ordinal, u.ordinal
         FROM generic_file AS f
         JOIN generic_group AS g ON g.relative_path = f.relative_path
         JOIN generic_unit AS u ON u.group_id = g.group_id
         ORDER BY f.ordinal, g.ordinal, u.ordinal",
    )?;
    let mut rows = statement.query([])?;
    let mut ids = HashSet::new();
    while let Some(row) = rows.next()? {
        let relative_path = decode_windows_path(&row.get::<_, Vec<u8>>(0)?)?;
        let line = natural_generic_ordinal(row.get(1)?, "行号")?;
        let unit = natural_generic_ordinal(row.get(2)?, "Unit 序号")?;
        let id = crate::generic::readable_generic_unit_id(&relative_path, line, unit);
        if !ids.insert(id) {
            return Err(ManualDatabaseError::InvalidProject(
                "Generic Extract 产生了重复自然 ID".to_owned(),
            ));
        }
    }
    Ok(ids)
}

fn natural_generic_ordinal(value: i64, label: &str) -> Result<usize, ManualDatabaseError> {
    usize::try_from(value)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| ManualDatabaseError::InvalidProject(format!("Generic {label}无效")))
}

pub(crate) fn load_generic_manual_lua_snapshot(
    connection: &Connection,
    language_modules: &LanguageModuleCatalog,
) -> Result<ManualProjectLuaSnapshot, ManualDatabaseError> {
    Ok(ManualProjectLuaSnapshot {
        current: load_generic_manual_command_snapshot(connection, Some(language_modules))?,
        detached: load_detached_generic_manual_translations(connection)?,
    })
}

fn load_generic_entries(
    connection: &Connection,
    placeholder_service: &GenericPlaceholderService,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    source_language: Option<&dyn LanguageModule>,
) -> Result<Vec<ManualTranslationEntry>, ManualDatabaseError> {
    let mut statement = connection.prepare(
        "SELECT f.relative_path, g.group_id, g.ordinal, g.kind, g.context_fingerprint,
                u.unit_id, u.ordinal, u.source_text, u.translation, u.translation_state,
                manual.readable_id, manual.source_json, manual.translation_json,
                manual.applicability_fingerprint,
                rejected.readable_id, rejected.origin, rejected.source_json,
                rejected.candidate_json, rejected.violation_json,
                rejected.group_context, rejected.planning_state,
                project.source_language, project.target_language
         FROM generic_file AS f
         CROSS JOIN generic_project AS project
         JOIN generic_group AS g ON g.relative_path = f.relative_path
         JOIN generic_unit AS u ON u.group_id = g.group_id
         LEFT JOIN generic_manual_translation AS manual
           ON manual.group_id = u.group_id AND manual.unit_id = u.unit_id
         LEFT JOIN generic_rejected_translation AS rejected
           ON rejected.group_id = u.group_id AND rejected.unit_id = u.unit_id
         ORDER BY f.ordinal, g.ordinal, u.ordinal",
    )?;
    let mut rows = statement.query([])?;
    let mut entries = Vec::new();
    while let Some(row) = rows.next()? {
        let relative_path_bytes: Vec<u8> = row.get(0)?;
        let relative_path = decode_windows_path(&relative_path_bytes)?;
        let group_id: String = row.get(1)?;
        let line: i64 = row.get(2)?;
        let kind: String = row.get(3)?;
        let group_context =
            Sha256Fingerprint::from_slice(&row.get::<_, Vec<u8>>(4)?).map_err(|_| {
                ManualDatabaseError::InvalidProject("Generic Group 语境指纹长度无效".to_owned())
            })?;
        let unit_id: String = row.get(5)?;
        let unit: i64 = row.get(6)?;
        let source_text: String = row.get(7)?;
        let automatic: Option<String> = row.get(8)?;
        let automatic_state: Option<Vec<u8>> = row.get(9)?;
        let project_source_language: String = row.get(21)?;
        let project_target_language: String = row.get(22)?;
        let expected_automatic_applicability =
            crate::translation::generic_automatic_applicability_v2(
                &project_source_language,
                &project_target_language,
                &group_id,
                &unit_id,
                &source_text,
                group_context,
            );
        let automatic = match (automatic, automatic_state) {
            (None, None) => None,
            (Some(translation), Some(state)) => {
                let state = Sha256Fingerprint::from_slice(&state).map_err(|_| {
                    ManualDatabaseError::InvalidProject("Generic 自动译文状态长度无效".to_owned())
                })?;
                crate::translation::generic_automatic_applicability_is_current(
                    state,
                    expected_automatic_applicability,
                )
                .then_some(translation)
            }
            _ => {
                return Err(ManualDatabaseError::InvalidProject(
                    "Generic 自动译文正文与状态不完整".to_owned(),
                ));
            }
        };
        let stored_manual = parse_stored_generic_manual_translation(
            row.get(10)?,
            row.get(11)?,
            row.get(12)?,
            row.get(13)?,
        )?;
        let rejected_row = (
            row.get::<_, Option<String>>(14)?,
            row.get::<_, Option<String>>(15)?,
            row.get::<_, Option<String>>(16)?,
            row.get::<_, Option<String>>(17)?,
            row.get::<_, Option<String>>(18)?,
            row.get::<_, Option<Vec<u8>>>(19)?,
            row.get::<_, Option<Vec<u8>>>(20)?,
        );
        let source = source_text
            .split('\n')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let readable_path = relative_path.to_string_lossy().replace('\\', "/");
        let id = crate::generic::readable_generic_unit_id(
            &relative_path,
            natural_generic_ordinal(line, "行号")?,
            natural_generic_ordinal(unit, "Unit 序号")?,
        );
        let automatic = automatic.filter(|translation| {
            validate_translation_placeholders_with_cancellation(
                placeholder_service,
                placeholder_rules,
                &id,
                &kind,
                &source_text,
                translation,
                || Ok::<_, std::convert::Infallible>(()),
            )
            .unwrap_or_else(|unreachable| match unreachable {})
            .is_ok()
        });
        let rejected = match rejected_row {
            (None, None, None, None, None, None, None) => None,
            (
                Some(_readable_id),
                Some(origin),
                Some(rejected_source_json),
                Some(candidate_json),
                Some(violation_json),
                Some(rejected_group_context),
                Some(rejected_planning_state),
            ) => {
                let rejected_source = serde_json::from_str::<Vec<String>>(&rejected_source_json)
                    .map_err(|_| {
                        ManualDatabaseError::InvalidProject(format!("{id} 的 Rejected 原文无效"))
                    })?;
                let rejected_group_context = Sha256Fingerprint::from_slice(&rejected_group_context)
                    .map_err(|_| {
                        ManualDatabaseError::InvalidProject(format!(
                            "{id} 的 Rejected Group 语境指纹长度无效"
                        ))
                    })?;
                let rejected_planning_state =
                    Sha256Fingerprint::from_slice(&rejected_planning_state).map_err(|_| {
                        ManualDatabaseError::InvalidProject(format!(
                            "{id} 的 Rejected 适用状态长度无效"
                        ))
                    })?;
                if rejected_source != source
                    || rejected_group_context != group_context
                    || !crate::translation::generic_automatic_applicability_is_current(
                        rejected_planning_state,
                        expected_automatic_applicability,
                    )
                {
                    None
                } else {
                    Some(parse_manual_rejected_candidate(
                        &id,
                        parse_manual_translation_origin(&id, &origin)?,
                        candidate_json,
                        None,
                        violation_json,
                    )?)
                }
            }
            _ => {
                return Err(ManualDatabaseError::InvalidProject(format!(
                    "{id} 的 Rejected 记录不完整"
                )));
            }
        };
        let applicability = generic_manual_applicability(
            &group_id,
            &unit_id,
            &readable_path,
            &kind,
            &project_source_language,
            &project_target_language,
            &source,
        );
        let current_manual = stored_manual
            .as_ref()
            .filter(|manual| manual.applicability == applicability);
        let outdated_manual = stored_manual
            .as_ref()
            .filter(|manual| manual.applicability != applicability)
            .map(manual_outdated_snapshot);
        let (current_translation, origin) = if let Some(manual) = current_manual {
            (
                Some(manual.translation.clone()),
                Some(ManualTranslationOrigin::Manual),
            )
        } else if let Some(automatic) = automatic.as_deref() {
            (
                Some(automatic.split('\n').map(str::to_owned).collect()),
                Some(ManualTranslationOrigin::Automatic),
            )
        } else {
            (None, None)
        };
        let active = source_language
            .map(|source_language| {
                generic_source_needs_translation(
                    &id,
                    &kind,
                    &source_text,
                    placeholder_service,
                    placeholder_rules,
                    source_language,
                )
            })
            .transpose()?
            .unwrap_or(false);
        entries.push(ManualTranslationEntry {
            id,
            kind: ManualTranslationType::Free,
            source,
            locator: ManualTranslationLocator::Generic { group_id, unit_id },
            rpg_maker_owner: None,
            applicability,
            needs_translation: current_translation.is_none() && rejected.is_none() && active,
            placeholder_scope: kind,
            current_translation,
            origin,
            rejected,
            outdated_manual,
        });
    }
    Ok(entries)
}

fn generic_source_needs_translation(
    id: &str,
    kind: &str,
    source: &str,
    placeholder_service: &GenericPlaceholderService,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    source_language: &dyn LanguageModule,
) -> Result<bool, ManualDatabaseError> {
    let protected = placeholder_service
        .protect_target_with_cancellation(id, kind, source, placeholder_rules, || {
            Ok::<_, std::convert::Infallible>(())
        })
        .map_err(|unreachable| match unreachable {})
        .and_then(|result| {
            result.map_err(|_| {
                ManualDatabaseError::InvalidProject(format!(
                    "{id} 的原文无法按当前 Placeholder 规则读取"
                ))
            })
        })?;
    let language_text = protected
        .language_text_with_cancellation(|| Ok::<_, std::convert::Infallible>(()))
        .map_err(|unreachable| match unreachable {})
        .and_then(|result| {
            result.map_err(|_| {
                ManualDatabaseError::InvalidProject(format!("{id} 的原文无法建立语言判断文本"))
            })
        })?;
    Ok(language_text.has_non_whitespace_natural_text()
        && source_language
            .analyze_source(&language_text)
            .needs_translation())
}

fn load_rpg_maker_manual_command_snapshot(
    connection: &Connection,
    engine: RpgMakerEngine,
    language_modules: Option<&LanguageModuleCatalog>,
) -> Result<ManualProjectSnapshot, ManualDatabaseError> {
    let canonical_json: String = connection.query_row(
        "SELECT canonical_json FROM rpg_maker_translation_resource WHERE resource_kind = 'placeholder_rules'",
        [],
        |row| row.get(0),
    )?;
    let definitions = serde_json::from_str::<Vec<PlaceholderRuleDefinition>>(&canonical_json)
        .map_err(|_| ManualDatabaseError::InvalidProject("项目 Placeholder 规则无效".to_owned()))?;
    let service =
        Pcre2PlaceholderService::new_with_cancellation(|| Ok::<_, std::convert::Infallible>(()))
            .map_err(|unreachable| match unreachable {})
            .and_then(|result| {
                result.map_err(|_| {
                    ManualDatabaseError::InvalidProject(
                        "RPG Maker 内置 Placeholder 规则无法编译".to_owned(),
                    )
                })
            })?;
    let profiles = load_rpg_maker_manual_unit_profiles(connection, engine)?;
    let valid_ids = profiles.keys().cloned().collect::<HashSet<_>>();
    let compiled = service
        .compile_custom_for_ids_with_cancellation(definitions, &valid_ids, || {
            Ok::<_, std::convert::Infallible>(())
        })
        .map_err(|unreachable| match unreachable {})
        .and_then(|result| {
            result.map_err(|_| {
                ManualDatabaseError::InvalidProject("项目 Placeholder 规则无法编译".to_owned())
            })
        })?;
    let semantics = language_modules
        .map(|modules| load_rpg_maker_language_context(connection, modules))
        .transpose()?
        .map(|(language_pair, source_language)| {
            ResolvedTranslationSemantics::new(
                engine,
                language_pair,
                Arc::new(CompiledTerminology::empty()),
                service.clone(),
                compiled.clone(),
                source_language,
            )
        });
    let entries = load_rpg_maker_entries(connection, engine, semantics.as_ref())?;
    Ok(ManualProjectSnapshot {
        index: ManualTranslationIndex::new(entries)?,
        placeholders: ManualPlaceholderValidator::RpgMaker {
            engine,
            service,
            compiled,
            profiles,
        },
    })
}

fn load_rpg_maker_manual_unit_profiles(
    connection: &Connection,
    engine: RpgMakerEngine,
) -> Result<HashMap<String, RpgMakerBuiltinPlaceholderProfile>, ManualDatabaseError> {
    let mut statement = connection.prepare(
        "SELECT g.owner, g.group_location, g.group_kind, u.unit_role
         FROM rpg_maker_text_group AS g
         JOIN rpg_maker_text_unit AS u
           ON u.owner = g.owner AND u.group_id = g.group_id
         ORDER BY u.semantic_order_key",
    )?;
    let mut rows = statement.query([])?;
    let mut profiles = HashMap::new();
    while let Some(row) = rows.next()? {
        let owner_raw: String = row.get(0)?;
        let location_raw: String = row.get(1)?;
        let kind_raw: String = row.get(2)?;
        let role_raw: String = row.get(3)?;
        let owner = RpgMakerAssetOwner::from_storage_name(&owner_raw).ok_or_else(|| {
            ManualDatabaseError::InvalidProject("RPG Maker Unit owner 无效".to_owned())
        })?;
        let location = RpgMakerLocationCodec::decode(&location_raw).map_err(|_| {
            ManualDatabaseError::InvalidProject("RPG Maker Unit 位置无效".to_owned())
        })?;
        let kind = TextGroupKind::from_storage_name(&kind_raw).ok_or_else(|| {
            ManualDatabaseError::InvalidProject("RPG Maker Unit 类型无效".to_owned())
        })?;
        let role = RpgMakerProjectionCodec::decode_role(&role_raw).map_err(|_| {
            ManualDatabaseError::InvalidProject("RPG Maker Unit role 无效".to_owned())
        })?;
        let id = readable_rpg_maker_id(&location, kind, &role);
        let profile =
            RpgMakerBuiltinPlaceholderProfile::for_location(engine, owner, kind, &location, &role);
        if profiles.insert(id, profile).is_some() {
            return Err(ManualDatabaseError::InvalidProject(
                "RPG Maker Extract 产生了重复自然 ID".to_owned(),
            ));
        }
    }
    Ok(profiles)
}

pub(crate) fn load_rpg_maker_manual_lua_snapshot(
    connection: &Connection,
    engine: RpgMakerEngine,
    language_modules: &LanguageModuleCatalog,
) -> Result<ManualProjectLuaSnapshot, ManualDatabaseError> {
    Ok(ManualProjectLuaSnapshot {
        current: load_rpg_maker_manual_command_snapshot(
            connection,
            engine,
            Some(language_modules),
        )?,
        detached: load_detached_rpg_maker_manual_translations(connection)?,
    })
}

struct ManualAutomaticApplicabilityUnit {
    role: String,
    semantic_order_key: Vec<u8>,
    source_content_json: String,
    source_context_json: String,
}

struct ManualAutomaticApplicabilityGroup {
    owner: String,
    location: String,
    kind: String,
    projection_recipe_json: String,
    semantic_order_key: Vec<u8>,
    units: Vec<ManualAutomaticApplicabilityUnit>,
}

#[derive(Clone, Copy)]
struct ManualRpgMakerApplicability {
    automatic: Sha256Fingerprint,
    rejected: Sha256Fingerprint,
}

fn load_rpg_maker_automatic_applicability(
    connection: &Connection,
) -> Result<HashMap<(String, String, String), ManualRpgMakerApplicability>, ManualDatabaseError> {
    let mut statement = connection.prepare(
        "SELECT g.owner, g.group_location, g.group_kind, g.projection_recipe_json,
                g.semantic_order_key, u.unit_role, u.semantic_order_key,
                u.source_content_json, u.source_context_json
         FROM rpg_maker_text_group AS g
         JOIN rpg_maker_text_unit AS u
           ON u.owner = g.owner AND u.group_id = g.group_id
         ORDER BY CASE g.owner WHEN 'builtin' THEN 0 ELSE 1 END,
                  g.semantic_order_key, u.semantic_order_key",
    )?;
    let mut rows = statement.query([])?;
    let mut groups = Vec::<ManualAutomaticApplicabilityGroup>::new();
    while let Some(row) = rows.next()? {
        let owner: String = row.get(0)?;
        let location: String = row.get(1)?;
        let kind: String = row.get(2)?;
        let projection_recipe_json: String = row.get(3)?;
        let group_order: Vec<u8> = row.get(4)?;
        let role: String = row.get(5)?;
        let unit_order: Vec<u8> = row.get(6)?;
        let source_content_json: String = row.get(7)?;
        let source_context_json: String = row.get(8)?;
        if RpgMakerAssetOwner::from_storage_name(&owner).is_none() {
            return Err(ManualDatabaseError::InvalidProject(
                "RPG Maker 自动译文 owner 无效".to_owned(),
            ));
        }
        let new_group = groups
            .last()
            .is_none_or(|group| group.owner != owner || group.location != location);
        if new_group {
            groups.push(ManualAutomaticApplicabilityGroup {
                owner,
                location,
                kind,
                projection_recipe_json,
                semantic_order_key: group_order,
                units: Vec::new(),
            });
        } else {
            let group = groups.last().expect("已确认当前行属于已有 Group");
            if group.kind != kind
                || group.projection_recipe_json != projection_recipe_json
                || group.semantic_order_key != group_order
            {
                return Err(ManualDatabaseError::InvalidProject(
                    "RPG Maker Group 自动译文事实不一致".to_owned(),
                ));
            }
        }
        groups
            .last_mut()
            .expect("当前行必须建立或命中一个 Group")
            .units
            .push(ManualAutomaticApplicabilityUnit {
                role,
                semantic_order_key: unit_order,
                source_content_json,
                source_context_json,
            });
    }
    drop(rows);
    drop(statement);

    if groups.is_empty() {
        return Ok(HashMap::new());
    }
    let (source_language, target_language): (String, String) = connection
        .query_row(
            "SELECT source_language, target_language FROM metadata",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(ManualDatabaseError::Sqlite)?;

    let mut logical_group_indexes = HashMap::<String, Vec<usize>>::new();
    for (index, group) in groups.iter().enumerate() {
        logical_group_indexes
            .entry(group.location.clone())
            .or_default()
            .push(index);
    }
    let mut logical_group_contexts = HashMap::new();
    for (location, indexes) in logical_group_indexes {
        let definition = &groups[indexes[0]];
        if indexes.iter().any(|index| {
            groups[*index].kind != definition.kind
                || groups[*index].semantic_order_key != definition.semantic_order_key
        }) {
            return Err(ManualDatabaseError::InvalidProject(format!(
                "{location} 的跨 owner Group 定义不一致"
            )));
        }
        let mut units = indexes
            .iter()
            .flat_map(|index| groups[*index].units.iter())
            .collect::<Vec<_>>();
        units.sort_by(|left, right| left.semantic_order_key.cmp(&right.semantic_order_key));
        let context = crate::translation::rpg_maker_group_source_context_v2(
            &definition.kind,
            units.iter().map(|unit| {
                (
                    unit.role.as_str(),
                    unit.semantic_order_key.as_slice(),
                    unit.source_content_json.as_str(),
                    unit.source_context_json.as_str(),
                )
            }),
        );
        logical_group_contexts.insert(location, context);
    }

    let mut applicability = HashMap::new();
    for group in groups {
        let group_context = *logical_group_contexts
            .get(&group.location)
            .expect("每个物理 Group 必须属于一个完整逻辑 Group");
        for unit in group.units {
            let role = RpgMakerProjectionCodec::decode_role(&unit.role).map_err(|_| {
                ManualDatabaseError::InvalidProject("RPG Maker 自动译文 Unit role 无效".to_owned())
            })?;
            let recipe_shape = RpgMakerProjectionCodec::encode_role_recipe_shape(
                &group.projection_recipe_json,
                &role,
            )
            .map_err(|_| {
                ManualDatabaseError::InvalidProject("RPG Maker 自动译文写回结构无效".to_owned())
            })?;
            let automatic = crate::translation::rpg_maker_automatic_applicability_v2(
                &source_language,
                &target_language,
                &group.owner,
                &group.kind,
                &group.location,
                &unit.role,
                &recipe_shape,
                &unit.source_content_json,
                &unit.source_context_json,
                group_context,
            );
            let rejected = crate::translation::rpg_maker_rejected_applicability_v2(
                &source_language,
                &target_language,
                &group.owner,
                &group.kind,
                &group.location,
                &unit.role,
                &recipe_shape,
                &unit.source_content_json,
                &unit.source_context_json,
                group_context,
            );
            if applicability
                .insert(
                    (group.owner.clone(), group.location.clone(), unit.role),
                    ManualRpgMakerApplicability {
                        automatic,
                        rejected,
                    },
                )
                .is_some()
            {
                return Err(ManualDatabaseError::InvalidProject(
                    "RPG Maker 自动译文 Unit 重复".to_owned(),
                ));
            }
        }
    }
    Ok(applicability)
}

fn load_rpg_maker_entries(
    connection: &Connection,
    _engine: RpgMakerEngine,
    semantics: Option<&ResolvedTranslationSemantics>,
) -> Result<Vec<ManualTranslationEntry>, ManualDatabaseError> {
    let automatic_applicability = load_rpg_maker_automatic_applicability(connection)?;
    let mut statement = connection.prepare(
        "SELECT g.owner, g.group_location, g.group_kind, g.projection_recipe_json,
                g.semantic_order_key, u.unit_role, u.source_content_json,
                u.source_context_json, u.translation_content_json, u.translation_state,
                u.semantic_order_key, manual.readable_id,
                manual.translation_type, manual.source_json,
                manual.translation_json, manual.applicability_fingerprint,
                u.rule_number,
                rejected.readable_id, rejected.origin,
                rejected.source_content_json, rejected.source_context_json,
                rejected.candidate_json, rejected.translation_json,
                rejected.violation_json,
                rejected.planning_state,
                metadata.source_language, metadata.target_language
         FROM rpg_maker_text_group AS g
         CROSS JOIN metadata
         JOIN rpg_maker_text_unit AS u
           ON u.owner = g.owner AND u.group_id = g.group_id
         LEFT JOIN rpg_maker_manual_translation AS manual
           ON manual.owner = g.owner
          AND manual.group_location = g.group_location
          AND manual.unit_role = u.unit_role
         LEFT JOIN rpg_maker_rejected_translation AS rejected
           ON rejected.owner = u.owner
          AND rejected.group_id = u.group_id
          AND rejected.unit_role = u.unit_role",
    )?;
    let mut rows = statement.query([])?;
    struct PendingEntry {
        entry: ManualTranslationEntry,
        unit_order: RpgMakerSemanticOrderKey,
    }

    let mut pending = Vec::new();
    let mut group_definitions =
        HashMap::<RpgMakerLocation, (TextGroupKind, RpgMakerSemanticOrderKey)>::new();
    let mut group_order_locations = HashMap::<RpgMakerSemanticOrderKey, RpgMakerLocation>::new();
    let mut logical_units = HashSet::<(RpgMakerLocation, TextUnitRole)>::new();
    let mut unit_order_locations =
        HashMap::<RpgMakerSemanticOrderKey, (RpgMakerLocation, TextUnitRole)>::new();
    while let Some(row) = rows.next()? {
        let owner_raw: String = row.get(0)?;
        let group_location_raw: String = row.get(1)?;
        let kind_raw: String = row.get(2)?;
        let recipe_json: String = row.get(3)?;
        let group_order_raw: Vec<u8> = row.get(4)?;
        let role_raw: String = row.get(5)?;
        let source_json: String = row.get(6)?;
        let context_json: String = row.get(7)?;
        let automatic: Option<String> = row.get(8)?;
        let automatic_state: Option<Vec<u8>> = row.get(9)?;
        let unit_order_raw: Vec<u8> = row.get(10)?;
        let stored_manual = parse_stored_manual_translation(
            row.get(11)?,
            row.get(12)?,
            row.get(13)?,
            row.get(14)?,
            row.get(15)?,
        )?;
        let rule_number: Option<i64> = row.get(16)?;
        let rejected_row = (
            row.get::<_, Option<String>>(17)?,
            row.get::<_, Option<String>>(18)?,
            row.get::<_, Option<String>>(19)?,
            row.get::<_, Option<String>>(20)?,
            row.get::<_, Option<String>>(21)?,
            row.get::<_, Option<String>>(22)?,
            row.get::<_, Option<String>>(23)?,
            row.get::<_, Option<Vec<u8>>>(24)?,
        );
        let source_language: String = row.get(25)?;
        let target_language: String = row.get(26)?;
        let owner = RpgMakerAssetOwner::from_storage_name(&owner_raw).ok_or_else(|| {
            ManualDatabaseError::InvalidProject("人工译文所属来源无效".to_owned())
        })?;
        let rpg_maker_owner = match (owner, rule_number) {
            (RpgMakerAssetOwner::Builtin, None) => ManualRpgMakerOwner::Builtin,
            (RpgMakerAssetOwner::Rules, Some(rule_number)) if rule_number > 0 => {
                ManualRpgMakerOwner::Rules {
                    rule_number: usize::try_from(rule_number).map_err(|_| {
                        ManualDatabaseError::InvalidProject(
                            "Rules Unit 的自然规则序号超出支持范围".to_owned(),
                        )
                    })?,
                }
            }
            (RpgMakerAssetOwner::Builtin, Some(_)) => {
                return Err(ManualDatabaseError::InvalidProject(
                    "Builtin Unit 不得带有 Rules 自然规则序号".to_owned(),
                ));
            }
            (RpgMakerAssetOwner::Rules, _) => {
                return Err(ManualDatabaseError::InvalidProject(
                    "Rules Unit 缺少正整数自然规则序号".to_owned(),
                ));
            }
        };
        let group_location = RpgMakerLocationCodec::decode(&group_location_raw)
            .map_err(|_| ManualDatabaseError::InvalidProject("人工译文位置无效".to_owned()))?;
        let kind = TextGroupKind::from_storage_name(&kind_raw)
            .ok_or_else(|| ManualDatabaseError::InvalidProject("人工译文组类型无效".to_owned()))?;
        let role = RpgMakerProjectionCodec::decode_role(&role_raw)
            .map_err(|_| ManualDatabaseError::InvalidProject("人工译文字段无效".to_owned()))?;
        let group_order = RpgMakerSemanticOrderKey::decode(&group_order_raw).map_err(|_| {
            ManualDatabaseError::InvalidProject("人工译文 Group 的自然顺序无效".to_owned())
        })?;
        let unit_order = RpgMakerSemanticOrderKey::decode(&unit_order_raw).map_err(|_| {
            ManualDatabaseError::InvalidProject("人工译文 Unit 的自然顺序无效".to_owned())
        })?;
        if let Some((existing_kind, existing_order)) = group_definitions.get(&group_location) {
            if existing_kind != &kind || existing_order != &group_order {
                return Err(ManualDatabaseError::InvalidProject(format!(
                    "{group_location} 在不同来源中的 Group 定义不一致"
                )));
            }
        } else {
            if group_order_locations
                .insert(group_order.clone(), group_location.clone())
                .is_some()
            {
                return Err(ManualDatabaseError::InvalidProject(
                    "多个 RPG Maker Group 使用了同一自然顺序".to_owned(),
                ));
            }
            group_definitions.insert(group_location.clone(), (kind, group_order));
        }
        if !logical_units.insert((group_location.clone(), role.clone())) {
            return Err(ManualDatabaseError::InvalidProject(format!(
                "{} 在不同来源中重复定义了同一字段",
                readable_rpg_maker_id(&group_location, kind, &role)
            )));
        }
        if unit_order_locations
            .insert(unit_order.clone(), (group_location.clone(), role.clone()))
            .is_some()
        {
            return Err(ManualDatabaseError::InvalidProject(
                "多个 RPG Maker Unit 使用了同一自然顺序".to_owned(),
            ));
        }
        let content = serde_json::from_str::<TextUnitContent>(&source_json)
            .map_err(|_| ManualDatabaseError::InvalidProject("人工译文原文无效".to_owned()))?;
        let identity = TranslationUnitIdentity::new(
            owner,
            kind,
            group_location.clone(),
            role.clone(),
            content.clone(),
            context_json,
        );
        let manual_type = rpg_maker_manual_type(&identity);
        let source = rpg_maker_manual_source_lines(&content);
        let id = readable_rpg_maker_id(&group_location, kind, &role);
        let rejected = match rejected_row {
            (None, None, None, None, None, None, None, None) => None,
            (
                Some(_readable_id),
                Some(origin),
                Some(rejected_source_json),
                Some(rejected_context_json),
                Some(candidate_json),
                translation_json,
                Some(violation_json),
                Some(planning_state),
            ) => {
                let rejected_source = serde_json::from_str::<TextUnitContent>(
                    &rejected_source_json,
                )
                .map_err(|_| {
                    ManualDatabaseError::InvalidProject(format!("{id} 的 Rejected 原文无效"))
                })?;
                let planning_state =
                    Sha256Fingerprint::from_slice(&planning_state).map_err(|_| {
                        ManualDatabaseError::InvalidProject(format!(
                            "{id} 的 Rejected planning_state 无效"
                        ))
                    })?;
                let expected_rejected = automatic_applicability
                    .get(&(
                        owner_raw.clone(),
                        group_location_raw.clone(),
                        role_raw.clone(),
                    ))
                    .ok_or_else(|| {
                        ManualDatabaseError::InvalidProject(format!(
                            "{id} 缺少 Rejected 适用性事实"
                        ))
                    })?;
                if rejected_source != content
                    || rejected_context_json != identity.source_context_json()
                    || !crate::translation::rpg_maker_rejected_applicability_is_current(
                        planning_state,
                        expected_rejected.rejected,
                    )
                {
                    None
                } else {
                    Some(parse_manual_rejected_candidate(
                        &id,
                        parse_manual_translation_origin(&id, &origin)?,
                        candidate_json,
                        translation_json,
                        violation_json,
                    )?)
                }
            }
            _ => {
                return Err(ManualDatabaseError::InvalidProject(format!(
                    "{id} 的 Rejected 记录不完整"
                )));
            }
        };
        let recipe_shape =
            RpgMakerProjectionCodec::encode_role_recipe_shape(&recipe_json, &role)
                .map_err(|_| ManualDatabaseError::InvalidProject(format!("{id} 的写回结构无效")))?;
        let applicability = rpg_maker_manual_applicability(RpgMakerManualApplicabilityFacts {
            owner: &owner_raw,
            group_location: &group_location_raw,
            kind: &kind_raw,
            role: &role_raw,
            recipe_shape: &recipe_shape,
            translation_type: manual_type,
            source_language: &source_language,
            target_language: &target_language,
            source: &source,
        });
        let current_manual = stored_manual
            .as_ref()
            .filter(|manual| manual.applicability == applicability);
        let outdated_manual = stored_manual
            .as_ref()
            .filter(|manual| manual.applicability != applicability)
            .map(manual_outdated_snapshot);
        let automatic = match (automatic.as_deref(), automatic_state) {
            (None, None) => None,
            (Some(value), Some(state)) => {
                let state = Sha256Fingerprint::from_slice(&state).map_err(|_| {
                    ManualDatabaseError::InvalidProject(format!("{id} 的自动译文状态长度无效"))
                })?;
                let expected = automatic_applicability
                    .get(&(
                        owner_raw.clone(),
                        group_location_raw.clone(),
                        role_raw.clone(),
                    ))
                    .ok_or_else(|| {
                        ManualDatabaseError::InvalidProject(format!("{id} 缺少自动译文适用性事实"))
                    })?;
                if crate::translation::rpg_maker_automatic_applicability_is_current(
                    state,
                    expected.automatic,
                ) {
                    Some(serde_json::from_str::<TextUnitContent>(value).map_err(|_| {
                        ManualDatabaseError::InvalidProject(format!("{id} 的自动译文无法读取"))
                    })?)
                } else {
                    None
                }
            }
            _ => {
                return Err(ManualDatabaseError::InvalidProject(format!(
                    "{id} 的自动译文正文与状态不完整"
                )));
            }
        };
        let (current_translation, origin) = if let Some(manual) = current_manual {
            (
                Some(manual.translation.clone()),
                Some(ManualTranslationOrigin::Manual),
            )
        } else if let Some(automatic) = automatic.as_ref() {
            (
                Some(rpg_maker_manual_source_lines(automatic)),
                Some(ManualTranslationOrigin::Automatic),
            )
        } else {
            (None, None)
        };
        let active = semantics
            .map(|semantics| {
                semantics
                    .prepare_identity_content_with_cancellation(&identity, &content, || {
                        Ok::<_, std::convert::Infallible>(())
                    })
                    .map_err(|unreachable| match unreachable {})
                    .and_then(|result| {
                        result.map_err(|_| {
                            ManualDatabaseError::InvalidProject(format!(
                                "{id} 的原文无法按当前 Placeholder 规则读取"
                            ))
                        })
                    })
                    .map(|prepared| prepared.status() == PreparedTranslationStatus::Active)
            })
            .transpose()?
            .unwrap_or(false);
        pending.push(PendingEntry {
            entry: ManualTranslationEntry {
                id,
                kind: manual_type,
                source,
                locator: ManualTranslationLocator::RpgMaker {
                    owner: owner_raw,
                    group_location: group_location_raw,
                    unit_role: role_raw,
                },
                rpg_maker_owner: Some(rpg_maker_owner),
                applicability,
                needs_translation: current_translation.is_none() && rejected.is_none() && active,
                placeholder_scope: kind_raw,
                current_translation,
                origin,
                rejected,
                outdated_manual,
            },
            unit_order,
        });
    }
    pending.sort_by(|left, right| left.unit_order.cmp(&right.unit_order));
    Ok(pending.into_iter().map(|pending| pending.entry).collect())
}

fn parse_stored_manual_translation(
    id: Option<String>,
    kind: Option<String>,
    source_json: Option<String>,
    translation_json: Option<String>,
    applicability: Option<Vec<u8>>,
) -> Result<Option<StoredManualTranslation>, ManualDatabaseError> {
    let (id, kind, source_json, translation_json, applicability) =
        match (id, kind, source_json, translation_json, applicability) {
            (None, None, None, None, None) => return Ok(None),
            (Some(id), Some(kind), Some(source), Some(translation), Some(applicability)) => {
                (id, kind, source, translation, applicability)
            }
            _ => {
                return Err(ManualDatabaseError::InvalidProject(
                    "人工译文记录不完整".to_owned(),
                ));
            }
        };
    let kind = ManualTranslationType::from_storage_name(&kind)
        .ok_or_else(|| ManualDatabaseError::InvalidProject(format!("{id} 的人工译文类型无效")))?;
    let source = serde_json::from_str::<Vec<String>>(&source_json)
        .map_err(|_| ManualDatabaseError::InvalidProject(format!("{id} 的旧原文无法读取")))?;
    let translation = serde_json::from_str::<Vec<String>>(&translation_json)
        .map_err(|_| ManualDatabaseError::InvalidProject(format!("{id} 的人工译文无法读取")))?;
    if translation.is_empty() {
        return Err(ManualDatabaseError::InvalidProject(format!(
            "{id} 的人工译文为空"
        )));
    }
    let applicability = Sha256Fingerprint::from_slice(&applicability)
        .map_err(|_| ManualDatabaseError::InvalidProject(format!("{id} 的人工译文状态无效")))?;
    Ok(Some(StoredManualTranslation {
        id,
        kind,
        source,
        translation,
        applicability,
    }))
}

fn parse_manual_rejected_candidate(
    id: &str,
    origin: ManualTranslationOrigin,
    candidate_json: String,
    translation_json: Option<String>,
    violation_json: String,
) -> Result<ManualRejectedCandidate, ManualDatabaseError> {
    serde_json::from_str::<Box<serde_json::value::RawValue>>(&candidate_json).map_err(|_| {
        ManualDatabaseError::InvalidProject(format!("{id} 的 Rejected 候选 JSON 无效"))
    })?;
    let translation = translation_json
        .map(|translation| {
            serde_json::from_str::<Vec<String>>(&translation).map_err(|_| {
                ManualDatabaseError::InvalidProject(format!("{id} 的 Rejected 译文投影无效"))
            })
        })
        .transpose()?
        .or_else(|| {
            serde_json::from_str::<Vec<String>>(&candidate_json)
                .ok()
                .filter(|translation| !translation.is_empty())
        });
    let violation = serde_json::from_str::<ProvenInvariantViolation>(&violation_json)
        .map_err(|_| ManualDatabaseError::InvalidProject(format!("{id} 的 Rejected 违反项无效")))?;
    Ok(ManualRejectedCandidate {
        origin,
        candidate_json,
        translation,
        violation,
    })
}

fn parse_manual_translation_origin(
    id: &str,
    value: &str,
) -> Result<ManualTranslationOrigin, ManualDatabaseError> {
    match value {
        "manual" => Ok(ManualTranslationOrigin::Manual),
        "automatic" => Ok(ManualTranslationOrigin::Automatic),
        _ => Err(ManualDatabaseError::InvalidProject(format!(
            "{id} 的 Rejected 译文来源无效"
        ))),
    }
}

fn parse_stored_generic_manual_translation(
    id: Option<String>,
    source_json: Option<String>,
    translation_json: Option<String>,
    applicability: Option<Vec<u8>>,
) -> Result<Option<StoredManualTranslation>, ManualDatabaseError> {
    let kind = id
        .as_ref()
        .map(|_| ManualTranslationType::Free.as_str().to_owned());
    parse_stored_manual_translation(id, kind, source_json, translation_json, applicability)
}

fn manual_outdated_snapshot(manual: &StoredManualTranslation) -> ManualOutdatedTranslation {
    ManualOutdatedTranslation {
        id: manual.id.clone(),
        kind: manual.kind,
        source: manual.source.clone(),
        translation: manual.translation.clone(),
    }
}

fn load_detached_generic_manual_translations(
    connection: &Connection,
) -> Result<Vec<ManualDetachedTranslation>, ManualDatabaseError> {
    let mut statement = connection.prepare(
        "SELECT manual.group_id, manual.unit_id, manual.readable_id,
                manual.source_json, manual.translation_json,
                manual.applicability_fingerprint
         FROM generic_manual_translation AS manual
         LEFT JOIN generic_unit AS unit
           ON unit.group_id = manual.group_id AND unit.unit_id = manual.unit_id
         WHERE unit.group_id IS NULL
         ORDER BY manual.readable_id, manual.group_id, manual.unit_id",
    )?;
    let mut rows = statement.query([])?;
    let mut detached = Vec::new();
    while let Some(row) = rows.next()? {
        let group_id: String = row.get(0)?;
        let unit_id: String = row.get(1)?;
        let stored = parse_stored_generic_manual_translation(
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
        )?
        .ok_or_else(|| ManualDatabaseError::InvalidProject("人工译文记录不完整".to_owned()))?;
        detached.push(ManualDetachedTranslation {
            snapshot: manual_outdated_snapshot(&stored),
            locator: ManualTranslationLocator::Generic { group_id, unit_id },
        });
    }
    Ok(detached)
}

fn load_detached_rpg_maker_manual_translations(
    connection: &Connection,
) -> Result<Vec<ManualDetachedTranslation>, ManualDatabaseError> {
    let mut statement = connection.prepare(
        "SELECT manual.owner, manual.group_location, manual.unit_role,
                manual.readable_id, manual.translation_type, manual.source_json,
                manual.translation_json, manual.applicability_fingerprint
         FROM rpg_maker_manual_translation AS manual
         LEFT JOIN rpg_maker_text_group AS text_group
           ON text_group.owner = manual.owner
          AND text_group.group_location = manual.group_location
         LEFT JOIN rpg_maker_text_unit AS unit
           ON unit.owner = text_group.owner
          AND unit.group_id = text_group.group_id
          AND unit.unit_role = manual.unit_role
         WHERE unit.owner IS NULL
         ORDER BY manual.readable_id, manual.owner,
                  manual.group_location, manual.unit_role",
    )?;
    let mut rows = statement.query([])?;
    let mut detached = Vec::new();
    while let Some(row) = rows.next()? {
        let owner: String = row.get(0)?;
        let group_location: String = row.get(1)?;
        let unit_role: String = row.get(2)?;
        let stored = parse_stored_manual_translation(
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
            row.get(7)?,
        )?
        .ok_or_else(|| ManualDatabaseError::InvalidProject("人工译文记录不完整".to_owned()))?;
        detached.push(ManualDetachedTranslation {
            snapshot: manual_outdated_snapshot(&stored),
            locator: ManualTranslationLocator::RpgMaker {
                owner,
                group_location,
                unit_role,
            },
        });
    }
    Ok(detached)
}

pub(crate) fn validate_manual_set(
    snapshot: &ManualProjectSnapshot,
    id: &str,
    translation: Vec<String>,
) -> Result<ValidatedManualTranslation, ManualCheckIssue> {
    if translation.is_empty() {
        let problem = ManualCheckProblem::EmptyTranslation;
        return Err(ManualCheckIssue {
            id: id.to_owned(),
            problem,
        });
    }
    let Some(current) = snapshot.index.get(id) else {
        let problem = ManualCheckProblem::UnknownId;
        return Err(ManualCheckIssue {
            id: id.to_owned(),
            problem,
        });
    };
    let document = ManualDocument {
        translation: vec![ManualDocumentEntry {
            id: id.to_owned(),
            kind: current.kind,
            source: current.source.clone(),
            translation,
        }],
    };
    let mut validate = |entry: &ManualTranslationEntry, translation: &[String]| {
        snapshot.placeholders.validate(entry, translation)
    };
    let mut report = check_document(document, &snapshot.index, &mut validate);
    if let Some(issue) = report.errors.pop() {
        return Err(issue);
    }
    report.writes.pop().ok_or_else(|| {
        let problem = ManualCheckProblem::InvalidStructure;
        ManualCheckIssue {
            id: id.to_owned(),
            problem,
        }
    })
}

pub(crate) fn apply_generic_manual_translations(
    connection: &Connection,
    writes: &[ValidatedManualTranslation],
) -> Result<usize, ManualDatabaseError> {
    apply_generic_manual_translations_with_cancellation(
        connection,
        writes,
        &CooperativeCancellation::default(),
    )
}

fn apply_generic_manual_translations_with_cancellation(
    connection: &Connection,
    writes: &[ValidatedManualTranslation],
    cancellation: &CooperativeCancellation,
) -> Result<usize, ManualDatabaseError> {
    for write in writes {
        ensure_manual_database_running(cancellation)?;
        let ManualTranslationLocator::Generic { group_id, unit_id } = &write.locator else {
            return Err(ManualDatabaseError::InvalidProject(
                "人工译文位置不属于 Generic".to_owned(),
            ));
        };
        connection.execute(
            "INSERT INTO generic_manual_translation (
                 group_id, unit_id, readable_id,
                 source_json, translation_json, applicability_fingerprint
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (group_id, unit_id) DO UPDATE SET
                 readable_id = excluded.readable_id,
                 source_json = excluded.source_json,
                 translation_json = excluded.translation_json,
                 applicability_fingerprint = excluded.applicability_fingerprint",
            params![
                group_id,
                unit_id,
                write.id,
                serde_json::to_string(&write.source).expect("字符串数组应可编码"),
                serde_json::to_string(&write.translation).expect("字符串数组应可编码"),
                write.applicability.as_bytes().as_slice(),
            ],
        )?;
        ensure_manual_database_running(cancellation)?;
        connection.execute(
            "UPDATE generic_unit SET translation = NULL, translation_state = NULL
             WHERE group_id = ?1 AND unit_id = ?2",
            params![group_id, unit_id],
        )?;
        ensure_manual_database_running(cancellation)?;
        connection.execute(
            "DELETE FROM generic_rejected_translation WHERE group_id = ?1 AND unit_id = ?2",
            params![group_id, unit_id],
        )?;
    }
    Ok(writes.len())
}

pub(crate) fn apply_rpg_maker_manual_translations(
    connection: &Connection,
    writes: &[ValidatedManualTranslation],
) -> Result<usize, ManualDatabaseError> {
    apply_rpg_maker_manual_translations_with_cancellation(
        connection,
        writes,
        &CooperativeCancellation::default(),
    )
}

fn apply_rpg_maker_manual_translations_with_cancellation(
    connection: &Connection,
    writes: &[ValidatedManualTranslation],
    cancellation: &CooperativeCancellation,
) -> Result<usize, ManualDatabaseError> {
    for write in writes {
        ensure_manual_database_running(cancellation)?;
        let ManualTranslationLocator::RpgMaker {
            owner,
            group_location,
            unit_role,
        } = &write.locator
        else {
            return Err(ManualDatabaseError::InvalidProject(
                "人工译文位置不属于 RPG Maker".to_owned(),
            ));
        };
        connection.execute(
            "INSERT INTO rpg_maker_manual_translation (
                 owner, group_location, unit_role, readable_id, translation_type,
                 source_json, translation_json, applicability_fingerprint
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (owner, group_location, unit_role) DO UPDATE SET
                 readable_id = excluded.readable_id,
                 translation_type = excluded.translation_type,
                 source_json = excluded.source_json,
                 translation_json = excluded.translation_json,
                 applicability_fingerprint = excluded.applicability_fingerprint",
            params![
                owner,
                group_location,
                unit_role,
                write.id,
                manual_type_name(write.kind),
                serde_json::to_string(&write.source).expect("字符串数组应可编码"),
                serde_json::to_string(&write.translation).expect("字符串数组应可编码"),
                write.applicability.as_bytes().as_slice(),
            ],
        )?;
        ensure_manual_database_running(cancellation)?;
        connection.execute(
            "UPDATE rpg_maker_text_unit SET translation_content_json = NULL, translation_state = NULL
             WHERE owner = ?1 AND unit_role = ?2
               AND group_id = (
                   SELECT group_id FROM rpg_maker_text_group
                   WHERE owner = ?1 AND group_location = ?3
               )",
            params![owner, unit_role, group_location],
        )?;
        ensure_manual_database_running(cancellation)?;
        connection.execute(
            "DELETE FROM rpg_maker_rejected_translation
             WHERE owner = ?1 AND unit_role = ?2
               AND group_id = (
                   SELECT group_id FROM rpg_maker_text_group
                   WHERE owner = ?1 AND group_location = ?3
               )",
            params![owner, unit_role, group_location],
        )?;
    }
    Ok(writes.len())
}

fn ensure_manual_database_running(
    cancellation: &CooperativeCancellation,
) -> Result<(), ManualDatabaseError> {
    if cancellation.is_requested() {
        Err(ManualDatabaseError::Cancelled)
    } else {
        Ok(())
    }
}

pub(crate) fn clear_generic_manual_translation(
    connection: &Connection,
    locator: &ManualTranslationLocator,
) -> Result<u64, ManualDatabaseError> {
    let ManualTranslationLocator::Generic { group_id, unit_id } = locator else {
        return Err(ManualDatabaseError::InvalidProject(
            "人工译文位置不属于 Generic".to_owned(),
        ));
    };
    let manual = connection.execute(
        "DELETE FROM generic_manual_translation WHERE group_id = ?1 AND unit_id = ?2",
        params![group_id, unit_id],
    )?;
    let automatic = connection.execute(
        "UPDATE generic_unit SET translation = NULL, translation_state = NULL
         WHERE group_id = ?1 AND unit_id = ?2
           AND (translation IS NOT NULL OR translation_state IS NOT NULL)",
        params![group_id, unit_id],
    )?;
    let rejected = connection.execute(
        "DELETE FROM generic_rejected_translation WHERE group_id = ?1 AND unit_id = ?2",
        params![group_id, unit_id],
    )?;
    Ok((manual as u64)
        .saturating_add(automatic as u64)
        .saturating_add(rejected as u64))
}

pub(crate) fn clear_rpg_maker_manual_translation(
    connection: &Connection,
    locator: &ManualTranslationLocator,
) -> Result<u64, ManualDatabaseError> {
    let ManualTranslationLocator::RpgMaker {
        owner,
        group_location,
        unit_role,
    } = locator
    else {
        return Err(ManualDatabaseError::InvalidProject(
            "人工译文位置不属于 RPG Maker".to_owned(),
        ));
    };
    let manual = connection.execute(
        "DELETE FROM rpg_maker_manual_translation
         WHERE owner = ?1 AND group_location = ?2 AND unit_role = ?3",
        params![owner, group_location, unit_role],
    )?;
    let automatic = connection.execute(
        "UPDATE rpg_maker_text_unit
         SET translation_content_json = NULL, translation_state = NULL
         WHERE owner = ?1 AND unit_role = ?3
           AND group_id = (
               SELECT group_id FROM rpg_maker_text_group
               WHERE owner = ?1 AND group_location = ?2
           )
           AND (translation_content_json IS NOT NULL OR translation_state IS NOT NULL)",
        params![owner, group_location, unit_role],
    )?;
    let rejected = connection.execute(
        "DELETE FROM rpg_maker_rejected_translation
         WHERE owner = ?1 AND unit_role = ?3
           AND group_id = (
               SELECT group_id FROM rpg_maker_text_group
               WHERE owner = ?1 AND group_location = ?2
           )",
        params![owner, group_location, unit_role],
    )?;
    Ok((manual as u64)
        .saturating_add(automatic as u64)
        .saturating_add(rejected as u64))
}

fn open_read_only(
    path: &Path,
    cancellation: &CooperativeCancellation,
) -> Result<Connection, ManualDatabaseError> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    install_manual_sqlite_cancellation(&connection, cancellation)?;
    Ok(connection)
}

fn open_read_write(
    path: &Path,
    cancellation: &CooperativeCancellation,
) -> Result<Connection, ManualDatabaseError> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    install_manual_sqlite_cancellation(&connection, cancellation)?;
    Ok(connection)
}

fn install_manual_sqlite_cancellation(
    connection: &Connection,
    cancellation: &CooperativeCancellation,
) -> Result<(), ManualDatabaseError> {
    let cancellation = cancellation.clone();
    connection.progress_handler(
        MANUAL_SQLITE_CANCELLATION_CHECK_OPERATIONS,
        Some(move || cancellation.is_requested()),
    )?;
    Ok(())
}

fn manual_type_name(kind: ManualTranslationType) -> &'static str {
    match kind {
        ManualTranslationType::Fixed => "fixed",
        ManualTranslationType::Free => "free",
    }
}

pub(crate) fn generic_manual_applicability(
    group_id: &str,
    unit_id: &str,
    relative_path: &str,
    kind: &str,
    source_language: &str,
    target_language: &str,
    source: &[String],
) -> Sha256Fingerprint {
    let mut hasher = Sha256FramedHasher::new(b"att.generic.manual-translation");
    hasher
        .frame(1, group_id.as_bytes())
        .frame(2, unit_id.as_bytes())
        .frame(3, relative_path.as_bytes())
        .frame(4, kind.as_bytes())
        .frame(5, source_language.as_bytes())
        .frame(6, target_language.as_bytes());
    for line in source {
        hasher.frame(7, line.as_bytes());
    }
    hasher.finish()
}

pub(crate) struct RpgMakerManualApplicabilityFacts<'a> {
    pub(crate) owner: &'a str,
    pub(crate) group_location: &'a str,
    pub(crate) kind: &'a str,
    pub(crate) role: &'a str,
    pub(crate) recipe_shape: &'a str,
    pub(crate) translation_type: ManualTranslationType,
    pub(crate) source_language: &'a str,
    pub(crate) target_language: &'a str,
    pub(crate) source: &'a [String],
}

pub(crate) fn rpg_maker_manual_applicability(
    facts: RpgMakerManualApplicabilityFacts<'_>,
) -> Sha256Fingerprint {
    let mut hasher = Sha256FramedHasher::new(b"att.rpg-maker.manual-translation");
    hasher
        .frame(1, facts.owner.as_bytes())
        .frame(2, facts.group_location.as_bytes())
        .frame(3, facts.kind.as_bytes())
        .frame(4, facts.role.as_bytes())
        .frame(5, facts.recipe_shape.as_bytes())
        .frame(6, manual_type_name(facts.translation_type).as_bytes())
        .frame(7, facts.source_language.as_bytes())
        .frame(8, facts.target_language.as_bytes());
    for line in facts.source {
        hasher.frame(9, line.as_bytes());
    }
    hasher.finish()
}

pub(crate) fn rpg_maker_manual_type(identity: &TranslationUnitIdentity) -> ManualTranslationType {
    match expected_line_shape(identity) {
        ExpectedLineShape::Aligned(_) => ManualTranslationType::Fixed,
        ExpectedLineShape::Reflow => ManualTranslationType::Free,
    }
}

pub(crate) fn rpg_maker_manual_source_lines(content: &TextUnitContent) -> Vec<String> {
    match content {
        TextUnitContent::Value(value) => value.split('\n').map(str::to_owned).collect(),
        TextUnitContent::Lines(lines) => lines.clone(),
    }
}

pub(crate) fn rpg_maker_manual_translation_content(
    source: &TextUnitContent,
    translation: Vec<String>,
) -> TextUnitContent {
    match source {
        TextUnitContent::Value(_) => TextUnitContent::Value(translation.join("\n")),
        TextUnitContent::Lines(_) => TextUnitContent::Lines(translation),
    }
}

pub(crate) fn readable_rpg_maker_id(
    location: &RpgMakerLocation,
    kind: TextGroupKind,
    role: &TextUnitRole,
) -> String {
    let mut id = match location.source() {
        RpgMakerSource::Data(file) => file.file_name().to_owned(),
        RpgMakerSource::DataFile(file) => file.as_str().to_owned(),
        RpgMakerSource::Map(map_id) => map_id.file_name(),
        RpgMakerSource::PluginParameter {
            plugin_index,
            plugin_name,
            parameter_name,
        } => format!(
            "plugins.js:plugin{}:{}:{}",
            plugin_index + 1,
            readable_component(plugin_name),
            readable_component(parameter_name)
        ),
    };
    let steps = location.steps();
    if matches!(location.source(), RpgMakerSource::Map(_))
        && let Some(rendered) = readable_map_steps(steps, kind)
    {
        id.push_str(&rendered);
    } else {
        for (position, step) in steps.iter().enumerate() {
            match step {
                RpgMakerLocationStep::ObjectKey(key) => {
                    push_readable_object_key(&mut id, key);
                }
                RpgMakerLocationStep::ArrayIndex(index) => {
                    id.push(':');
                    id.push_str(
                        &readable_array_number(location.source(), steps, position, *index)
                            .to_string(),
                    );
                }
                RpgMakerLocationStep::DecodeJsonString => {}
            }
        }
    }
    let role = match role {
        TextUnitRole::Scalar(key) => Some(key.as_str()),
        TextUnitRole::DialogueSpeaker => Some("speaker"),
        TextUnitRole::DialogueBody => None,
        TextUnitRole::Choices => None,
        TextUnitRole::ScrollingText => None,
    };
    if let Some(role) = role {
        id.push(':');
        id.push_str(&readable_component(role));
    }
    id
}

fn readable_map_steps(steps: &[RpgMakerLocationStep], kind: TextGroupKind) -> Option<String> {
    let [
        RpgMakerLocationStep::ObjectKey(events),
        RpgMakerLocationStep::ArrayIndex(event),
        RpgMakerLocationStep::ObjectKey(pages),
        RpgMakerLocationStep::ArrayIndex(page),
        RpgMakerLocationStep::ObjectKey(list),
        RpgMakerLocationStep::ArrayIndex(command),
    ] = steps
    else {
        return None;
    };
    if events != "events" || pages != "pages" || list != "list" {
        return None;
    }
    let label = match kind {
        TextGroupKind::EventDialogue => "dialogue",
        TextGroupKind::EventChoices => "choices",
        TextGroupKind::EventScrollingText => "scrolling",
        TextGroupKind::EventCommand => "command",
        _ => return None,
    };
    Some(format!(
        ":event{event}:page{}:{label}{}",
        page + 1,
        command + 1
    ))
}

fn readable_component(value: &str) -> String {
    let mut characters = value.chars();
    if characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        value.to_owned()
    } else {
        serde_json::to_string(value).expect("字符串应可编码为 JSON")
    }
}

fn push_readable_object_key(output: &mut String, value: &str) {
    let readable = readable_component(value);
    if readable == value {
        output.push(':');
        output.push_str(value);
    } else {
        output.push('[');
        output.push_str(&readable);
        output.push(']');
    }
}

fn readable_array_number(
    source: &RpgMakerSource,
    steps: &[RpgMakerLocationStep],
    position: usize,
    index: usize,
) -> usize {
    let is_database_id = position == 0 && matches!(source, RpgMakerSource::Data(_));
    let is_map_event_id = matches!(source, RpgMakerSource::Map(_))
        && position > 0
        && matches!(
            &steps[position - 1],
            RpgMakerLocationStep::ObjectKey(key) if key == "events"
        );
    if is_database_id || is_map_event_id {
        index
    } else {
        index + 1
    }
}

#[cfg(windows)]
fn decode_windows_path(bytes: &[u8]) -> Result<PathBuf, ManualDatabaseError> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    if !bytes.len().is_multiple_of(2) {
        return Err(ManualDatabaseError::InvalidProject(
            "Generic 文件路径无效".to_owned(),
        ));
    }
    let units = bytes
        .chunks_exact(2)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
        .collect::<Vec<_>>();
    Ok(PathBuf::from(OsString::from_wide(&units)))
}

#[cfg(not(windows))]
fn decode_windows_path(_: &[u8]) -> Result<PathBuf, ManualDatabaseError> {
    unreachable!("ATT 只支持 Windows")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generic::create_current_generic_schema_for_test;
    use crate::rpg_maker::project_database::create_current_rpg_maker_schema_for_test;
    use crate::rpg_maker::text::StandardDataFile;
    use std::fs::File;
    use std::os::windows::ffi::OsStrExt;

    fn indexed_entry(kind: ManualTranslationType, source: &[&str]) -> ManualTranslationEntry {
        ManualTranslationEntry {
            id: "Skills.json:798:name".to_owned(),
            kind,
            source: source.iter().map(|line| (*line).to_owned()).collect(),
            locator: ManualTranslationLocator::RpgMaker {
                owner: "builtin".to_owned(),
                group_location: "location".to_owned(),
                unit_role: "role".to_owned(),
            },
            rpg_maker_owner: Some(ManualRpgMakerOwner::Builtin),
            applicability: Sha256Fingerprint::from_bytes([7; 32]),
            needs_translation: true,
            placeholder_scope: "database_entry".to_owned(),
            current_translation: None,
            origin: None,
            rejected: None,
            outdated_manual: None,
        }
    }

    fn write_document(path: &Path, source: &str) {
        fs::write(path, source).expect("应写入 Manual TOML");
    }

    #[test]
    fn manual_export_never_moves_or_replaces_an_existing_directory_target() {
        let directory = tempfile::tempdir().expect("应建立目录目标测试根");
        let directory_target = directory.path().join("manual.toml");
        fs::create_dir(&directory_target).unwrap();
        fs::write(directory_target.join("keep.txt"), "keep").unwrap();
        let single_error = atomic_replace(
            &directory_target,
            b"manual",
            &CooperativeCancellation::default(),
        )
        .expect_err("目录目标必须在单文件 staging 前拒绝");
        assert!(matches!(
            single_error,
            ManualDocumentError::OutputTarget {
                problem: ManualOutputTargetProblem::NotRegularFile { .. },
            }
        ));
        assert!(directory_target.is_dir());
        assert_eq!(
            fs::read_to_string(directory_target.join("keep.txt")).unwrap(),
            "keep"
        );
        assert!(!directory.path().join(".manual.toml.tmp").exists());
    }

    #[cfg(windows)]
    #[test]
    fn manual_exports_reject_reparse_target_before_creating_publication_artifacts() {
        let directory = tempfile::tempdir().expect("应建立 reparse 目标测试根");
        let real = directory.path().join("real-manual.toml");
        let link = directory.path().join("manual.toml");
        fs::write(&real, "original").unwrap();
        if let Err(source) = std::os::windows::fs::symlink_file(&real, &link) {
            if source.kind() == io::ErrorKind::PermissionDenied
                || source.raw_os_error() == Some(1314)
            {
                return;
            }
            panic!("应建立测试文件符号链接：{source}");
        }

        let error = atomic_replace(&link, b"new manual", &CooperativeCancellation::default())
            .expect_err("单文件导出必须拒绝 reparse 目标");
        assert!(matches!(
            error,
            ManualDocumentError::OutputTarget {
                problem: ManualOutputTargetProblem::ReparsePoint { .. },
            }
        ));
        assert_eq!(fs::read_to_string(&real).unwrap(), "original");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!manual_temporary_path(&link).exists());
    }

    #[test]
    fn manual_export_replaces_the_exact_bound_regular_file() {
        let directory = tempfile::tempdir().expect("应建立输出替换测试根");
        let target = directory.path().join("manual.toml");
        fs::write(&target, "old manual").expect("应建立旧输出");

        atomic_replace(&target, b"new manual", &CooperativeCancellation::default())
            .expect("应通过固定目标句柄完成原子替换");

        assert_eq!(fs::read_to_string(&target).unwrap(), "new manual");
        assert!(!manual_temporary_path(&target).exists());
    }

    #[test]
    fn manual_export_rejects_a_target_replaced_after_binding() {
        let directory = tempfile::tempdir().expect("应建立目标身份测试根");
        let target = directory.path().join("manual.toml");
        let moved = directory.path().join("original.toml");
        fs::write(&target, "original").expect("应建立原目标");
        let bound = bind_manual_output_target(&target).expect("应固定原目标身份");
        let expected_target = bound.initial_identity().expect("原目标必须存在");

        fs::rename(&target, &moved).expect("绑定只固定父链，不应掩盖目标身份变化测试");
        fs::write(&target, "other writer").expect("应在同一路径换入另一文件");
        let temporary = manual_temporary_path(bound.resolved_path());
        fs::write(&temporary, "new manual").expect("应建立待发布临时文件");
        let temporary_file = File::open(&temporary).expect("应打开临时文件");
        let temporary_identity = FileIdentity::of(&temporary_file, &temporary).unwrap();
        drop(temporary_file);

        let error = rename_with_replace_if_identity(
            &temporary,
            bound.resolved_path(),
            temporary_identity,
            expected_target,
        )
        .expect_err("目标身份变化后不得覆盖另一文件");

        assert!(matches!(error, WindowsFsError::FileIdentityChanged { .. }));
        assert_eq!(fs::read_to_string(&target).unwrap(), "other writer");
        assert_eq!(fs::read_to_string(&moved).unwrap(), "original");
        assert_eq!(fs::read_to_string(&temporary).unwrap(), "new manual");
    }

    #[test]
    fn manual_output_binding_keeps_the_parent_chain_pinned() {
        let directory = tempfile::tempdir().expect("应建立父链固定测试根");
        let parent = directory.path().join("stable");
        let moved = directory.path().join("moved");
        fs::create_dir(&parent).expect("应建立输出父目录");
        let bound = bind_manual_output_target(&parent.join("manual.toml"))
            .expect("应固定缺失目标的父目录链");

        fs::rename(&parent, &moved).expect_err("绑定存活期间不得替换输出父目录");
        assert!(parent.is_dir());
        drop(bound);
        fs::rename(&parent, &moved).expect("释放绑定后父目录应可移动");
    }

    #[test]
    fn output_target_diagnostics_keep_directory_and_reparse_facts() {
        let localizer = UiLocalizer::new(crate::i18n::UiLocale::SimplifiedChinese);
        let target = PathBuf::from(r"C:\game\manual.toml");
        let cases = [
            (
                ManualOutputTargetProblem::NotRegularFile {
                    path: target.clone(),
                },
                "现有目标不是普通文件",
            ),
            (
                ManualOutputTargetProblem::ReparsePoint {
                    path: target.clone(),
                },
                "路径包含不能信任的重解析点",
            ),
        ];

        for (problem, expected_reason) in cases {
            let mut stderr = Vec::new();
            render_manual_document_issues(
                &ManualDocumentError::OutputTarget { problem },
                &localizer,
                &mut stderr,
            )
            .unwrap();
            let stderr = String::from_utf8(stderr).unwrap();
            assert!(stderr.contains(expected_reason));
            assert!(stderr.contains("修正指出的输入后重试"));
            assert!(!stderr.contains("权限"));
            assert!(stderr.contains(&render_state_effect_impact(
                StateEffect::Unchanged,
                &localizer,
            )));
        }
    }

    #[test]
    fn rpg_maker_ownership_jsonl_follows_manual_order_and_has_only_public_fields() {
        let directory = tempfile::tempdir().expect("应建立所有权导出测试目录");
        let ownership = directory.path().join("ownership.jsonl");
        let mut builtin = indexed_entry(ManualTranslationType::Fixed, &["Actor"]);
        builtin.id = "Actors.json:1:name".to_owned();
        let mut rules = indexed_entry(ManualTranslationType::Free, &["Quest"]);
        rules.id = "plugins.js:Quest:Title".to_owned();
        rules.rpg_maker_owner = Some(ManualRpgMakerOwner::Rules { rule_number: 7 });
        let index = ManualTranslationIndex::new(vec![builtin, rules]).expect("测试 ID 必须唯一");

        let count = export_ownership_document_with_cancellation(
            &ownership,
            &index,
            &CooperativeCancellation::default(),
        )
        .expect("所有权清单应导出");

        assert_eq!(count, 2);
        assert_eq!(
            fs::read_to_string(&ownership).expect("所有权 JSONL 应可读取"),
            concat!(
                "{\"manual_id\":\"Actors.json:1:name\",\"owner\":\"builtin\"}\n",
                "{\"manual_id\":\"plugins.js:Quest:Title\",\"owner\":\"rules\",\"rule_number\":7}\n",
            )
        );
    }

    #[test]
    fn translation_export_keeps_rejected_candidates_opaque_and_one_record_per_line() {
        const FORMATTED_DUPLICATE_CANDIDATE: &str =
            "{\n  \"translation\": [\"候选\"],\n  \"translation\": null\n}";
        let directory = tempfile::tempdir().expect("应建立 Translation export 测试目录");
        let export = directory.path().join("translations.jsonl");

        let mut current = indexed_entry(ManualTranslationType::Fixed, &["Current"]);
        current.id = "Actors.json:1:name".to_owned();
        current.current_translation = Some(vec!["当前译文".to_owned()]);
        current.origin = Some(ManualTranslationOrigin::Automatic);

        let mut pending = indexed_entry(ManualTranslationType::Fixed, &["Pending"]);
        pending.id = "Actors.json:2:name".to_owned();

        let mut rejected = indexed_entry(ManualTranslationType::Fixed, &["Rejected"]);
        rejected.id = "Actors.json:3:name".to_owned();
        rejected.rejected = Some(ManualRejectedCandidate {
            origin: ManualTranslationOrigin::Automatic,
            candidate_json: FORMATTED_DUPLICATE_CANDIDATE.to_owned(),
            translation: None,
            violation: ProvenInvariantViolation::InvalidCandidateShape,
        });

        let mut rejected_null = indexed_entry(ManualTranslationType::Fixed, &["Rejected null"]);
        rejected_null.id = "Actors.json:4:name".to_owned();
        rejected_null.rejected = Some(ManualRejectedCandidate {
            origin: ManualTranslationOrigin::Automatic,
            candidate_json: "null".to_owned(),
            translation: None,
            violation: ProvenInvariantViolation::InvalidCandidateShape,
        });

        let index = ManualTranslationIndex::new(vec![current, pending, rejected, rejected_null])
            .expect("测试 ID 必须唯一");
        let count = export_translation_document_with_cancellation(
            &export,
            &index,
            &CooperativeCancellation::default(),
        )
        .expect("Translation export 应成功");

        assert_eq!(count, 4);
        let document = fs::read_to_string(&export).expect("Translation export 应可读取");
        let lines = document.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 4);
        let rows = lines
            .iter()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).expect("每行必须是完整 JSON")
            })
            .collect::<Vec<_>>();

        assert_eq!(rows[0]["state"], "current");
        assert_eq!(rows[0]["translation"], serde_json::json!(["当前译文"]));
        assert!(rows[0].get("rejected_candidate_json").is_none());
        assert_eq!(rows[1]["state"], "pending");
        assert!(rows[1]["translation"].is_null());
        assert!(rows[1].get("rejected_candidate_json").is_none());
        assert_eq!(rows[2]["state"], "rejected");
        assert!(rows[2]["translation"].is_null());
        assert_eq!(
            rows[2]["rejected_candidate_json"].as_str(),
            Some(FORMATTED_DUPLICATE_CANDIDATE)
        );
        assert_eq!(rows[3]["state"], "rejected");
        assert!(rows[3]["translation"].is_null());
        assert_eq!(rows[3]["rejected_candidate_json"].as_str(), Some("null"));
    }

    #[test]
    fn toml_requires_arrays_and_rejects_unknown_fields() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("manual.toml");
        let index = ManualTranslationIndex::new(vec![indexed_entry(
            ManualTranslationType::Fixed,
            &["Tails Stomp"],
        )])
        .unwrap();
        write_document(
            &path,
            "[[translation]]\nid = \"Skills.json:798:name\"\ntype = \"fixed\"\nsource = [\"Tails Stomp\"]\ntranslation = []\n",
        );
        let report = check_manual_document(&path, &index, |_, _| Ok(())).unwrap();
        assert_eq!(report.unfilled, 1);
        assert!(report.is_valid());

        for invalid in [
            "[[translation]]\nid = \"Skills.json:798:name\"\ntype = \"fixed\"\nsource = \"Tails Stomp\"\ntranslation = []\n",
            "[[translation]]\nid = \"Skills.json:798:name\"\ntype = \"fixed\"\nsource = [\"Tails Stomp\"]\ntranslation = []\ncontext = \"hidden\"\n",
            "[[translation]]\nid = \"Skills.json:798:name\"\ntype = \"strict\"\nsource = [\"Tails Stomp\"]\ntranslation = []\n",
        ] {
            write_document(&path, invalid);
            assert!(matches!(
                check_manual_document(&path, &index, |_, _| Ok(())),
                Err(ManualDocumentError::InvalidToml { .. })
            ));
        }
    }

    #[test]
    fn empty_document_is_valid_and_empty_export_has_no_template_noise() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("manual.toml");
        let index = ManualTranslationIndex::new(Vec::new()).unwrap();

        assert_eq!(export_manual_document(&path, &index).unwrap(), 0);
        assert_eq!(fs::read_to_string(&path).unwrap(), "");
        let report = check_manual_document(&path, &index, |_, _| Ok(())).unwrap();
        assert_eq!(report, ManualCheckReport::default());
    }

    #[test]
    fn duplicate_unknown_and_control_character_entries_are_reported_by_readable_id() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("manual.toml");
        let index = ManualTranslationIndex::new(vec![indexed_entry(
            ManualTranslationType::Fixed,
            &["source"],
        )])
        .unwrap();
        write_document(
            &path,
            concat!(
                "[[translation]]\n",
                "id = \"Skills.json:798:name\"\n",
                "type = \"fixed\"\n",
                "source = [\"source\"]\n",
                "translation = [\"bad\\nline\"]\n\n",
                "[[translation]]\n",
                "id = \"Skills.json:798:name\"\n",
                "type = \"fixed\"\n",
                "source = [\"source\"]\n",
                "translation = [\"译文\"]\n\n",
                "[[translation]]\n",
                "id = \"Unknown.json:1:name\"\n",
                "type = \"fixed\"\n",
                "source = [\"source\"]\n",
                "translation = [\"译文\"]\n",
            ),
        );

        let report = check_manual_document(&path, &index, |_, _| Ok(())).unwrap();
        assert_eq!(report.errors.len(), 3);
        assert_eq!(report.errors[0].id, "Skills.json:798:name");
        assert!(matches!(
            report.errors[0].problem,
            ManualCheckProblem::InvalidTranslationLine { line: 1 }
        ));
        assert_eq!(report.errors[1].problem, ManualCheckProblem::DuplicateEntry);
        assert_eq!(report.errors[2].id, "Unknown.json:1:name");
        assert_eq!(report.errors[2].problem, ManualCheckProblem::UnknownId);
    }

    #[test]
    fn source_array_controls_and_placeholder_failures_are_structural_errors() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("manual.toml");
        let index = ManualTranslationIndex::new(vec![indexed_entry(
            ManualTranslationType::Fixed,
            &["source"],
        )])
        .unwrap();
        write_document(
            &path,
            "[[translation]]\nid = \"Skills.json:798:name\"\ntype = \"fixed\"\nsource = [\"source\\n\"]\ntranslation = [\"译文\"]\n",
        );
        let report = check_manual_document(&path, &index, |_, _| Ok(())).unwrap();
        assert_eq!(report.errors.len(), 1);
        assert!(matches!(
            report.errors[0].problem,
            ManualCheckProblem::InvalidSourceLine { line: 1 }
        ));

        write_document(
            &path,
            "[[translation]]\nid = \"Skills.json:798:name\"\ntype = \"fixed\"\nsource = [\"source\"]\ntranslation = [\"译文\"]\n",
        );
        let report = check_manual_document(&path, &index, |_, _| {
            Err("译文没有保留原文中的 Placeholder".to_owned())
        })
        .unwrap();
        assert_eq!(report.errors.len(), 1);
        assert_eq!(
            report.errors[0].problem,
            ManualCheckProblem::PlaceholderMismatch
        );
    }

    #[test]
    fn fixed_requires_length_and_empty_slots_while_free_may_reflow() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("manual.toml");
        let fixed = ManualTranslationIndex::new(vec![indexed_entry(
            ManualTranslationType::Fixed,
            &["first", "", "third"],
        )])
        .unwrap();
        write_document(
            &path,
            "[[translation]]\nid = \"Skills.json:798:name\"\ntype = \"fixed\"\nsource = [\"first\", \"\", \"third\"]\ntranslation = [\"一\", \"错误\", \"三\"]\n",
        );
        let report = check_manual_document(&path, &fixed, |_, _| Ok(())).unwrap();
        assert_eq!(report.errors.len(), 1);
        assert!(matches!(
            report.errors[0].problem,
            ManualCheckProblem::FixedBlankSlot { slot: 2 }
        ));

        let free = ManualTranslationIndex::new(vec![indexed_entry(
            ManualTranslationType::Free,
            &["first", "second"],
        )])
        .unwrap();
        write_document(
            &path,
            "[[translation]]\nid = \"Skills.json:798:name\"\ntype = \"free\"\nsource = [\"first\", \"second\"]\ntranslation = [\"合并译文\"]\n",
        );
        let report = check_manual_document(&path, &free, |_, _| Ok(())).unwrap();
        assert_eq!(report.valid, 1);
        assert!(report.is_valid());
    }

    #[test]
    fn fixed_requires_blank_source_slots_to_have_exactly_empty_translations() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("manual.toml");
        for blank_source in ["", " "] {
            let fixed = ManualTranslationIndex::new(vec![indexed_entry(
                ManualTranslationType::Fixed,
                &["first", blank_source, "third"],
            )])
            .unwrap();
            write_document(
                &path,
                &format!(
                    "[[translation]]\nid = \"Skills.json:798:name\"\ntype = \"fixed\"\nsource = [\"first\", \"{blank_source}\", \"third\"]\ntranslation = [\"一\", \"错误\", \"三\"]\n"
                ),
            );

            let report = check_manual_document(&path, &fixed, |_, _| Ok(())).unwrap();
            assert_eq!(report.errors.len(), 1);
            assert!(matches!(
                report.errors[0].problem,
                ManualCheckProblem::FixedBlankSlot { slot: 2 }
            ));

            write_document(
                &path,
                &format!(
                    "[[translation]]\nid = \"Skills.json:798:name\"\ntype = \"fixed\"\nsource = [\"first\", \"{blank_source}\", \"third\"]\ntranslation = [\"一\", \"\", \"三\"]\n"
                ),
            );
            let report = check_manual_document(&path, &fixed, |_, _| Ok(())).unwrap();
            assert_eq!(report.valid, 1);
            assert!(report.is_valid());
        }
    }

    #[test]
    fn fixed_manual_translation_rejects_blank_replacement_for_nonblank_source() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("manual.toml");
        let index = ManualTranslationIndex::new(vec![indexed_entry(
            ManualTranslationType::Fixed,
            &["source"],
        )])
        .unwrap();
        write_document(
            &path,
            "[[translation]]\nid = \"Skills.json:798:name\"\ntype = \"fixed\"\nsource = [\"source\"]\ntranslation = [\"\"]\n",
        );

        let report = check_manual_document(&path, &index, |_, _| Ok(())).unwrap();

        assert_eq!(report.valid, 0);
        assert!(report.writes.is_empty());
        assert_eq!(report.errors.len(), 1);
        assert_eq!(
            report.errors[0].problem,
            ManualCheckProblem::EmptyTranslation
        );
    }

    #[test]
    fn export_does_not_overwrite_or_remove_another_writers_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("manual.toml");
        let temporary = directory.path().join(".manual.toml.tmp");
        fs::write(&temporary, "other writer").unwrap();
        let index = ManualTranslationIndex::new(vec![indexed_entry(
            ManualTranslationType::Fixed,
            &["source"],
        )])
        .unwrap();

        let error = export_manual_document(&path, &index).unwrap_err();

        assert!(matches!(
            &error,
            ManualDocumentError::ExistingTemporary { path }
                if path == &temporary.canonicalize().unwrap()
        ));
        assert_eq!(fs::read_to_string(temporary).unwrap(), "other writer");
        assert!(!path.exists());

        let localizer = UiLocalizer::new(crate::i18n::UiLocale::SimplifiedChinese);
        let mut stderr = Vec::new();
        render_manual_command_error(
            &ManualCommandError::from_document(error),
            &localizer,
            &mut stderr,
        )
        .unwrap();
        let stderr = String::from_utf8(stderr).unwrap();
        assert!(stderr.contains(&render_state_effect_impact(
            StateEffect::RecoveryRequired,
            &localizer,
        )));
    }

    #[test]
    fn export_preserves_the_primary_failure_when_temporary_cleanup_also_fails() {
        let directory = tempfile::tempdir().unwrap();
        let temporary = directory.path().join(".manual.toml.tmp");
        let file = File::create(&temporary).unwrap();

        let source =
            cleanup_open_manual_temporary(&file, &temporary, ManualDocumentError::Cancelled);

        let localizer = UiLocalizer::new(crate::i18n::UiLocale::SimplifiedChinese);
        let expected_cleanup_reason = match &source {
            ManualDocumentError::TemporaryCleanup {
                operation,
                temporary: actual,
                cleanup,
            } => {
                assert!(matches!(operation.as_ref(), ManualDocumentError::Cancelled));
                assert_eq!(actual, &temporary);
                manual_temporary_cleanup_reason(cleanup, &localizer)
            }
            other => panic!("应保留主取消与临时文件清理失败，实际为 {other:?}"),
        };
        let generic_cleanup_reason = localizer.format(UiMessage::DiagnosticFailureValue {
            code: "operation_failed",
        });
        assert_eq!(expected_cleanup_reason, generic_cleanup_reason);

        let error = ManualCommandError::from_document(source);
        assert!(!error.is_cancelled(), "清理失败不得报告为干净取消");
        let mut stderr = Vec::new();
        render_manual_command_error(&error, &localizer, &mut stderr).unwrap();
        let stderr = String::from_utf8(stderr).unwrap();
        assert!(stderr.contains(".manual.toml.tmp"));
        assert_eq!(
            stderr
                .matches(&localizer.format(UiMessage::DiagnosticErrorHeading))
                .count(),
            1,
            "临时文件清理失败是主错误的相关 cleanup，不得再次显示主错误标题：{stderr}"
        );
        assert!(
            stderr.contains(&localizer.format(UiMessage::DiagnosticRelated {
                relation: RelatedFailureRelation::Cleanup.as_str(),
            }))
        );
        assert!(
            stderr.contains(
                &localizer.format(UiMessage::DiagnosticFailureValue { code: "cancelled" })
            )
        );
        assert!(stderr.contains(&expected_cleanup_reason));
        assert!(
            stderr.contains(&localizer.format(UiMessage::DiagnosticResolutionValue {
                code: "preserve_recovery_artifacts",
            }))
        );
        assert!(stderr.contains(&render_state_effect_impact(
            StateEffect::RecoveryRequired,
            &localizer,
        )));
    }

    #[test]
    fn failed_export_cleanup_keeps_the_candidate_exclusive_until_exact_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let temporary = directory.path().join(".manual.toml.tmp");
        let moved = directory.path().join("original.tmp");
        let file = create_new_atomic_replace_candidate(&temporary).unwrap();
        fs::rename(&temporary, &moved).expect_err("独占候选句柄存活时不得被移走");

        let source =
            cleanup_open_manual_temporary(&file, &temporary, ManualDocumentError::Cancelled);

        assert!(matches!(source, ManualDocumentError::Cancelled));
        drop(file);
        assert!(!temporary.exists());
        assert!(!moved.exists());
    }

    #[test]
    fn temporary_identity_failure_deletes_the_exact_open_candidate() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("manual.toml");
        let temporary = manual_temporary_path(&target);
        let file = create_new_atomic_replace_candidate(&temporary).unwrap();
        let identity_error = WindowsFsError::Io {
            operation: "测试读取候选身份",
            path: temporary.clone(),
            source: io::Error::other("forced identity failure"),
        };

        let error = manual_temporary_identity(&file, &target, &temporary, Err(identity_error))
            .expect_err("身份读取失败必须返回原始写入错误");
        assert!(matches!(error, ManualDocumentError::Write { .. }));
        drop(file);
        assert!(!temporary.exists(), "失败后不得遗留未确认身份的候选");
    }

    #[test]
    fn replace_outcome_unknown_preserves_candidate_and_reports_unknown_effect() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("manual.toml");
        let temporary = manual_temporary_path(&target);
        fs::write(&temporary, "candidate").unwrap();
        let error = manual_replace_error(
            &target,
            WindowsFsError::RenameTargetUnconfirmed {
                path: target.clone(),
            },
        );

        assert!(matches!(
            error,
            ManualDocumentError::ReplaceOutcomeUnknown { .. }
        ));
        assert_eq!(fs::read_to_string(&temporary).unwrap(), "candidate");
        assert_eq!(
            ManualCommandError::from_document(error)
                .diagnostic_report()
                .effect(),
            StateEffect::OutcomeUnknown
        );
    }

    #[test]
    fn requested_cancellation_stops_apply_before_the_callback() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("manual.toml");
        let index = ManualTranslationIndex::new(vec![indexed_entry(
            ManualTranslationType::Fixed,
            &["source"],
        )])
        .unwrap();
        write_document(
            &path,
            "[[translation]]\nid = \"Skills.json:798:name\"\ntype = \"fixed\"\nsource = [\"source\"]\ntranslation = [\"译文\"]\n",
        );
        let snapshot = ManualProjectSnapshot {
            index,
            placeholders: test_placeholder_validator(),
        };
        let cancellation = CooperativeCancellation::default();
        cancellation.request();
        let mut called = false;

        let result = apply_manual_snapshot(&path, &snapshot, &cancellation, |_| {
            called = true;
            Ok(0)
        });

        assert!(matches!(result, Err(ManualCommandError::Cancelled)));
        assert!(!called);
    }

    #[test]
    fn generic_schema_sqlite_failure_remains_a_database_access_error() {
        let error = manual_generic_schema_error(GenericProjectError::Sqlite {
            operation: "读取 Generic schema",
            source: rusqlite::Error::QueryReturnedNoRows,
        });

        assert!(matches!(
            error,
            ManualDatabaseError::Sqlite(rusqlite::Error::QueryReturnedNoRows)
        ));
    }

    #[test]
    fn requested_cancellation_interrupts_sqlite_work_and_maps_to_cancelled() {
        let connection = Connection::open_in_memory().expect("应建立测试数据库");
        let cancellation = CooperativeCancellation::default();
        install_manual_sqlite_cancellation(&connection, &cancellation)
            .expect("应安装 SQLite 取消检查");
        cancellation.request();

        let source = connection
            .query_row(
                "WITH RECURSIVE numbers(value) AS (
                     VALUES(1) UNION ALL SELECT value + 1 FROM numbers WHERE value < 1000000
                 ) SELECT sum(value) FROM numbers",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect_err("已请求取消的长查询必须中断");
        assert_eq!(
            source.sqlite_error_code(),
            Some(rusqlite::ErrorCode::OperationInterrupted)
        );
        let running = CooperativeCancellation::default();
        assert!(matches!(
            manual_command_database_error(ManualDatabaseError::Sqlite(source), &running),
            ManualCommandError::Cancelled
        ));
    }

    #[test]
    fn requested_cancellation_does_not_hide_an_unrelated_database_failure() {
        let cancellation = CooperativeCancellation::default();
        cancellation.request();

        let error = manual_command_database_error(
            ManualDatabaseError::Sqlite(rusqlite::Error::QueryReturnedNoRows),
            &cancellation,
        );

        assert!(matches!(
            error,
            ManualCommandError::Database(ManualDatabaseError::Sqlite(
                rusqlite::Error::QueryReturnedNoRows
            ))
        ));
    }

    #[test]
    fn generic_manual_check_and_apply_ignore_invalid_detached_records() {
        let directory = tempfile::tempdir().expect("应建立测试目录");
        let database = directory.path().join("project.db");
        let connection = Connection::open(&database).expect("应建立 Generic 测试数据库");
        create_current_generic_schema_for_test(&connection).expect("应建立当前 Generic schema");
        connection
            .execute_batch(
                "PRAGMA ignore_check_constraints = ON;
                 INSERT INTO generic_manual_translation VALUES (
                     'detached', 'unit', 'detached-id',
                     'invalid source', 'invalid translation', X'00'
                 );
                 PRAGMA ignore_check_constraints = OFF;",
            )
            .expect("应写入 raw Lua 可留下的无效脱离记录");
        drop(connection);
        let document = directory.path().join("manual.toml");
        fs::write(&document, "").expect("应建立空 Manual 文件");
        let cancellation = CooperativeCancellation::default();

        let checked = execute_generic_manual_command(
            &database,
            ManualOperation::Check,
            &document,
            None,
            None,
            &cancellation,
        )
        .expect("Manual check 不应读取脱离当前位置的记录");
        assert!(matches!(checked, ManualCommandSummary::Checked { .. }));

        let applied = execute_generic_manual_command(
            &database,
            ManualOperation::Apply,
            &document,
            None,
            None,
            &cancellation,
        )
        .expect("Manual apply 不应读取脱离当前位置的记录");
        assert!(matches!(
            applied,
            ManualCommandSummary::Applied { applied: 0, .. }
        ));
    }

    #[test]
    fn generic_manual_rejected_uses_content_applicability_not_historical_readable_id() {
        let connection = Connection::open_in_memory().expect("应建立 Generic 测试数据库");
        create_current_generic_schema_for_test(&connection).expect("应建立当前 Generic schema");
        let relative_path = Path::new("renamed.jsonl")
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        let group_context = Sha256Fingerprint::from_bytes([81; 32]);
        let current_state = crate::translation::generic_automatic_applicability_v2(
            "ja",
            "zh-Hans",
            "g",
            "u",
            "原文",
            group_context,
        );
        connection
            .execute(
                "INSERT INTO generic_project (
                     singleton, project_name, source_root, source_language, target_language
                 ) VALUES (1, 'game', X'0100', 'ja', 'zh-Hans')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO generic_file (relative_path, ordinal) VALUES (?1, 0)",
                [relative_path],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO generic_group (
                     group_id, relative_path, ordinal, kind, context_fingerprint
                 ) SELECT 'g', relative_path, 0, 'dialogue', ?1 FROM generic_file",
                [group_context.as_bytes().as_slice()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO generic_unit (group_id, unit_id, ordinal, source_text)
                 VALUES ('g', 'u', 0, '原文')",
                [],
            )
            .unwrap();
        let violation = serde_json::to_string(&ProvenInvariantViolation::InvalidCandidateShape)
            .expect("违反项应可编码");
        connection
            .execute(
                "INSERT INTO generic_rejected_translation (
                     group_id, unit_id, readable_id, origin, source_json, candidate_json,
                     translation_shape, group_context, violation_json, planning_state
                 ) VALUES ('g', 'u', 'old/path.jsonl:line9:unit9:text', 'automatic',
                           '[\"原文\"]', '[\"旧候选\"]', 'free', ?1, ?2, ?3)",
                params![
                    group_context.as_bytes().as_slice(),
                    violation,
                    current_state.as_bytes().as_slice(),
                ],
            )
            .unwrap();
        let service = GenericPlaceholderService::default();
        let rules = service.compile(Vec::new()).unwrap();

        let current = load_generic_entries(&connection, &service, &rules, None).unwrap();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].id, "renamed.jsonl:line1:unit1:text");
        assert_eq!(
            current[0]
                .rejected
                .as_ref()
                .and_then(|rejected| rejected.translation.as_deref()),
            Some(["旧候选".to_owned()].as_slice()),
            "历史 readable_id 变化不得隐藏仍适用的 Rejected"
        );
        connection
            .execute(
                "UPDATE generic_project SET target_language = 'en' WHERE singleton = 1",
                [],
            )
            .unwrap();
        let other_language = load_generic_entries(&connection, &service, &rules, None).unwrap();
        assert!(other_language[0].rejected.is_none());

        connection
            .execute(
                "UPDATE generic_project SET target_language = 'zh-Hans' WHERE singleton = 1",
                [],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE generic_group SET context_fingerprint = ?1 WHERE group_id = 'g'",
                [Sha256Fingerprint::from_bytes([83; 32])
                    .as_bytes()
                    .as_slice()],
            )
            .unwrap();
        let other_context = load_generic_entries(&connection, &service, &rules, None).unwrap();
        assert!(other_context[0].rejected.is_none());
    }

    #[test]
    fn generic_manual_commands_reject_a_trigger_attached_to_a_managed_table() {
        let directory = tempfile::tempdir().expect("应建立 Generic schema 测试目录");
        let database = directory.path().join("project.db");
        let connection = Connection::open(&database).expect("应建立 Generic 测试数据库");
        create_current_generic_schema_for_test(&connection).expect("应建立当前 Generic schema");
        let relative_path = Path::new("input.jsonl")
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        connection
            .execute(
                "INSERT INTO generic_file (relative_path, ordinal) VALUES (?1, 0)",
                [relative_path],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO generic_group (
                     group_id, relative_path, ordinal, kind, context_fingerprint
                 ) SELECT 'g', relative_path, 0, 'text', zeroblob(32) FROM generic_file",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO generic_unit (
                     group_id, unit_id, ordinal, source_text
                 ) VALUES ('g', 'u', 0, '原文')",
                [],
            )
            .unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER delete_unit_after_manual_insert
                 AFTER INSERT ON generic_manual_translation
                 BEGIN
                     DELETE FROM generic_unit;
                 END;",
            )
            .expect("raw Lua 应能留下附着于受管表的 trigger");
        drop(connection);
        let document = directory.path().join("manual.toml");
        write_document(
            &document,
            "[[translation]]\nid = \"input.jsonl:line1:unit1:text\"\ntype = \"free\"\nsource = [\"原文\"]\ntranslation = [\"译文\"]\n",
        );
        let cancellation = CooperativeCancellation::default();

        for operation in [ManualOperation::Check, ManualOperation::Apply] {
            let error = execute_generic_manual_command(
                &database,
                operation,
                &document,
                None,
                None,
                &cancellation,
            )
            .expect_err("普通 Manual 命令必须拒绝被修改的精确 schema");
            assert!(matches!(
                error,
                ManualCommandError::Database(ManualDatabaseError::InvalidProject(_))
            ));
        }

        let connection = Connection::open(&database).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM generic_unit", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM generic_manual_translation",
                    [],
                    |row| { row.get::<_, i64>(0) }
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn generic_manual_apply_clears_current_rejected_candidate_in_the_same_transaction() {
        let connection = Connection::open_in_memory().expect("应建立 Generic 测试数据库");
        connection
            .execute_batch(
                "CREATE TABLE generic_unit (
                     group_id TEXT, unit_id TEXT, translation TEXT, translation_state BLOB
                 );
                 CREATE TABLE generic_manual_translation (
                     group_id TEXT, unit_id TEXT, readable_id TEXT,
                     source_json TEXT, translation_json TEXT,
                     applicability_fingerprint BLOB,
                     PRIMARY KEY (group_id, unit_id)
                 );
                 CREATE TABLE generic_rejected_translation (
                     group_id TEXT, unit_id TEXT, candidate_json TEXT,
                     PRIMARY KEY (group_id, unit_id)
                 );
                 INSERT INTO generic_unit VALUES ('g', 'u', NULL, NULL);
                 INSERT INTO generic_rejected_translation VALUES ('g', 'u', 'true');",
            )
            .expect("应建立含当前被拒候选的 Generic 数据库");
        let applicability = Sha256Fingerprint::from_bytes([7; 32]);
        let write = ValidatedManualTranslation {
            id: "input.jsonl:line1:unit1:text".to_owned(),
            kind: ManualTranslationType::Free,
            source: vec!["原文".to_owned()],
            translation: vec!["译文".to_owned()],
            locator: ManualTranslationLocator::Generic {
                group_id: "g".to_owned(),
                unit_id: "u".to_owned(),
            },
            applicability,
        };

        connection
            .execute_batch("BEGIN IMMEDIATE")
            .expect("应开始测试事务");
        apply_generic_manual_translations(&connection, std::slice::from_ref(&write))
            .expect("人工译文与被拒候选清理应共同成功");
        connection.execute_batch("COMMIT").expect("应提交测试事务");

        let manual: i64 = connection
            .query_row(
                "SELECT count(*) FROM generic_manual_translation WHERE group_id = 'g' AND unit_id = 'u'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let rejected: i64 = connection
            .query_row(
                "SELECT count(*) FROM generic_rejected_translation WHERE group_id = 'g' AND unit_id = 'u'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(manual, 1);
        assert_eq!(rejected, 0);
    }

    #[test]
    fn rpg_maker_manual_check_and_apply_ignore_invalid_detached_records() {
        let directory = tempfile::tempdir().expect("应建立测试目录");
        let database = directory.path().join("project.db");
        let connection = Connection::open(&database).expect("应建立 RPG Maker 测试数据库");
        create_current_rpg_maker_schema_for_test(&connection).expect("应建立当前 RPG Maker schema");
        connection
            .execute_batch(
                "PRAGMA ignore_check_constraints = ON;
                 INSERT INTO rpg_maker_manual_translation VALUES (
                     'detached', 'location', 'role', 'detached-id', 'invalid',
                     'invalid source', 'invalid translation', X'00'
                 );
                 PRAGMA ignore_check_constraints = OFF;",
            )
            .expect("应写入 raw Lua 可留下的无效脱离记录");
        drop(connection);
        let document = directory.path().join("manual.toml");
        fs::write(&document, "").expect("应建立空 Manual 文件");
        let cancellation = CooperativeCancellation::default();

        let checked = execute_rpg_maker_manual_command(
            &database,
            RpgMakerEngine::Mz,
            ManualOperation::Check,
            &document,
            None,
            None,
            &cancellation,
        )
        .expect("Manual check 不应读取脱离当前位置的记录");
        assert!(matches!(checked, ManualCommandSummary::Checked { .. }));

        let applied = execute_rpg_maker_manual_command(
            &database,
            RpgMakerEngine::Mz,
            ManualOperation::Apply,
            &document,
            None,
            None,
            &cancellation,
        )
        .expect("Manual apply 不应读取脱离当前位置的记录");
        assert!(matches!(
            applied,
            ManualCommandSummary::Applied { applied: 0, .. }
        ));
    }

    #[test]
    fn rpg_maker_manual_applicability_uses_the_complete_cross_owner_group() {
        let connection = Connection::open_in_memory().expect("应建立内存数据库");
        create_current_rpg_maker_schema_for_test(&connection).expect("应建立当前 schema");
        connection
            .execute_batch(
                "INSERT INTO metadata VALUES ('game', 'ja', 'zh-Hans', zeroblob(32));
                 INSERT INTO rpg_maker_asset_owner_state VALUES
                    ('builtin', zeroblob(32), zeroblob(32)),
                    ('rules', zeroblob(32), zeroblob(32));",
            )
            .expect("应建立项目语言事实");
        let location = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(1)],
        );
        let location_raw = RpgMakerLocationCodec::encode(&location).expect("位置应可编码");
        let group_order = RpgMakerSemanticOrderKey::new(vec![1], 0)
            .encode()
            .expect("Group 顺序应可编码");
        let builtin_order = RpgMakerSemanticOrderKey::new(vec![1], 1)
            .encode()
            .expect("Builtin Unit 顺序应可编码");
        let rules_order = RpgMakerSemanticOrderKey::new(vec![1], 2)
            .encode()
            .expect("Rules Unit 顺序应可编码");
        let builtin_role = RpgMakerProjectionCodec::encode_role(&TextUnitRole::Scalar(
            ScalarFieldKey::new("name").expect("Builtin role 应合法"),
        ))
        .expect("Builtin role 应可编码");
        let rules_role = RpgMakerProjectionCodec::encode_role(&TextUnitRole::Scalar(
            ScalarFieldKey::new("note").expect("Rules role 应合法"),
        ))
        .expect("Rules role 应可编码");
        for (owner, group_id, role, rule_number, order, source) in [
            (
                "builtin",
                1_i64,
                builtin_role.as_str(),
                None,
                builtin_order.clone(),
                r#""名称""#,
            ),
            (
                "rules",
                1_i64,
                rules_role.as_str(),
                Some(1_i64),
                rules_order.clone(),
                r#""备注""#,
            ),
        ] {
            connection
                .execute(
                    "INSERT INTO rpg_maker_text_group VALUES (?1, ?2, ?3, ?4, 'database_entry', '[]')",
                    params![owner, group_id, &location_raw, &group_order],
                )
                .expect("跨 owner Group 应可写入");
            connection
                .execute(
                    "INSERT INTO rpg_maker_text_unit (
                         owner, group_id, unit_role, rule_number, semantic_order_key,
                         source_content_json, source_context_json
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, '{}')",
                    params![owner, group_id, role, rule_number, order, source],
                )
                .expect("跨 owner Unit 应可写入");
        }

        let actual = load_rpg_maker_automatic_applicability(&connection)
            .expect("Manual 应建立跨 owner 完整 Group 适用性");
        let complete_context = crate::translation::rpg_maker_group_source_context_v2(
            "database_entry",
            [
                (
                    builtin_role.as_str(),
                    builtin_order.as_slice(),
                    r#""名称""#,
                    "{}",
                ),
                (
                    rules_role.as_str(),
                    rules_order.as_slice(),
                    r#""备注""#,
                    "{}",
                ),
            ]
            .into_iter(),
        );
        let builtin = actual
            .get(&(
                "builtin".to_owned(),
                location_raw.clone(),
                builtin_role.clone(),
            ))
            .expect("Builtin Unit 应有适用性");
        assert_eq!(
            builtin.automatic,
            crate::translation::rpg_maker_automatic_applicability_v2(
                "ja",
                "zh-Hans",
                "builtin",
                "database_entry",
                &location_raw,
                &builtin_role,
                "[]",
                r#""名称""#,
                "{}",
                complete_context,
            )
        );
        let owner_only = crate::translation::rpg_maker_group_source_context_v2(
            "database_entry",
            [(
                builtin_role.as_str(),
                builtin_order.as_slice(),
                r#""名称""#,
                "{}",
            )]
            .into_iter(),
        );
        assert_ne!(
            complete_context, owner_only,
            "Rules sibling 必须进入 Manual/Lua Group 事实"
        );
    }

    #[test]
    fn rpg_maker_manual_commands_reject_a_trigger_attached_to_a_managed_table() {
        let directory = tempfile::tempdir().expect("应建立 RPG Maker schema 测试目录");
        let database = directory.path().join("project.db");
        let connection = Connection::open(&database).expect("应建立 RPG Maker 测试数据库");
        create_current_rpg_maker_schema_for_test(&connection).expect("应建立当前 RPG Maker schema");
        let location = RpgMakerLocation::value(
            RpgMakerSource::map(1),
            vec![RpgMakerLocationStep::key("displayName")],
        );
        let group_location = RpgMakerLocationCodec::encode(&location).unwrap();
        let role = TextUnitRole::Scalar(ScalarFieldKey::new("displayName").unwrap());
        let readable_id = readable_rpg_maker_id(&location, TextGroupKind::Map, &role);
        let role_json = RpgMakerProjectionCodec::encode_role(&role).unwrap();
        let recipe_json = RpgMakerProjectionCodec::encode_recipes(&[TextProjectionRecipe::Direct(
            DirectTextRecipe::new(
                location.clone(),
                "原文",
                vec![DirectTextPart::TextSlot { role: role.clone() }],
            )
            .unwrap(),
        )])
        .unwrap();
        let group_order = RpgMakerSemanticOrderKey::new(vec![0], 0).encode().unwrap();
        let unit_order = RpgMakerSemanticOrderKey::new(vec![0], 1).encode().unwrap();
        connection
            .execute_batch(
                "INSERT INTO metadata (
                     name, source_language, target_language, source_snapshot_fingerprint
                 ) VALUES ('game', 'ja', 'zh-Hans', zeroblob(32));
                 INSERT INTO rpg_maker_asset_owner_state
                     VALUES ('builtin', zeroblob(32), zeroblob(32));",
            )
            .expect("应建立 RPG Maker owner 状态");
        connection
            .execute(
                "INSERT INTO rpg_maker_text_group (
                     owner, group_id, group_location, semantic_order_key,
                     group_kind, projection_recipe_json
                 ) VALUES ('builtin', 1, ?1, ?2, 'map', ?3)",
                params![group_location, group_order, recipe_json],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO rpg_maker_text_unit (
                     owner, group_id, unit_role, rule_number, semantic_order_key,
                     source_content_json, source_context_json
                 ) VALUES ('builtin', 1, ?1, NULL, ?2, '\"原文\"', '{}')",
                params![role_json, unit_order],
            )
            .unwrap();
        drop(connection);
        let document = directory.path().join("manual.toml");
        write_document(
            &document,
            &format!(
                "[[translation]]\nid = {readable_id:?}\ntype = \"fixed\"\nsource = [\"原文\"]\ntranslation = [\"译文\"]\n"
            ),
        );
        let cancellation = CooperativeCancellation::default();

        let checked = execute_rpg_maker_manual_command(
            &database,
            RpgMakerEngine::Mz,
            ManualOperation::Check,
            &document,
            None,
            None,
            &cancellation,
        )
        .expect("合法 RPG Maker Manual 文件应先通过普通 check");
        assert!(matches!(
            checked,
            ManualCommandSummary::Checked {
                report: ManualCheckReport {
                    valid: 1,
                    errors,
                    ..
                },
                ..
            } if errors.is_empty()
        ));

        let connection = Connection::open(&database).expect("应重新打开 RPG Maker 测试数据库");
        connection
            .execute_batch(
                "CREATE TRIGGER delete_unit_after_manual_insert
                  AFTER INSERT ON rpg_maker_manual_translation
                  BEGIN
                      DELETE FROM rpg_maker_text_unit;
                  END;",
            )
            .expect("raw Lua 应能留下附着于受管表的 trigger");
        drop(connection);

        for operation in [ManualOperation::Check, ManualOperation::Apply] {
            let error = execute_rpg_maker_manual_command(
                &database,
                RpgMakerEngine::Mz,
                operation,
                &document,
                None,
                None,
                &cancellation,
            )
            .expect_err("普通 Manual 命令必须拒绝被修改的精确 schema");
            assert!(matches!(
                error,
                ManualCommandError::Database(ManualDatabaseError::InvalidProject(_))
            ));
        }

        let connection = Connection::open(&database).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM rpg_maker_text_unit", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM rpg_maker_manual_translation",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn clear_locator_rejects_a_readable_id_shared_by_current_and_detached_entries() {
        let current = indexed_entry(ManualTranslationType::Fixed, &["current"]);
        let id = current.id.clone();
        let snapshot = ManualProjectLuaSnapshot {
            current: ManualProjectSnapshot {
                index: ManualTranslationIndex::new(vec![current]).unwrap(),
                placeholders: test_placeholder_validator(),
            },
            detached: vec![ManualDetachedTranslation {
                snapshot: ManualOutdatedTranslation {
                    id: id.clone(),
                    kind: ManualTranslationType::Fixed,
                    source: vec!["outdated".to_owned()],
                    translation: vec!["旧译文".to_owned()],
                },
                locator: ManualTranslationLocator::RpgMaker {
                    owner: "rules".to_owned(),
                    group_location: "detached-location".to_owned(),
                    unit_role: "role".to_owned(),
                },
            }],
        };

        assert_eq!(
            snapshot.clear_locator(&id),
            Err(ManualClearLocatorError::Ambiguous)
        );
    }

    #[test]
    fn manual_output_uses_selected_locale_and_sanitizes_dynamic_values() {
        let localizer = UiLocalizer::new(crate::i18n::UiLocale::English);
        let mut stdout = Vec::new();
        render_manual_command_summary(
            &ManualCommandSummary::Exported {
                entries: 2,
                file: PathBuf::from("C:\\Games\n\u{202e}demo\u{2068}\u{1b}[31m.toml"),
            },
            &localizer,
            &mut stdout,
        )
        .unwrap();
        let stdout = String::from_utf8(stdout).unwrap();
        assert!(stdout.contains("Exported"));
        assert!(stdout.contains("C:\\Games demo[31m.toml"));
        assert!(!stdout.contains('\u{202e}'));
        assert!(!stdout.contains('\u{1b}'));

        let problem = ManualCheckProblem::UnknownId;
        let error = ManualCommandError::InvalidEntries(ManualCheckReport {
            errors: vec![ManualCheckIssue {
                id: "Skills.json:1:name\n\u{202e}\u{1b}[31mforged".to_owned(),
                problem,
            }],
            ..ManualCheckReport::default()
        });
        let mut stderr = Vec::new();
        render_manual_command_error(&error, &localizer, &mut stderr).unwrap();
        let stderr = String::from_utf8(stderr).unwrap();
        assert!(stderr.contains("does not exist"));
        assert!(stderr.contains("Skills.json:1:name [31mforged"));
        assert!(stderr.contains(&localizer.format(UiMessage::DiagnosticErrorHeading)));
        assert!(stderr.contains(&render_state_effect_impact(
            StateEffect::Unchanged,
            &localizer,
        )));
        assert_eq!(stderr.lines().count(), 6);
        assert!(!stderr.contains('\u{202e}'));
        assert!(!stderr.contains('\u{1b}'));
    }

    #[test]
    fn apply_stops_before_callback_when_any_entry_is_invalid() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("manual.toml");
        let index = ManualTranslationIndex::new(vec![indexed_entry(
            ManualTranslationType::Fixed,
            &["source"],
        )])
        .unwrap();
        write_document(
            &path,
            "[[translation]]\nid = \"Skills.json:798:name\"\ntype = \"fixed\"\nsource = [\"changed\"]\ntranslation = [\"译文\"]\n",
        );
        let snapshot = ManualProjectSnapshot {
            index,
            placeholders: test_placeholder_validator(),
        };
        let mut called = false;
        let result = apply_manual_snapshot(
            &path,
            &snapshot,
            &CooperativeCancellation::default(),
            |_| {
                called = true;
                Ok(0)
            },
        );
        assert!(matches!(result, Err(ManualCommandError::InvalidEntries(_))));
        assert!(!called);
    }

    fn test_placeholder_validator() -> ManualPlaceholderValidator {
        let service = GenericPlaceholderService::default();
        let compiled = service.compile(Vec::new()).unwrap();
        ManualPlaceholderValidator::Generic { service, compiled }
    }

    #[test]
    fn rpg_maker_manual_placeholder_check_uses_the_source_binding() {
        let service = Pcre2PlaceholderService::new().expect("内置 Placeholder 应可编译");
        let compiled = service
            .compile_custom(vec![PlaceholderRuleDefinition::new(
                Some(vec!["database_entry".to_owned()]),
                r"(?<=Name: )[A-Za-z0-9-]+",
            )])
            .expect("lookbehind Placeholder 规则应有效");
        let role = TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应有效"));
        let location = RpgMakerLocation::value(
            RpgMakerSource::Data(StandardDataFile::Skills),
            vec![RpgMakerLocationStep::ArrayIndex(798)],
        );
        let profile = RpgMakerBuiltinPlaceholderProfile::for_location(
            RpgMakerEngine::Mz,
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            &location,
            &role,
        );
        let entry = indexed_entry(
            ManualTranslationType::Fixed,
            &["Name: abc-123", "Name: def-456"],
        );
        let validator = ManualPlaceholderValidator::RpgMaker {
            engine: RpgMakerEngine::Mz,
            service,
            compiled,
            profiles: HashMap::from([(entry.id.clone(), profile)]),
        };

        validator
            .validate(
                &entry,
                &["名称：abc-123".to_owned(), "名称：def-456".to_owned()],
            )
            .expect("标签翻译后，源文绑定的凭据仍应通过 Manual check");
        assert!(
            validator
                .validate(
                    &entry,
                    &["名称：def-456".to_owned(), "名称：abc-123".to_owned()],
                )
                .is_err(),
            "Fixed Manual 的 Placeholder 不得跨源文槽位交换"
        );
    }

    #[test]
    fn readable_ids_use_natural_rpg_maker_locations() {
        use crate::rpg_maker::model::ScalarFieldKey;
        use crate::rpg_maker::text::{DataFileName, MapId, StandardDataFile};

        let skill = RpgMakerLocation::value(
            RpgMakerSource::Data(StandardDataFile::Skills),
            vec![RpgMakerLocationStep::ArrayIndex(798)],
        );
        let role = TextUnitRole::Scalar(ScalarFieldKey::new("name").unwrap());
        assert_eq!(
            readable_rpg_maker_id(&skill, TextGroupKind::DatabaseEntry, &role),
            "Skills.json:798:name"
        );

        let map = RpgMakerLocation::value(
            RpgMakerSource::Map(MapId::new(23).unwrap()),
            vec![
                RpgMakerLocationStep::ObjectKey("events".to_owned()),
                RpgMakerLocationStep::ArrayIndex(17),
                RpgMakerLocationStep::ObjectKey("pages".to_owned()),
                RpgMakerLocationStep::ArrayIndex(0),
                RpgMakerLocationStep::ObjectKey("list".to_owned()),
                RpgMakerLocationStep::ArrayIndex(41),
            ],
        );
        assert_eq!(
            readable_rpg_maker_id(
                &map,
                TextGroupKind::EventDialogue,
                &TextUnitRole::DialogueBody
            ),
            "Map023.json:event17:page1:dialogue42"
        );

        let rules = RpgMakerLocation::value(
            RpgMakerSource::DataFile(DataFileName::parse("QuestData.json").unwrap()),
            vec![
                RpgMakerLocationStep::ObjectKey("quests".to_owned()),
                RpgMakerLocationStep::ArrayIndex(3),
                RpgMakerLocationStep::DecodeJsonString,
                RpgMakerLocationStep::ObjectKey("display name".to_owned()),
            ],
        );
        assert_eq!(
            readable_rpg_maker_id(&rules, TextGroupKind::DatabaseEntry, &role),
            "QuestData.json:quests:4[\"display name\"]:name"
        );
    }
}
