//! MV、MZ 与 Generic 共用的 TOML 人工补译契约。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, OpenOptions};
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
    GenericCompiledPlaceholderRules, GenericPlaceholderService,
    validate_translation_placeholders_with_cancellation,
};
use crate::i18n::{UiLocalizer, UiMessage};
use crate::language::{LanguageId, LanguageModule, LanguageModuleCatalog, LanguagePair};
use crate::rpg_maker::RpgMakerEngine;
use crate::rpg_maker::asset::RpgMakerAssetOwner;
use crate::rpg_maker::location_codec::{RpgMakerLocationCodec, RpgMakerProjectionCodec};
use crate::rpg_maker::model::{TextUnitContent, TextUnitRole};
use crate::rpg_maker::semantic_order::RpgMakerSemanticOrderKey;
use crate::rpg_maker::text::{
    RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, TextGroupKind,
};
use crate::rpg_maker::translate::pipeline::{ExpectedLineShape, TranslationUnitIdentity};
use crate::rpg_maker::translate::placeholder::{
    CompiledPlaceholderRules, Pcre2PlaceholderService, PlaceholderRuleDefinition,
};
use crate::rpg_maker::translate::planner::expected_line_shape;
use crate::rpg_maker::translate::semantics::{
    PreparedTranslationStatus, ResolvedTranslationSemantics,
};
use crate::runtime::windows::{
    FileIdentity, PinnedPath, WindowsFsError, pin_directory_without_reparse,
    pin_path_without_reparse,
};
use crate::translation::planning_resource::CompiledTerminology;
use crate::windows_path::{WindowsOrdinalCaseKey, WindowsOrdinalCaseKeyError};

const MANUAL_SQLITE_CANCELLATION_CHECK_OPERATIONS: i32 = 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManualOperation {
    Export,
    Check,
    Apply,
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
    pub(crate) outdated_manual: Option<ManualOutdatedTranslation>,
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
    ExistingBackup {
        path: PathBuf,
    },
    TemporaryCleanup {
        operation: Box<ManualDocumentError>,
        temporary: PathBuf,
        cleanup: io::Error,
    },
    PairedPublicationRollback {
        operation: Box<ManualDocumentError>,
        failures: Vec<ManualPairIoFailure>,
    },
    PairedPublicationFinalization {
        failures: Vec<ManualPairIoFailure>,
    },
}

