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
use crate::translation::planning_resource::CompiledTerminology;

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
    pub(crate) applicability: Sha256Fingerprint,
    pub(crate) needs_translation: bool,
    pub(crate) placeholder_scope: String,
    pub(crate) current_translation: Option<Vec<String>>,
    pub(crate) origin: Option<ManualTranslationOrigin>,
    pub(crate) outdated_manual: Option<ManualOutdatedTranslation>,
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
}

impl fmt::Display for ManualDocumentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("人工补译操作已取消"),
            Self::Read { path, .. } => write!(formatter, "无法读取 {}", path.display()),
            Self::InvalidUtf8 { path } => {
                write!(formatter, "{} 不是 UTF-8 TOML 文件", path.display())
            }
            Self::InvalidToml { path, .. } => {
                write!(formatter, "{} 不是有效的人工译文 TOML", path.display())
            }
            Self::Encode(_) => formatter.write_str("无法生成人工译文 TOML"),
            Self::Write { path, .. } => write!(formatter, "无法写入 {}", path.display()),
        }
    }
}

impl Error for ManualDocumentError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } | Self::Write { source, .. } => Some(source),
            Self::InvalidToml { source, .. } => Some(source),
            Self::Encode(source) => Some(source),
            Self::Cancelled | Self::InvalidUtf8 { .. } => None,
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
            if let Some(slot) = current
                .source
                .iter()
                .zip(&item.translation)
                .position(|(source, translation)| source.is_empty() && !translation.is_empty())
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
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let temporary = path.with_file_name(format!(".{file_name}.tmp"));
    ensure_manual_document_running(cancellation)?;
    let mut owns_temporary = false;
    let write = (|| -> Result<(), ManualDocumentError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|source| ManualDocumentError::Write {
                path: path.to_path_buf(),
                source,
            })?;
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
    if write.is_err() && owns_temporary {
        let _ = fs::remove_file(&temporary);
    }
    write
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
            execute_manual_read_operation(operation, file, &snapshot, cancellation)
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
    snapshot: &ManualProjectSnapshot,
    cancellation: &CooperativeCancellation,
) -> Result<ManualCommandSummary, ManualCommandError> {
    match operation {
        ManualOperation::Export => {
            export_manual_document_with_cancellation(file, &snapshot.index, cancellation)
                .map(|entries| ManualCommandSummary::Exported {
                    entries,
                    file: file.to_path_buf(),
                })
                .map_err(ManualCommandError::from_document)
        }
        ManualOperation::Check => {
            let report = check_manual_snapshot(file, snapshot, cancellation)?;
            if report.is_valid() {
                Ok(ManualCommandSummary::Checked { report })
            } else {
                Err(ManualCommandError::InvalidEntries(report))
            }
        }
        ManualOperation::Apply => unreachable!("apply 必须在写事务中执行"),
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
}

fn manual_command_database_error(
    source: ManualDatabaseError,
    cancellation: &CooperativeCancellation,
) -> ManualCommandError {
    if cancellation.is_requested() || matches!(source, ManualDatabaseError::Cancelled) {
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
    let message = match summary {
        ManualCommandSummary::Exported { entries, file } => {
            localizer.format(UiMessage::ManualExported {
                entries: manual_count(*entries),
                path: &file.to_string_lossy(),
            })
        }
        ManualCommandSummary::Checked { report } => localizer.format(UiMessage::ManualChecked {
            valid: manual_count(report.valid),
            unfilled: manual_count(report.unfilled),
            errors: manual_count(report.errors.len()),
        }),
        ManualCommandSummary::Applied { report, applied } => {
            localizer.format(UiMessage::ManualApplied {
                applied: manual_count(*applied),
                unfilled: manual_count(report.unfilled),
                errors: manual_count(report.errors.len()),
            })
        }
    };
    writeln!(stdout, "{message}")
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
                    errors: 1,
                })
            )?;
            let (object, reason_code, help_code) = manual_document_issue(source);
            let reason = render_manual_value(localizer, reason_code, 0, 0, 0);
            let help = render_manual_value(localizer, help_code, 0, 0, 0);
            render_manual_issue(&object, &reason, &help, localizer, stderr)
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
        localizer.format(UiMessage::ManualIssue {
            object,
            reason,
            help,
        })
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
        ManualDocumentError::Read { path, .. } => (
            path.display().to_string(),
            "document_read",
            "check_read_access",
        ),
        ManualDocumentError::InvalidUtf8 { path } => (
            path.display().to_string(),
            "document_invalid_utf8",
            "save_as_utf8",
        ),
        ManualDocumentError::InvalidToml { path, .. } => (
            path.display().to_string(),
            "document_invalid_toml",
            "fix_toml_contract",
        ),
        ManualDocumentError::Encode(_) => (
            "Manual TOML".to_owned(),
            "document_encode",
            "retry_or_report",
        ),
        ManualDocumentError::Write { path, .. } => (
            path.display().to_string(),
            "document_write",
            "check_write_access",
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
                manual.translation_json, manual.applicability_fingerprint
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
        let owner = RpgMakerAssetOwner::from_storage_name(&owner_raw).ok_or_else(|| {
            ManualDatabaseError::InvalidProject("人工译文所属来源无效".to_owned())
        })?;
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
            error,
            ManualDocumentError::Write { source, .. }
                if source.kind() == io::ErrorKind::AlreadyExists
        ));
        assert_eq!(fs::read_to_string(temporary).unwrap(), "other writer");
        assert!(!path.exists());
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
        assert!(matches!(
            manual_command_database_error(ManualDatabaseError::Sqlite(source), &cancellation),
            ManualCommandError::Cancelled
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
        assert_eq!(stderr.lines().count(), 2);
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