/// Manual 导出在建立输出目标身份时能够确认的闭集问题。
///
/// 这些事实不能退化成普通写入错误，否则 CLI 会把同一目标、目录或 reparse point
/// 错误地解释为权限问题。
#[derive(Debug)]
pub(crate) enum ManualOutputTargetProblem {
    SameTarget { first: PathBuf, second: PathBuf },
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

#[derive(Debug)]
pub(crate) struct ManualPairIoFailure {
    path: PathBuf,
    source: io::Error,
}

#[cfg(test)]
impl ManualPairIoFailure {
    pub(crate) fn for_test(path: PathBuf, source: io::Error) -> Self {
        Self { path, source }
    }
}

impl ManualOutputTargetProblem {
    fn object(&self) -> String {
        match self {
            Self::SameTarget { first, second } => {
                format!("{}；{}", public_path(first), public_path(second))
            }
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
            Self::SameTarget { .. } => "conflicting_values",
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
            Self::SameTarget { .. }
            | Self::NotRegularFile { .. }
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
            Self::Encode(_) => formatter.write_str("无法生成人工译文 TOML"),
            Self::Write { path, .. } => write!(formatter, "无法写入 {}", public_path(path)),
            Self::OutputTarget { problem } => {
                write!(formatter, "Manual 输出目标无效：{}", problem.object())
            }
            Self::ExistingTemporary { path } => {
                write!(formatter, "固定临时文件已经存在：{}", public_path(path))
            }
            Self::ExistingBackup { path } => {
                write!(formatter, "固定恢复文件已经存在：{}", public_path(path))
            }
            Self::TemporaryCleanup {
                operation,
                temporary,
                cleanup,
            } => write!(
                formatter,
                "{operation}；清理人工译文临时文件 {} 失败：{cleanup}",
                public_path(temporary)
            ),
            Self::PairedPublicationRollback {
                operation,
                failures,
            } => write!(
                formatter,
                "{operation}；成对发布恢复有 {} 项失败",
                failures.len()
            ),
            Self::PairedPublicationFinalization { failures } => {
                write!(
                    formatter,
                    "成对发布已经生效，但有 {} 项收尾失败",
                    failures.len()
                )
            }
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
            Self::PairedPublicationRollback { operation, .. } => Some(operation.as_ref()),
            Self::PairedPublicationFinalization { failures } => failures
                .first()
                .map(|failure| &failure.source as &(dyn Error + 'static)),
            Self::Cancelled
            | Self::InvalidUtf8 { .. }
            | Self::OutputTarget { .. }
            | Self::ExistingTemporary { .. }
            | Self::ExistingBackup { .. } => None,
        }
    }
}

#[cfg(test)]
pub(crate) fn export_manual_document(
    path: &Path,
    index: &ManualTranslationIndex,
) -> Result<usize, ManualDocumentError> {
    export_manual_document_with_cancellation(path, index, &CooperativeCancellation::default())
}

fn export_manual_document_with_cancellation(
    path: &Path,
    index: &ManualTranslationIndex,
    cancellation: &CooperativeCancellation,
) -> Result<usize, ManualDocumentError> {
    ensure_manual_document_running(cancellation)?;
    let translation = index
        .entries()
        .iter()
        .filter(|entry| entry.needs_translation)
        .map(|entry| ManualDocumentEntry {
            id: entry.id.clone(),
            kind: entry.kind,
            source: entry.source.clone(),
            translation: Vec::new(),
        })
        .collect::<Vec<_>>();
    let count = translation.len();
    let encoded = if translation.is_empty() {
        String::new()
    } else {
        toml::to_string_pretty(&ManualDocument { translation })
            .map_err(ManualDocumentError::Encode)?
    };
    ensure_manual_document_running(cancellation)?;
    atomic_replace(path, encoded.as_bytes(), cancellation)?;
    Ok(count)
}

fn export_rpg_maker_manual_documents_with_cancellation(
    path: &Path,
    ownership_path: &Path,
    index: &ManualTranslationIndex,
    cancellation: &CooperativeCancellation,
) -> Result<usize, ManualDocumentError> {
    ensure_manual_document_running(cancellation)?;
    let entries = index
        .entries()
        .iter()
        .filter(|entry| entry.needs_translation)
        .collect::<Vec<_>>();
    let translation = entries
        .iter()
        .map(|entry| ManualDocumentEntry {
            id: entry.id.clone(),
            kind: entry.kind,
            source: entry.source.clone(),
            translation: Vec::new(),
        })
        .collect::<Vec<_>>();
    let encoded = if translation.is_empty() {
        String::new()
    } else {
        toml::to_string_pretty(&ManualDocument { translation })
            .map_err(ManualDocumentError::Encode)?
    };
    let mut ownership = String::new();
    for entry in &entries {
        let (owner, rule_number) = match entry.rpg_maker_owner {
            Some(ManualRpgMakerOwner::Builtin) => ("builtin", None),
            Some(ManualRpgMakerOwner::Rules { rule_number }) => ("rules", Some(rule_number)),
            None => {
                return Err(ManualDocumentError::Write {
                    path: ownership_path.to_path_buf(),
                    source: io::Error::new(
                        io::ErrorKind::InvalidData,
                        "RPG Maker Manual 条目缺少文字所有权",
                    ),
                });
            }
        };
        let record = ManualOwnershipRecord {
            manual_id: &entry.id,
            owner,
            rule_number,
        };
        ownership.push_str(
            &serde_json::to_string(&record)
                .expect("只包含字符串和正整数的 Manual 所有权记录必须可编码"),
        );
        ownership.push('\n');
    }
    ensure_manual_document_running(cancellation)?;
    atomic_replace_pair(
        path,
        encoded.as_bytes(),
        ownership_path,
        ownership.as_bytes(),
        cancellation,
    )?;
    Ok(entries.len())
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
        if let Some(line) = invalid_line(&item.translation) {
            push_issue(
                &mut report,
                id,
                ManualCheckProblem::InvalidTranslationLine { line: line + 1 },
            );
            continue;
        }
        if current.kind == ManualTranslationType::Fixed {
            if item.translation.len() != current.source.len() {
                push_issue(
                    &mut report,
                    id,
                    ManualCheckProblem::FixedLength {
                        expected: current.source.len(),
                        actual: item.translation.len(),
                    },
                );
                continue;
            }
            if let Some(slot) =
                current
                    .source
                    .iter()
                    .zip(&item.translation)
                    .position(|(source, translation)| {
                        source.trim().is_empty() && !translation.is_empty()
                    })
            {
                push_issue(
                    &mut report,
                    id,
                    ManualCheckProblem::FixedBlankSlot { slot: slot + 1 },
                );
                continue;
            }
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
    drop(bind_manual_output_target(path)?);
    let temporary = manual_temporary_path(path);
    ensure_manual_document_running(cancellation)?;
    let mut owns_temporary = false;
    let write = (|| -> Result<(), ManualDocumentError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|source| manual_temporary_open_error(path, &temporary, source))?;
        owns_temporary = true;
        file.write_all(bytes)
            .map_err(|source| ManualDocumentError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        ensure_manual_document_running(cancellation)?;
        file.sync_all()
            .map_err(|source| ManualDocumentError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        drop(file);
        ensure_manual_document_running(cancellation)?;
        replace_file(&temporary, path).map_err(|source| ManualDocumentError::Write {
            path: path.to_path_buf(),
            source,
        })
    })();
    match write {
        Err(operation) if owns_temporary => {
            Err(cleanup_manual_temporary_after_failure(temporary, operation))
        }
        result => result,
    }
}

fn atomic_replace_pair(
    first_path: &Path,
    first_bytes: &[u8],
    second_path: &Path,
    second_bytes: &[u8],
    cancellation: &CooperativeCancellation,
) -> Result<(), ManualDocumentError> {
    ensure_distinct_manual_output_targets(first_path, second_path)?;
    let first_backup = manual_backup_path(first_path);
    let second_backup = manual_backup_path(second_path);
    ensure_target_absent(&first_backup)?;
    ensure_target_absent(&second_backup)?;
    let first_temporary = stage_manual_file(first_path, first_bytes, cancellation)?;
    let second_temporary = match stage_manual_file(second_path, second_bytes, cancellation) {
        Ok(temporary) => temporary,
        Err(operation) => {
            return Err(cleanup_manual_temporary_after_failure(
                first_temporary,
                operation,
            ));
        }
    };
    let result = publish_staged_pair(
        &first_temporary,
        first_path,
        &first_backup,
        &second_temporary,
        second_path,
        &second_backup,
        &SystemManualPairPublisher,
    );
    match result {
        Ok(()) => Ok(()),
        Err(operation) => {
            let operation = if first_temporary.exists() {
                cleanup_manual_temporary_after_failure(first_temporary, operation)
            } else {
                operation
            };
            if second_temporary.exists() {
                Err(cleanup_manual_temporary_after_failure(
                    second_temporary,
                    operation,
                ))
            } else {
                Err(operation)
            }
        }
    }
}

enum ManualOutputTargetIdentity {
    Existing(FileIdentity),
    Planned(WindowsOrdinalCaseKey),
}

struct BoundManualOutputTarget {
    identity: ManualOutputTargetIdentity,
    _pinned: PinnedPath,
}

fn ensure_distinct_manual_output_targets(
    first: &Path,
    second: &Path,
) -> Result<(), ManualDocumentError> {
    let paths = [
        first.to_path_buf(),
        manual_temporary_path(first),
        manual_backup_path(first),
        second.to_path_buf(),
        manual_temporary_path(second),
        manual_backup_path(second),
    ];
    let bound = paths
        .iter()
        .map(|path| bind_manual_output_target(path))
        .collect::<Result<Vec<_>, _>>()?;
    for left in 0..bound.len() {
        for right in left + 1..bound.len() {
            let same = match (&bound[left].identity, &bound[right].identity) {
                (
                    ManualOutputTargetIdentity::Existing(first),
                    ManualOutputTargetIdentity::Existing(second),
                ) => first == second,
                (
                    ManualOutputTargetIdentity::Planned(first),
                    ManualOutputTargetIdentity::Planned(second),
                ) => first == second,
                _ => false,
            };
            if same {
                return Err(ManualDocumentError::OutputTarget {
                    problem: ManualOutputTargetProblem::SameTarget {
                        first: paths[left].clone(),
                        second: paths[right].clone(),
                    },
                });
            }
        }
    }
    Ok(())
}

fn bind_manual_output_target(path: &Path) -> Result<BoundManualOutputTarget, ManualDocumentError> {
    match pin_path_without_reparse(path) {
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
            let identity = FileIdentity::of(pinned.file(), path)
                .map_err(|source| manual_target_error(path, source))?;
            Ok(BoundManualOutputTarget {
                identity: ManualOutputTargetIdentity::Existing(identity),
                _pinned: pinned,
            })
        }
        Err(WindowsFsError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let file_name = path
                .file_name()
                .ok_or_else(|| ManualDocumentError::OutputTarget {
                    problem: ManualOutputTargetProblem::MissingFileName {
                        path: path.to_path_buf(),
                    },
                })?;
            let pinned = pin_directory_without_reparse(parent)
                .map_err(|source| manual_target_error(path, source))?;
            let resolved = pinned.resolved_path().join(file_name);
            let key = WindowsOrdinalCaseKey::from_os_str(resolved.as_os_str())
                .map_err(|source| manual_target_key_error(path, source))?;
            Ok(BoundManualOutputTarget {
                identity: ManualOutputTargetIdentity::Planned(key),
                _pinned: pinned,
            })
        }
        Err(source) => Err(manual_target_error(path, source)),
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
        WindowsFsError::FileIdentityChanged { path } => {
            ManualOutputTargetProblem::IdentityChanged { path }
        }
    };
    ManualDocumentError::OutputTarget { problem }
}

fn manual_target_key_error(path: &Path, source: WindowsOrdinalCaseKeyError) -> ManualDocumentError {
    let problem = match source {
        WindowsOrdinalCaseKeyError::InputTooLarge { .. } => {
            ManualOutputTargetProblem::MissingFileName {
                path: path.to_path_buf(),
            }
        }
        WindowsOrdinalCaseKeyError::WindowsApi { source, .. } => {
            ManualOutputTargetProblem::BindingIo {
                path: path.to_path_buf(),
                failure: IoFailure::from_error(&source),
            }
        }
    };
    ManualDocumentError::OutputTarget { problem }
}

fn manual_backup_path(path: &Path) -> PathBuf {
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{file_name}.backup"))
}

fn manual_temporary_path(path: &Path) -> PathBuf {
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(".{file_name}.tmp"))
}

fn ensure_target_absent(path: &Path) -> Result<(), ManualDocumentError> {
    match fs::metadata(path) {
        Ok(_) => Err(ManualDocumentError::ExistingBackup {
            path: path.to_path_buf(),
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(ManualDocumentError::Write {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn stage_manual_file(
    path: &Path,
    bytes: &[u8],
    cancellation: &CooperativeCancellation,
) -> Result<PathBuf, ManualDocumentError> {
    let temporary = manual_temporary_path(path);
    ensure_manual_document_running(cancellation)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|source| manual_temporary_open_error(path, &temporary, source))?;
    let result = (|| -> Result<(), ManualDocumentError> {
        file.write_all(bytes)
            .map_err(|source| ManualDocumentError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        ensure_manual_document_running(cancellation)?;
        file.sync_all()
            .map_err(|source| ManualDocumentError::Write {
                path: path.to_path_buf(),
                source,
            })?;
        ensure_manual_document_running(cancellation)
    })();
    drop(file);
    match result {
        Ok(()) => Ok(temporary),
        Err(operation) => Err(cleanup_manual_temporary_after_failure(temporary, operation)),
    }
}

fn manual_temporary_open_error(
    target: &Path,
    temporary: &Path,
    source: io::Error,
) -> ManualDocumentError {
    if source.kind() == io::ErrorKind::AlreadyExists {
        ManualDocumentError::ExistingTemporary {
            path: temporary.to_path_buf(),
        }
    } else {
        ManualDocumentError::Write {
            path: target.to_path_buf(),
            source,
        }
    }
}

trait ManualPairPublisher {
    fn exists(&self, path: &Path) -> io::Result<bool>;
    fn backup_existing(&self, source: &Path, backup: &Path) -> io::Result<()>;
    fn commit_new(&self, source: &Path, target: &Path) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn restore_backup(&self, backup: &Path, target: &Path) -> io::Result<()>;
}

struct SystemManualPairPublisher;

impl ManualPairPublisher for SystemManualPairPublisher {
    fn exists(&self, path: &Path) -> io::Result<bool> {
        match fs::metadata(path) {
            Ok(_) => Ok(true),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(source),
        }
    }

    fn backup_existing(&self, source: &Path, backup: &Path) -> io::Result<()> {
        rename_new_file(source, backup)
    }

    fn commit_new(&self, source: &Path, target: &Path) -> io::Result<()> {
        rename_new_file(source, target)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }

    fn restore_backup(&self, backup: &Path, target: &Path) -> io::Result<()> {
        rename_new_file(backup, target)
    }
}

struct ManualPairTarget<'a> {
    target: &'a Path,
    backup: &'a Path,
    had_original: bool,
    published: bool,
}

impl<'a> ManualPairTarget<'a> {
    const fn new(target: &'a Path, backup: &'a Path) -> Self {
        Self {
            target,
            backup,
            had_original: false,
            published: false,
        }
    }
}

fn publish_staged_pair(
    first_temporary: &Path,
    first_target: &Path,
    first_backup: &Path,
    second_temporary: &Path,
    second_target: &Path,
    second_backup: &Path,
    publisher: &impl ManualPairPublisher,
) -> Result<(), ManualDocumentError> {
    let mut first = ManualPairTarget::new(first_target, first_backup);
    let mut second = ManualPairTarget::new(second_target, second_backup);
    let operation = (|| -> Result<(), ManualDocumentError> {
        backup_pair_target(&mut first, publisher)?;
        backup_pair_target(&mut second, publisher)?;
        publisher
            .commit_new(first_temporary, first_target)
            .map_err(|source| ManualDocumentError::Write {
                path: first_target.to_path_buf(),
                source,
            })?;
        first.published = true;
        publisher
            .commit_new(second_temporary, second_target)
            .map_err(|source| ManualDocumentError::Write {
                path: second_target.to_path_buf(),
                source,
            })?;
        second.published = true;
        Ok(())
    })();
    if let Err(operation) = operation {
        return Err(restore_pair_after_failure(
            operation,
            &mut first,
            &mut second,
            publisher,
        ));
    }
    let mut finalization_failures = Vec::new();
    for target in [&first, &second] {
        if target.had_original
            && let Err(source) = publisher.remove_file(target.backup)
        {
            finalization_failures.push(ManualPairIoFailure {
                path: target.backup.to_path_buf(),
                source,
            });
        }
    }
    if !finalization_failures.is_empty() {
        return Err(ManualDocumentError::PairedPublicationFinalization {
            failures: finalization_failures,
        });
    }
    Ok(())
}

fn backup_pair_target(
    target: &mut ManualPairTarget<'_>,
    publisher: &impl ManualPairPublisher,
) -> Result<(), ManualDocumentError> {
    let had_original =
        publisher
            .exists(target.target)
            .map_err(|source| ManualDocumentError::Write {
                path: target.target.to_path_buf(),
                source,
            })?;
    if had_original {
        publisher
            .backup_existing(target.target, target.backup)
            .map_err(|source| ManualDocumentError::Write {
                path: target.target.to_path_buf(),
                source,
            })?;
        target.had_original = true;
    }
    Ok(())
}

fn restore_pair_after_failure(
    operation: ManualDocumentError,
    first: &mut ManualPairTarget<'_>,
    second: &mut ManualPairTarget<'_>,
    publisher: &impl ManualPairPublisher,
) -> ManualDocumentError {
    let mut failures = Vec::new();
    restore_pair_target(second, publisher, &mut failures);
    restore_pair_target(first, publisher, &mut failures);
    if failures.is_empty() {
        operation
    } else {
        ManualDocumentError::PairedPublicationRollback {
            operation: Box::new(operation),
            failures,
        }
    }
}

fn restore_pair_target(
    target: &mut ManualPairTarget<'_>,
    publisher: &impl ManualPairPublisher,
    failures: &mut Vec<ManualPairIoFailure>,
) {
    if target.published {
        match publisher.remove_file(target.target) {
            Ok(()) => target.published = false,
            Err(source) => {
                failures.push(ManualPairIoFailure {
                    path: target.target.to_path_buf(),
                    source,
                });
                return;
            }
        }
    }
    if target.had_original {
        match publisher.restore_backup(target.backup, target.target) {
            Ok(()) => target.had_original = false,
            Err(source) => failures.push(ManualPairIoFailure {
                path: target.target.to_path_buf(),
                source,
            }),
        }
    }
}

#[cfg(windows)]
fn rename_new_file(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};
    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let moved = unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), MOVEFILE_WRITE_THROUGH) };
    if moved == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn rename_new_file(source: &Path, target: &Path) -> io::Result<()> {
    fs::rename(source, target)
}

fn cleanup_manual_temporary_after_failure(
    temporary: PathBuf,
    operation: ManualDocumentError,
) -> ManualDocumentError {
    match fs::remove_file(&temporary) {
        Ok(()) => operation,
        Err(cleanup) if cleanup.kind() == io::ErrorKind::NotFound => operation,
        Err(cleanup) => ManualDocumentError::TemporaryCleanup {
            operation: Box::new(operation),
            temporary,
            cleanup,
        },
    }
}

#[cfg(windows)]
fn replace_file(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(source: &Path, target: &Path) -> io::Result<()> {
    fs::rename(source, target)
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
    },
}

impl ManualPlaceholderValidator {
    pub(crate) fn validate(
        &self,
        entry: &ManualTranslationEntry,
        translation: &[String],
    ) -> Result<(), String> {
        let source = entry.source.join("\n");
        let translation = translation.join("\n");
        match self {
            Self::Generic { service, compiled } => {
                match validate_translation_placeholders_with_cancellation(
                    service,
                    compiled,
                    &entry.placeholder_scope,
                    &source,
                    &translation,
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
            } => {
                let Some(kind) = TextGroupKind::from_storage_name(&entry.placeholder_scope) else {
                    return Err("当前位置的 Placeholder 范围无效".to_owned());
                };
                let source_offsets = line_separator_offsets(&entry.source);
                let translation_lines = translation
                    .split('\n')
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                let translation_offsets = line_separator_offsets(&translation_lines);
                let source = service
                    .protect_with_line_boundaries_with_cancellation(
                        *engine,
                        kind,
                        &source,
                        &source_offsets,
                        compiled,
                        || Ok::<_, std::convert::Infallible>(()),
                    )
                    .map_err(|unreachable| match unreachable {})
                    .and_then(|result| result.map_err(|_| "无法读取原文 Placeholder".to_owned()))?;
                let candidate = service
                    .protect_with_line_boundaries_with_cancellation(
                        *engine,
                        kind,
                        &translation,
                        &translation_offsets,
                        compiled,
                        || Ok::<_, std::convert::Infallible>(()),
                    )
                    .map_err(|unreachable| match unreachable {})
                    .and_then(|result| result.map_err(|_| "无法读取译文 Placeholder".to_owned()))?;
                if source.placeholders() == candidate.placeholders() {
                    Ok(())
                } else {
                    Err("译文没有保留原文中的控制码或 Placeholder".to_owned())
                }
            }
        }
    }
}

fn line_separator_offsets(lines: &[String]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(lines.len().saturating_sub(1));
    let mut offset = 0;
    for line in lines.iter().take(lines.len().saturating_sub(1)) {
        offset += line.len();
        offsets.push(offset);
        offset += 1;
    }
    offsets
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
        ownership_file: Option<PathBuf>,
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
    language_modules: Option<&LanguageModuleCatalog>,
    cancellation: &CooperativeCancellation,
) -> Result<ManualCommandSummary, ManualCommandError> {
    assert_eq!(
        matches!(operation, ManualOperation::Export),
        language_modules.is_some(),
        "只有 Manual export 应获得语言模块"
    );
    execute_manual_database_command(
        database_path,
        operation,
        file,
        None,
        cancellation,
        |connection| load_generic_manual_command_snapshot(connection, language_modules),
        |connection, writes| {
            apply_generic_manual_translations_with_cancellation(connection, writes, cancellation)
        },
    )
}

pub(crate) fn execute_rpg_maker_manual_command(
    database_path: &Path,
    engine: RpgMakerEngine,
    operation: ManualOperation,
    file: &Path,
    ownership_file: Option<&Path>,
    language_modules: Option<&LanguageModuleCatalog>,
    cancellation: &CooperativeCancellation,
) -> Result<ManualCommandSummary, ManualCommandError> {
    assert_eq!(
        matches!(operation, ManualOperation::Export),
        language_modules.is_some(),
        "只有 Manual export 应获得语言模块"
    );
    execute_manual_database_command(
        database_path,
        operation,
        file,
        ownership_file,
        cancellation,
        |connection| load_rpg_maker_manual_command_snapshot(connection, engine, language_modules),
        |connection, writes| {
            apply_rpg_maker_manual_translations_with_cancellation(connection, writes, cancellation)
        },
    )
}

fn execute_manual_database_command(
    database_path: &Path,
    operation: ManualOperation,
    file: &Path,
    ownership_file: Option<&Path>,
    cancellation: &CooperativeCancellation,
    mut load_snapshot: impl FnMut(&Connection) -> Result<ManualProjectSnapshot, ManualDatabaseError>,
    mut apply: impl FnMut(
        &Connection,
        &[ValidatedManualTranslation],
    ) -> Result<usize, ManualDatabaseError>,
) -> Result<ManualCommandSummary, ManualCommandError> {
    ensure_manual_command_running(cancellation)?;
    match operation {
        ManualOperation::Export | ManualOperation::Check => {
            let mut connection = open_read_only(database_path, cancellation)
                .map_err(|source| manual_command_database_error(source, cancellation))?;
            ensure_manual_command_running(cancellation)?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Deferred)
                .map_err(ManualDatabaseError::from)
                .map_err(|source| manual_command_database_error(source, cancellation))?;
            let snapshot = load_snapshot(&transaction)
                .map_err(|source| manual_command_database_error(source, cancellation))?;
            ensure_manual_command_running(cancellation)?;
            transaction
                .commit()
                .map_err(ManualDatabaseError::from)
                .map_err(|source| manual_command_database_error(source, cancellation))?;
            ensure_manual_command_running(cancellation)?;
            execute_manual_read_operation(operation, file, ownership_file, &snapshot, cancellation)
        }
        ManualOperation::Apply => {
            let mut connection = open_read_write(database_path, cancellation)
                .map_err(|source| manual_command_database_error(source, cancellation))?;
            ensure_manual_command_running(cancellation)?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(ManualDatabaseError::from)
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
    ownership_file: Option<&Path>,
    snapshot: &ManualProjectSnapshot,
    cancellation: &CooperativeCancellation,
) -> Result<ManualCommandSummary, ManualCommandError> {
    match operation {
        ManualOperation::Export => {
            let exported = match ownership_file {
                Some(ownership_file) => export_rpg_maker_manual_documents_with_cancellation(
                    file,
                    ownership_file,
                    &snapshot.index,
                    cancellation,
                ),
                None => {
                    export_manual_document_with_cancellation(file, &snapshot.index, cancellation)
                }
            };
            exported
                .map(|entries| ManualCommandSummary::Exported {
                    entries,
                    file: file.to_path_buf(),
                    ownership_file: ownership_file.map(Path::to_path_buf),
                })
                .map_err(ManualCommandError::from_document)
        }
        ManualOperation::Check => {
            assert!(
                ownership_file.is_none(),
                "Manual check 不得获得所有权输出路径"
            );
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
        ManualOutputTargetProblem::SameTarget { first, second } => {
            FileSystemProblem::ConflictingOutputPaths {
                first_path: SafePath::new(first),
                second_path: SafePath::new(second),
            }
        }
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
        ManualDocumentError::ExistingTemporary { path }
        | ManualDocumentError::ExistingBackup { path } => DiagnosticReport::new(
            StateEffect::RecoveryRequired,
            manual_recovery_artifact_diagnostic(path),
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
        ManualDocumentError::PairedPublicationRollback {
            operation,
            failures,
        } => failures.iter().fold(
            manual_document_diagnostic_report(operation).with_effect(StateEffect::RecoveryRequired),
            |report, failure| {
                report.with_related(
                    RelatedFailureRelation::Rollback,
                    DiagnosticReport::new(
                        StateEffect::RecoveryRequired,
                        manual_recovery_io_diagnostic(
                            &failure.path,
                            FileSystemOperation::RecoverTarget,
                            &failure.source,
                        ),
                    ),
                )
            },
        ),
        ManualDocumentError::PairedPublicationFinalization { failures } => {
            let mut failures = failures.iter();
            let Some(first) = failures.next() else {
                return manual_fallback_diagnostic_report(StateEffect::AppliedFinalizationFailed);
            };
            std::iter::once(first).chain(failures).fold(
                DiagnosticReport::new(
                    StateEffect::AppliedFinalizationFailed,
                    manual_file_system_diagnostic(
                        FileSystemOperation::Remove,
                        FileSystemProblem::CleanupFailed {
                            path: SafePath::new(&first.path),
                        },
                    ),
                ),
                |report, failure| {
                    report.with_related(
                        RelatedFailureRelation::Cleanup,
                        DiagnosticReport::new(
                            StateEffect::AppliedFinalizationFailed,
                            manual_recovery_io_diagnostic(
                                &failure.path,
                                FileSystemOperation::Remove,
                                &failure.source,
                            ),
                        ),
                    )
                },
            )
        }
        ManualDocumentError::InvalidUtf8 { .. }
        | ManualDocumentError::InvalidToml { .. }
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
        ManualCommandSummary::Exported {
            entries,
            file,
            ownership_file,
        } => {
            let path = public_path(file);
            let mut messages = vec![localizer.format(UiMessage::ManualExported {
                entries: manual_count(*entries),
                path: &path,
            })];
            if let Some(ownership_file) = ownership_file {
                let path = public_path(ownership_file);
                messages.push(localizer.format(UiMessage::ManualOwnershipExported { path: &path }));
            }
            messages
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
                    | ManualDocumentError::ExistingBackup { .. }
                    | ManualDocumentError::TemporaryCleanup { .. }
                    | ManualDocumentError::PairedPublicationRollback { .. }
                    | ManualDocumentError::PairedPublicationFinalization { .. }
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
        ManualDocumentError::PairedPublicationRollback {
            operation,
            failures,
        } => manual_document_issue_count(operation).saturating_add(failures.len()),
        ManualDocumentError::PairedPublicationFinalization { failures } => failures.len().max(1),
        ManualDocumentError::Cancelled
        | ManualDocumentError::Read { .. }
        | ManualDocumentError::InvalidUtf8 { .. }
        | ManualDocumentError::InvalidToml { .. }
        | ManualDocumentError::Encode(_)
        | ManualDocumentError::Write { .. }
        | ManualDocumentError::OutputTarget { .. }
        | ManualDocumentError::ExistingTemporary { .. }
        | ManualDocumentError::ExistingBackup { .. } => 1,
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
    if let ManualDocumentError::PairedPublicationRollback {
        operation,
        failures,
    } = source
    {
        render_manual_document_issues_with_effect(
            operation,
            StateEffect::RecoveryRequired,
            localizer,
            stderr,
        )?;
        let help = render_manual_value(localizer, "resolve_temporary_then_rerun_export", 0, 0, 0);
        for failure in failures {
            let reason = manual_temporary_cleanup_reason(&failure.source, localizer);
            writeln!(stderr)?;
            writeln!(
                stderr,
                "{}",
                localizer.format(UiMessage::DiagnosticRelated {
                    relation: RelatedFailureRelation::Rollback.as_str(),
                })
            )?;
            render_manual_issue_body(
                &public_path(&failure.path),
                &reason,
                &help,
                StateEffect::RecoveryRequired,
                localizer,
                stderr,
            )?;
        }
        return Ok(());
    }
    if let ManualDocumentError::PairedPublicationFinalization { failures } = source {
        writeln!(
            stderr,
            "{}",
            localizer.format(UiMessage::DiagnosticErrorHeading)
        )?;
        let help = render_manual_value(localizer, "resolve_published_backup_cleanup", 0, 0, 0);
        for (index, failure) in failures.iter().enumerate() {
            if index != 0 {
                writeln!(stderr)?;
                writeln!(
                    stderr,
                    "{}",
                    localizer.format(UiMessage::DiagnosticRelated {
                        relation: RelatedFailureRelation::Cleanup.as_str(),
                    })
                )?;
            }
            let reason = manual_temporary_cleanup_reason(&failure.source, localizer);
            render_manual_issue_body(
                &public_path(&failure.path),
                &reason,
                &help,
                StateEffect::AppliedFinalizationFailed,
                localizer,
                stderr,
            )?;
        }
        return Ok(());
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
            ManualDocumentError::ExistingTemporary { .. }
            | ManualDocumentError::ExistingBackup { .. } => StateEffect::RecoveryRequired,
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
        "document_invalid_utf8" => failure("invalid_encoding"),
        "document_invalid_toml" => failure("invalid_syntax"),
        "document_encode" => failure("internal_invariant"),
        "keep_placeholders" => resolution("fix_placeholder_rules"),
        "check_read_access" | "check_write_access" | "check_database_access" => {
            resolution("check_path_and_permissions")
        }
        "fix_project_then_export" => resolution("check_project_state"),
        "retry_if_needed" | "retry_or_report" => resolution("retry"),
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
        ManualDocumentError::ExistingBackup { path } => (
            public_path(path),
            "publication_artifact_exists",
            "resolve_published_backup_cleanup",
        ),
        ManualDocumentError::TemporaryCleanup { operation, .. } => manual_document_issue(operation),
        ManualDocumentError::PairedPublicationRollback { operation, .. } => {
            manual_document_issue(operation)
        }
        ManualDocumentError::PairedPublicationFinalization { .. } => (
            "Manual export".to_owned(),
            "document_write",
            "resolve_published_backup_cleanup",
        ),
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
    let compiled = service
        .compile_with_cancellation(definitions, || Ok::<_, std::convert::Infallible>(()))
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
        "SELECT f.relative_path, g.group_id, g.ordinal, g.kind,
                u.unit_id, u.ordinal, u.source_text, u.translation,
                manual.readable_id, manual.source_json, manual.translation_json,
                manual.applicability_fingerprint
         FROM generic_file AS f
         JOIN generic_group AS g ON g.relative_path = f.relative_path
         JOIN generic_unit AS u ON u.group_id = g.group_id
         LEFT JOIN generic_manual_translation AS manual
           ON manual.group_id = u.group_id AND manual.unit_id = u.unit_id
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
        let unit_id: String = row.get(4)?;
        let unit: i64 = row.get(5)?;
        let source_text: String = row.get(6)?;
        let automatic: Option<String> = row.get(7)?;
        let stored_manual = parse_stored_generic_manual_translation(
            row.get(8)?,
            row.get(9)?,
            row.get(10)?,
            row.get(11)?,
        )?;
        let source = source_text
            .split('\n')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let readable_path = relative_path.to_string_lossy().replace('\\', "/");
        let id = format!("{readable_path}:line{}:unit{}:text", line + 1, unit + 1);
        let applicability =
            generic_manual_applicability(&group_id, &unit_id, &readable_path, &kind, &source);
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
            needs_translation: current_translation.is_none() && active,
            placeholder_scope: kind,
            current_translation,
            origin,
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
        .protect_with_cancellation(kind, source, placeholder_rules, || {
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
    let compiled = service
        .compile_custom_with_cancellation(definitions, || Ok::<_, std::convert::Infallible>(()))
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
                Sha256Fingerprint::from_bytes([0; 32]),
            )
        });
    let entries = load_rpg_maker_entries(connection, semantics.as_ref())?;
    Ok(ManualProjectSnapshot {
        index: ManualTranslationIndex::new(entries)?,
        placeholders: ManualPlaceholderValidator::RpgMaker {
            engine,
            service,
            compiled,
        },
    })
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

fn load_rpg_maker_entries(
    connection: &Connection,
    semantics: Option<&ResolvedTranslationSemantics>,
) -> Result<Vec<ManualTranslationEntry>, ManualDatabaseError> {
    let mut statement = connection.prepare(
        "SELECT g.owner, g.group_location, g.group_kind, g.projection_recipe_json,
                g.semantic_order_key, u.unit_role, u.source_content_json,
                u.source_context_json, u.translation_content_json,
                u.semantic_order_key, manual.readable_id,
                manual.translation_type, manual.source_json,
                manual.translation_json, manual.applicability_fingerprint,
                u.rule_number
         FROM rpg_maker_text_group AS g
         JOIN rpg_maker_text_unit AS u
           ON u.owner = g.owner AND u.group_id = g.group_id
         LEFT JOIN rpg_maker_manual_translation AS manual
           ON manual.owner = g.owner
          AND manual.group_location = g.group_location
          AND manual.unit_role = u.unit_role",
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
        let unit_order_raw: Vec<u8> = row.get(9)?;
        let stored_manual = parse_stored_manual_translation(
            row.get(10)?,
            row.get(11)?,
            row.get(12)?,
            row.get(13)?,
            row.get(14)?,
        )?;
        let rule_number: Option<i64> = row.get(15)?;
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
        let recipe_shape =
            RpgMakerProjectionCodec::encode_role_recipe_shape(&recipe_json, &role)
                .map_err(|_| ManualDatabaseError::InvalidProject(format!("{id} 的写回结构无效")))?;
        let applicability = rpg_maker_manual_applicability(
            &owner_raw,
            &group_location_raw,
            &kind_raw,
            &role_raw,
            &recipe_shape,
            manual_type,
            &source,
        );
        let current_manual = stored_manual
            .as_ref()
            .filter(|manual| manual.applicability == applicability);
        let outdated_manual = stored_manual
            .as_ref()
            .filter(|manual| manual.applicability != applicability)
            .map(manual_outdated_snapshot);
        let automatic = automatic
            .as_deref()
            .map(|value| {
                serde_json::from_str::<TextUnitContent>(value).map_err(|_| {
                    ManualDatabaseError::InvalidProject(format!("{id} 的自动译文无法读取"))
                })
            })
            .transpose()?;
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
                    .prepare_content_with_cancellation(kind, &content, || {
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
                needs_translation: current_translation.is_none() && active,
                placeholder_scope: kind_raw,
                current_translation,
                origin,
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
    Ok((manual as u64).saturating_add(automatic as u64))
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
    Ok((manual as u64).saturating_add(automatic as u64))
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
    source: &[String],
) -> Sha256Fingerprint {
    let mut hasher = Sha256FramedHasher::new(b"att.generic.manual-translation");
    hasher
        .frame(1, group_id.as_bytes())
        .frame(2, unit_id.as_bytes())
        .frame(3, relative_path.as_bytes())
        .frame(4, kind.as_bytes());
    for line in source {
        hasher.frame(5, line.as_bytes());
    }
    hasher.finish()
}

pub(crate) fn rpg_maker_manual_applicability(
    owner: &str,
    group_location: &str,
    kind: &str,
    role: &str,
    recipe_shape: &str,
    translation_type: ManualTranslationType,
    source: &[String],
) -> Sha256Fingerprint {
    let mut hasher = Sha256FramedHasher::new(b"att.rpg-maker.manual-translation");
    hasher
        .frame(1, owner.as_bytes())
        .frame(2, group_location.as_bytes())
        .frame(3, kind.as_bytes())
        .frame(4, role.as_bytes())
        .frame(5, recipe_shape.as_bytes())
        .frame(6, manual_type_name(translation_type).as_bytes());
    for line in source {
        hasher.frame(7, line.as_bytes());
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

fn readable_rpg_maker_id(
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
    use std::cell::Cell;

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
            outdated_manual: None,
        }
    }

    fn write_document(path: &Path, source: &str) {
        fs::write(path, source).expect("应写入 Manual TOML");
    }

    #[derive(Default)]
    struct InjectedPairPublisher {
        backups: Cell<usize>,
        commits: Cell<usize>,
        fail_second_backup: bool,
        fail_second_commit: bool,
        fail_published_removal: bool,
        fail_backup_restore: bool,
        fail_backup_cleanup: bool,
    }

    impl ManualPairPublisher for InjectedPairPublisher {
        fn exists(&self, path: &Path) -> io::Result<bool> {
            Ok(path.exists())
        }

        fn backup_existing(&self, source: &Path, backup: &Path) -> io::Result<()> {
            let backup_index = self.backups.get() + 1;
            self.backups.set(backup_index);
            if self.fail_second_backup && backup_index == 2 {
                Err(io::Error::other("injected second backup failure"))
            } else {
                fs::rename(source, backup)
            }
        }

        fn commit_new(&self, source: &Path, target: &Path) -> io::Result<()> {
            let commit = self.commits.get() + 1;
            self.commits.set(commit);
            if self.fail_second_commit && commit == 2 {
                Err(io::Error::other("injected second commit failure"))
            } else {
                fs::rename(source, target)
            }
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
            let is_backup = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .starts_with('.');
            if (self.fail_published_removal && !is_backup)
                || (self.fail_backup_cleanup && is_backup)
            {
                Err(io::Error::other("injected rollback failure"))
            } else {
                fs::remove_file(path)
            }
        }

        fn restore_backup(&self, backup: &Path, target: &Path) -> io::Result<()> {
            if self.fail_backup_restore {
                Err(io::Error::other("injected restore failure"))
            } else {
                fs::rename(backup, target)
            }
        }
    }

    #[test]
    fn paired_publication_replaces_both_existing_files() {
        let directory = tempfile::tempdir().expect("应建立成对替换测试目录");
        let first_target = directory.path().join("manual.toml");
        let second_target = directory.path().join("ownership.jsonl");
        fs::write(&first_target, "old manual").expect("应建立旧 Manual");
        fs::write(&second_target, "old ownership").expect("应建立旧所有权");

        atomic_replace_pair(
            &first_target,
            b"new manual",
            &second_target,
            b"new ownership",
            &CooperativeCancellation::default(),
        )
        .expect("已有两份输出应成对替换");

        assert_eq!(fs::read_to_string(&first_target).unwrap(), "new manual");
        assert_eq!(fs::read_to_string(&second_target).unwrap(), "new ownership");
        assert!(!manual_backup_path(&first_target).exists());
        assert!(!manual_backup_path(&second_target).exists());
    }

    #[test]
    fn paired_publication_rejects_normalized_case_alias_before_staging() {
        let directory = tempfile::tempdir().expect("应建立目标别名测试目录");
        let nested = directory.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let first = directory.path().join("manual.toml");
        let second = nested.join("..").join("MANUAL.TOML");

        let error = atomic_replace_pair(
            &first,
            b"manual",
            &second,
            b"ownership",
            &CooperativeCancellation::default(),
        )
        .expect_err("规范化后相同的大小写别名必须在 staging 前拒绝");

        assert!(matches!(
            error,
            ManualDocumentError::OutputTarget {
                problem: ManualOutputTargetProblem::SameTarget { .. },
            }
        ));
        assert!(!first.exists());
        assert!(!directory.path().join(".manual.toml.tmp").exists());
        assert!(!directory.path().join(".MANUAL.TOML.tmp").exists());
    }

    #[test]
    fn paired_publication_rejects_two_existing_names_for_the_same_file_identity() {
        let directory = tempfile::tempdir().expect("应建立物理目标别名测试目录");
        let first = directory.path().join("manual.toml");
        let second = directory.path().join("ownership.jsonl");
        fs::write(&first, "old").unwrap();
        fs::hard_link(&first, &second).expect("NTFS 测试目录应支持硬链接身份别名");

        let error = atomic_replace_pair(
            &first,
            b"manual",
            &second,
            b"ownership",
            &CooperativeCancellation::default(),
        )
        .expect_err("同一文件身份的两个名称必须在 staging 前拒绝");

        assert!(matches!(
            error,
            ManualDocumentError::OutputTarget {
                problem: ManualOutputTargetProblem::SameTarget { .. },
            }
        ));
        assert_eq!(fs::read_to_string(&first).unwrap(), "old");
        assert_eq!(fs::read_to_string(&second).unwrap(), "old");
        assert!(!directory.path().join(".manual.toml.tmp").exists());
        assert!(!directory.path().join(".ownership.jsonl.tmp").exists());
    }

    #[test]
    fn paired_publication_rejects_targets_that_collide_with_fixed_artifacts() {
        for suffix in ["backup", "tmp"] {
            let directory = tempfile::tempdir().expect("应建立固定产物冲突测试目录");
            let manual = directory.path().join("manual.toml");
            fs::write(&manual, "old manual").unwrap();
            let ownership = directory.path().join(format!(".manual.toml.{suffix}"));

            let error = atomic_replace_pair(
                &manual,
                b"new manual",
                &ownership,
                b"new ownership",
                &CooperativeCancellation::default(),
            )
            .expect_err("最终目标不得与另一输出的固定发布产物冲突");

            assert!(matches!(
                error,
                ManualDocumentError::OutputTarget {
                    problem: ManualOutputTargetProblem::SameTarget { .. },
                }
            ));
            assert_eq!(fs::read_to_string(&manual).unwrap(), "old manual");
            assert!(!ownership.exists());
            assert!(!manual_temporary_path(&manual).exists());
            assert!(!manual_backup_path(&manual).exists());
            assert!(!manual_temporary_path(&ownership).exists());
            assert!(!manual_backup_path(&ownership).exists());
        }
    }

    #[test]
    fn manual_exports_never_move_or_replace_an_existing_directory_target() {
        let directory = tempfile::tempdir().expect("应建立目录目标测试根");
        let directory_target = directory.path().join("manual.toml");
        fs::create_dir(&directory_target).unwrap();
        fs::write(directory_target.join("keep.txt"), "keep").unwrap();
        let ownership = directory.path().join("ownership.jsonl");

        let pair_error = atomic_replace_pair(
            &directory_target,
            b"manual",
            &ownership,
            b"ownership",
            &CooperativeCancellation::default(),
        )
        .expect_err("目录目标必须在成对 staging 前拒绝");
        assert!(matches!(
            pair_error,
            ManualDocumentError::OutputTarget {
                problem: ManualOutputTargetProblem::NotRegularFile { .. },
            }
        ));
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
        assert!(!ownership.exists());
        assert!(!directory.path().join(".manual.toml.tmp").exists());
        assert!(!directory.path().join(".manual.toml.backup").exists());
        assert!(!directory.path().join(".ownership.jsonl.tmp").exists());
        assert!(!directory.path().join(".ownership.jsonl.backup").exists());
    }

    #[cfg(windows)]
    #[test]
    fn manual_exports_reject_reparse_target_before_creating_publication_artifacts() {
        let directory = tempfile::tempdir().expect("应建立 reparse 目标测试根");
        let real = directory.path().join("real-manual.toml");
        let link = directory.path().join("manual.toml");
        let ownership = directory.path().join("ownership.jsonl");
        fs::write(&real, "original").unwrap();
        if let Err(source) = std::os::windows::fs::symlink_file(&real, &link) {
            if source.kind() == io::ErrorKind::PermissionDenied
                || source.raw_os_error() == Some(1314)
            {
                return;
            }
            panic!("应建立测试文件符号链接：{source}");
        }

        for error in [
            atomic_replace_pair(
                &link,
                b"new manual",
                &ownership,
                b"ownership",
                &CooperativeCancellation::default(),
            )
            .expect_err("成对导出必须拒绝 reparse 目标"),
            atomic_replace(&link, b"new manual", &CooperativeCancellation::default())
                .expect_err("单文件导出必须拒绝 reparse 目标"),
        ] {
            assert!(matches!(
                error,
                ManualDocumentError::OutputTarget {
                    problem: ManualOutputTargetProblem::ReparsePoint { .. },
                }
            ));
        }
        assert_eq!(fs::read_to_string(&real).unwrap(), "original");
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!ownership.exists());
        assert!(!manual_temporary_path(&link).exists());
        assert!(!manual_backup_path(&link).exists());
        assert!(!manual_temporary_path(&ownership).exists());
        assert!(!manual_backup_path(&ownership).exists());
    }

    #[test]
    fn output_target_diagnostics_keep_same_target_directory_and_reparse_facts() {
        let localizer = UiLocalizer::new(crate::i18n::UiLocale::SimplifiedChinese);
        let target = PathBuf::from(r"C:\game\manual.toml");
        let cases = [
            (
                ManualOutputTargetProblem::SameTarget {
                    first: target.clone(),
                    second: target.clone(),
                },
                "提供的值互相冲突",
            ),
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
    fn paired_publication_restores_old_files_when_second_commit_fails() {
        let directory = tempfile::tempdir().expect("应建立成对发布测试目录");
        let first_temporary = directory.path().join(".manual.toml.tmp");
        let second_temporary = directory.path().join(".ownership.jsonl.tmp");
        let first_target = directory.path().join("manual.toml");
        let second_target = directory.path().join("ownership.jsonl");
        let first_backup = manual_backup_path(&first_target);
        let second_backup = manual_backup_path(&second_target);
        fs::write(&first_target, "old manual").expect("应建立旧 Manual");
        fs::write(&second_target, "old ownership").expect("应建立旧所有权");
        fs::write(&first_temporary, "new manual").expect("应建立第一份临时文件");
        fs::write(&second_temporary, "new ownership").expect("应建立第二份临时文件");

        let error = publish_staged_pair(
            &first_temporary,
            &first_target,
            &first_backup,
            &second_temporary,
            &second_target,
            &second_backup,
            &InjectedPairPublisher {
                fail_second_commit: true,
                ..InjectedPairPublisher::default()
            },
        )
        .expect_err("第二份提交失败必须返回主错误");

        assert!(matches!(error, ManualDocumentError::Write { path, .. } if path == second_target));
        assert_eq!(fs::read_to_string(&first_target).unwrap(), "old manual");
        assert_eq!(fs::read_to_string(&second_target).unwrap(), "old ownership");
        assert!(!first_backup.exists(), "恢复后不得留下第一份 backup");
        assert!(!second_backup.exists(), "恢复后不得留下第二份 backup");
    }

    #[test]
    fn paired_publication_restores_first_file_when_second_backup_fails() {
        let directory = tempfile::tempdir().expect("应建立备份失败测试目录");
        let first_temporary = directory.path().join(".manual.toml.tmp");
        let second_temporary = directory.path().join(".ownership.jsonl.tmp");
        let first_target = directory.path().join("manual.toml");
        let second_target = directory.path().join("ownership.jsonl");
        let first_backup = manual_backup_path(&first_target);
        let second_backup = manual_backup_path(&second_target);
        fs::write(&first_target, "old manual").unwrap();
        fs::write(&second_target, "old ownership").unwrap();
        fs::write(&first_temporary, "new manual").unwrap();
        fs::write(&second_temporary, "new ownership").unwrap();

        let error = publish_staged_pair(
            &first_temporary,
            &first_target,
            &first_backup,
            &second_temporary,
            &second_target,
            &second_backup,
            &InjectedPairPublisher {
                fail_second_backup: true,
                ..InjectedPairPublisher::default()
            },
        )
        .expect_err("第二份备份失败必须恢复已备份的第一份");

        assert!(matches!(error, ManualDocumentError::Write { path, .. } if path == second_target));
        assert_eq!(fs::read_to_string(&first_target).unwrap(), "old manual");
        assert_eq!(fs::read_to_string(&second_target).unwrap(), "old ownership");
        assert!(!first_backup.exists());
        assert!(!second_backup.exists());
    }

    #[test]
    fn paired_publication_keeps_primary_and_rollback_failures_for_recovery() {
        let directory = tempfile::tempdir().expect("应建立成对发布恢复测试目录");
        let first_temporary = directory.path().join(".manual.toml.tmp");
        let second_temporary = directory.path().join(".ownership.jsonl.tmp");
        let first_target = directory.path().join("manual.toml");
        let second_target = directory.path().join("ownership.jsonl");
        let first_backup = manual_backup_path(&first_target);
        let second_backup = manual_backup_path(&second_target);
        fs::write(&first_target, "old manual").expect("应建立旧 Manual");
        fs::write(&second_target, "old ownership").expect("应建立旧所有权");
        fs::write(&first_temporary, "new manual").expect("应建立第一份临时文件");
        fs::write(&second_temporary, "new ownership").expect("应建立第二份临时文件");

        let error = publish_staged_pair(
            &first_temporary,
            &first_target,
            &first_backup,
            &second_temporary,
            &second_target,
            &second_backup,
            &InjectedPairPublisher {
                fail_second_commit: true,
                fail_published_removal: true,
                fail_backup_restore: true,
                ..InjectedPairPublisher::default()
            },
        )
        .expect_err("撤回失败必须保留可恢复错误");

        assert!(matches!(
            error,
            ManualDocumentError::PairedPublicationRollback {
                operation,
                failures,
            } if failures.len() == 2
                && failures.iter().any(|failure| failure.path == first_target)
                && failures.iter().any(|failure| failure.path == second_target)
                && matches!(operation.as_ref(), ManualDocumentError::Write { path, .. } if path == &second_target)
        ));
        assert_eq!(fs::read_to_string(&first_target).unwrap(), "new manual");
        assert_eq!(fs::read_to_string(&first_backup).unwrap(), "old manual");
        assert!(!second_target.exists());
        assert_eq!(fs::read_to_string(&second_backup).unwrap(), "old ownership");
    }

    #[test]
    fn paired_publication_reports_applied_finalization_when_backup_cleanup_fails() {
        let directory = tempfile::tempdir().expect("应建立收尾失败测试目录");
        let first_temporary = directory.path().join(".manual.toml.tmp");
        let second_temporary = directory.path().join(".ownership.jsonl.tmp");
        let first_target = directory.path().join("manual.toml");
        let second_target = directory.path().join("ownership.jsonl");
        let first_backup = manual_backup_path(&first_target);
        let second_backup = manual_backup_path(&second_target);
        fs::write(&first_target, "old manual").unwrap();
        fs::write(&second_target, "old ownership").unwrap();
        fs::write(&first_temporary, "new manual").unwrap();
        fs::write(&second_temporary, "new ownership").unwrap();

        let error = publish_staged_pair(
            &first_temporary,
            &first_target,
            &first_backup,
            &second_temporary,
            &second_target,
            &second_backup,
            &InjectedPairPublisher {
                fail_backup_cleanup: true,
                ..InjectedPairPublisher::default()
            },
        )
        .expect_err("backup 清理失败必须报告已经生效但收尾失败");
        assert!(matches!(
            &error,
            ManualDocumentError::PairedPublicationFinalization { failures }
                if failures.len() == 2
        ));
        assert_eq!(fs::read_to_string(&first_target).unwrap(), "new manual");
        assert_eq!(fs::read_to_string(&second_target).unwrap(), "new ownership");
        assert_eq!(fs::read_to_string(&first_backup).unwrap(), "old manual");
        assert_eq!(fs::read_to_string(&second_backup).unwrap(), "old ownership");

        let error = ManualCommandError::from_document(error);
        let localizer = UiLocalizer::new(crate::i18n::UiLocale::SimplifiedChinese);
        let mut stderr = Vec::new();
        render_manual_command_error(&error, &localizer, &mut stderr).unwrap();
        let stderr = String::from_utf8(stderr).unwrap();
        assert!(stderr.contains(&render_state_effect_impact(
            StateEffect::AppliedFinalizationFailed,
            &localizer,
        )));
        assert!(!stderr.contains("injected rollback failure"));

        let retry = atomic_replace_pair(
            &first_target,
            b"retry manual",
            &second_target,
            b"retry ownership",
            &CooperativeCancellation::default(),
        )
        .expect_err("遗留 backup 必须在下一次 staging 前明确拒绝");
        assert!(matches!(
            &retry,
            ManualDocumentError::ExistingBackup { path }
                if path == &first_backup || path == &second_backup
        ));
        assert_eq!(fs::read_to_string(&first_target).unwrap(), "new manual");
        assert_eq!(fs::read_to_string(&second_target).unwrap(), "new ownership");
        let mut stderr = Vec::new();
        render_manual_command_error(
            &ManualCommandError::from_document(retry),
            &localizer,
            &mut stderr,
        )
        .unwrap();
        let stderr = String::from_utf8(stderr).unwrap();
        assert!(stderr.contains(".backup"));
        assert!(stderr.contains(&render_state_effect_impact(
            StateEffect::RecoveryRequired,
            &localizer,
        )));
    }

    #[test]
    fn rpg_maker_ownership_jsonl_follows_manual_order_and_has_only_public_fields() {
        let directory = tempfile::tempdir().expect("应建立所有权导出测试目录");
        let manual = directory.path().join("manual.toml");
        let ownership = directory.path().join("ownership.jsonl");
        let mut builtin = indexed_entry(ManualTranslationType::Fixed, &["Actor"]);
        builtin.id = "Actors.json:1:name".to_owned();
        let mut rules = indexed_entry(ManualTranslationType::Free, &["Quest"]);
        rules.id = "plugins.js:Quest:Title".to_owned();
        rules.rpg_maker_owner = Some(ManualRpgMakerOwner::Rules { rule_number: 7 });
        let index = ManualTranslationIndex::new(vec![builtin, rules]).expect("测试 ID 必须唯一");

        let count = export_rpg_maker_manual_documents_with_cancellation(
            &manual,
            &ownership,
            &index,
            &CooperativeCancellation::default(),
        )
        .expect("Manual 与所有权清单应成对导出");

        assert_eq!(count, 2);
        let document: ManualDocument =
            toml::from_str(&fs::read_to_string(&manual).expect("Manual TOML 应可读取"))
                .expect("Manual TOML 应可解析");
        assert_eq!(
            document
                .translation
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            ["Actors.json:1:name", "plugins.js:Quest:Title"]
        );
        assert_eq!(
            fs::read_to_string(&ownership).expect("所有权 JSONL 应可读取"),
            concat!(
                "{\"manual_id\":\"Actors.json:1:name\",\"owner\":\"builtin\"}\n",
                "{\"manual_id\":\"plugins.js:Quest:Title\",\"owner\":\"rules\",\"rule_number\":7}\n",
            )
        );
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
    fn fixed_manual_translation_may_intentionally_replace_nonblank_source_with_blank() {
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

        assert_eq!(report.valid, 1);
        assert_eq!(report.writes[0].translation, [""]);
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
            ManualDocumentError::ExistingTemporary { path } if path == &temporary
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
        fs::create_dir(&temporary).unwrap();

        let source = cleanup_manual_temporary_after_failure(
            temporary.clone(),
            ManualDocumentError::Cancelled,
        );

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
    fn rollback_diagnostic_never_exposes_raw_io_error_text() {
        let localizer = UiLocalizer::new(crate::i18n::UiLocale::SimplifiedChinese);
        let source =
            ManualCommandError::from_document(ManualDocumentError::PairedPublicationRollback {
                operation: Box::new(ManualDocumentError::Write {
                    path: PathBuf::from("ownership.jsonl"),
                    source: io::Error::other("PRIMARY_SECRET_SYSTEM_TEXT"),
                }),
                failures: vec![ManualPairIoFailure {
                    path: PathBuf::from("manual.toml"),
                    source: io::Error::other("ROLLBACK_SECRET_SYSTEM_TEXT"),
                }],
            });
        let report = source.diagnostic_report();
        assert_eq!(report.effect(), StateEffect::RecoveryRequired);
        assert_eq!(report.related().len(), 1);
        assert_eq!(
            report.related()[0].relation(),
            RelatedFailureRelation::Rollback
        );
        assert_eq!(
            report.related()[0].report().primary().code(),
            "filesystem.recovery_artifact_io"
        );
        let serialized = serde_json::to_string(&report).expect("Manual rollback 诊断应可序列化");
        assert!(!serialized.contains("PRIMARY_SECRET_SYSTEM_TEXT"));
        assert!(!serialized.contains("ROLLBACK_SECRET_SYSTEM_TEXT"));
        let mut stderr = Vec::new();
        render_manual_command_error(&source, &localizer, &mut stderr).unwrap();
        let stderr = String::from_utf8(stderr).unwrap();

        assert!(!stderr.contains("PRIMARY_SECRET_SYSTEM_TEXT"));
        assert!(!stderr.contains("ROLLBACK_SECRET_SYSTEM_TEXT"));
        assert!(
            stderr.contains(&localizer.format(UiMessage::DiagnosticRelated {
                relation: RelatedFailureRelation::Rollback.as_str(),
            }))
        );
        assert!(stderr.contains(&render_state_effect_impact(
            StateEffect::RecoveryRequired,
            &localizer,
        )));
    }

    #[test]
    fn missing_temporary_after_failure_counts_as_successful_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let temporary = directory.path().join(".manual.toml.tmp");

        let source =
            cleanup_manual_temporary_after_failure(temporary, ManualDocumentError::Cancelled);

        assert!(matches!(source, ManualDocumentError::Cancelled));
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
        connection
            .execute_batch(
                "CREATE TABLE translation_resource (
                     resource_kind TEXT PRIMARY KEY,
                     canonical_json TEXT NOT NULL
                 );
                 INSERT INTO translation_resource VALUES ('placeholder_rules', '[]');
                 CREATE TABLE generic_file (relative_path BLOB, ordinal INTEGER);
                 CREATE TABLE generic_group (
                     relative_path BLOB, group_id TEXT, ordinal INTEGER, kind TEXT
                 );
                 CREATE TABLE generic_unit (
                     group_id TEXT, unit_id TEXT, ordinal INTEGER,
                     source_text TEXT, translation TEXT
                 );
                 CREATE TABLE generic_manual_translation (
                     group_id TEXT, unit_id TEXT, readable_id TEXT,
                     source_json TEXT, translation_json TEXT,
                     applicability_fingerprint BLOB
                 );
                 INSERT INTO generic_manual_translation VALUES (
                     'detached', 'unit', 'detached-id',
                     'invalid source', 'invalid translation', X'00'
                 );",
            )
            .expect("应建立包含无效脱离记录的 Generic 数据库");
        drop(connection);
        let document = directory.path().join("manual.toml");
        fs::write(&document, "").expect("应建立空 Manual 文件");
        let cancellation = CooperativeCancellation::default();

        let checked = execute_generic_manual_command(
            &database,
            ManualOperation::Check,
            &document,
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
            &cancellation,
        )
        .expect("Manual apply 不应读取脱离当前位置的记录");
        assert!(matches!(
            applied,
            ManualCommandSummary::Applied { applied: 0, .. }
        ));
    }

    #[test]
    fn rpg_maker_manual_check_and_apply_ignore_invalid_detached_records() {
        let directory = tempfile::tempdir().expect("应建立测试目录");
        let database = directory.path().join("project.db");
        let connection = Connection::open(&database).expect("应建立 RPG Maker 测试数据库");
        connection
            .execute_batch(
                "CREATE TABLE rpg_maker_translation_resource (
                     resource_kind TEXT PRIMARY KEY,
                     canonical_json TEXT NOT NULL
                 );
                 INSERT INTO rpg_maker_translation_resource VALUES (
                     'placeholder_rules', '[]'
                 );
                 CREATE TABLE rpg_maker_text_group (
                     owner TEXT, group_id TEXT, group_location TEXT,
                     group_kind TEXT, projection_recipe_json TEXT,
                     semantic_order_key BLOB
                 );
                 CREATE TABLE rpg_maker_text_unit (
                     owner TEXT, group_id TEXT, unit_role TEXT,
                     rule_number INTEGER,
                     source_content_json TEXT, source_context_json TEXT,
                     translation_content_json TEXT, semantic_order_key BLOB
                 );
                 CREATE TABLE rpg_maker_manual_translation (
                     owner TEXT, group_location TEXT, unit_role TEXT,
                     readable_id TEXT, translation_type TEXT,
                     source_json TEXT, translation_json TEXT,
                     applicability_fingerprint BLOB
                 );
                 INSERT INTO rpg_maker_manual_translation VALUES (
                     'detached', 'location', 'role', 'detached-id', 'invalid',
                     'invalid source', 'invalid translation', X'00'
                 );",
            )
            .expect("应建立包含无效脱离记录的 RPG Maker 数据库");
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
                ownership_file: None,
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
