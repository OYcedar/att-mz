//! MV、MZ 与 Generic 共用的 TOML 人工补译契约。

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::generic::{
    GenericCompiledPlaceholderRules, GenericPlaceholderService,
    validate_translation_placeholders_with_cancellation,
};
use crate::language::{LanguageId, LanguageModule, LanguageModuleCatalog, LanguagePair};
use crate::rpg_maker::RpgMakerEngine;
use crate::rpg_maker::asset::RpgMakerAssetOwner;
use crate::rpg_maker::location_codec::{RpgMakerLocationCodec, RpgMakerProjectionCodec};
use crate::rpg_maker::model::{TextUnitContent, TextUnitRole};
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
    pub(crate) reason: String,
    pub(crate) help: String,
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
            Self::InvalidUtf8 { .. } => None,
        }
    }
}

pub(crate) fn export_manual_document(
    path: &Path,
    index: &ManualTranslationIndex,
) -> Result<usize, ManualDocumentError> {
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
    atomic_replace(path, encoded.as_bytes())?;
    Ok(count)
}

pub(crate) fn check_manual_document(
    path: &Path,
    index: &ManualTranslationIndex,
    mut validate_placeholders: impl FnMut(&ManualTranslationEntry, &[String]) -> Result<(), String>,
) -> Result<ManualCheckReport, ManualDocumentError> {
    let bytes = fs::read(path).map_err(|source| ManualDocumentError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let source = std::str::from_utf8(&bytes).map_err(|_| ManualDocumentError::InvalidUtf8 {
        path: path.to_path_buf(),
    })?;
    let document = toml::from_str::<ManualDocument>(source).map_err(|source| {
        ManualDocumentError::InvalidToml {
            path: path.to_path_buf(),
            source,
        }
    })?;
    Ok(check_document(document, index, &mut validate_placeholders))
}

fn check_document(
    document: ManualDocument,
    index: &ManualTranslationIndex,
    validate_placeholders: &mut impl FnMut(&ManualTranslationEntry, &[String]) -> Result<(), String>,
) -> ManualCheckReport {
    let mut report = ManualCheckReport::default();
    let mut seen = BTreeSet::new();
    for item in document.translation {
        let id = item.id.clone();
        if !seen.insert(id.clone()) {
            push_issue(
                &mut report,
                id,
                "同一位置在文件中出现了多次",
                "只保留一条 translation",
            );
            continue;
        }
        let Some(current) = index.get(&item.id) else {
            push_issue(
                &mut report,
                id,
                "当前项目中没有这个位置",
                "重新运行 manual export",
            );
            continue;
        };
        if let Some(line) = invalid_line(&item.source) {
            push_issue(
                &mut report,
                id,
                format!("source 第 {} 项包含换行或 NUL", line + 1),
                "重新运行 manual export，不要把换行写进数组项",
            );
            continue;
        }
        if item.source != current.source {
            push_issue(
                &mut report,
                id,
                "当前原文已经变化",
                "重新运行 manual export 后再填写译文",
            );
            continue;
        }
        if item.kind != current.kind {
            push_issue(
                &mut report,
                id,
                "type 与当前位置的行数规则不一致",
                "保留 manual export 生成的 type",
            );
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
                format!("translation 第 {} 项包含换行或 NUL", line + 1),
                "用数组项表达分行，并删除控制字符",
            );
            continue;
        }
        if current.kind == ManualTranslationType::Fixed {
            if item.translation.len() != current.source.len() {
                push_issue(
                    &mut report,
                    id,
                    format!(
                        "fixed 译文需要 {} 项，当前为 {} 项",
                        current.source.len(),
                        item.translation.len()
                    ),
                    "保持 translation 与 source 的数组长度一致",
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
                    format!("fixed 译文第 {} 项必须保留空槽", slot + 1),
                    "把对应 translation 数组项改为空字符串",
                );
                continue;
            }
        }
        if let Err(reason) = validate_placeholders(current, &item.translation) {
            push_issue(
                &mut report,
                id,
                reason,
                "保留原文中的控制码和 Placeholder，并保持必要顺序",
            );
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
    report
}

fn invalid_line(lines: &[String]) -> Option<usize> {
    lines.iter().position(|line| {
        line.chars()
            .any(|character| matches!(character, '\r' | '\n' | '\0'))
    })
}

fn push_issue(
    report: &mut ManualCheckReport,
    id: String,
    reason: impl Into<String>,
    help: impl Into<String>,
) {
    report.errors.push(ManualCheckIssue {
        id,
        reason: reason.into(),
        help: help.into(),
    });
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<(), ManualDocumentError> {
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let temporary = path.with_file_name(format!(".{file_name}.tmp"));
    let write = (|| {
        let mut file = File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        replace_file(&temporary, path)
    })();
    if write.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write.map_err(|source| ManualDocumentError::Write {
        path: path.to_path_buf(),
        source,
    })
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
    pub(crate) detached: Vec<ManualDetachedTranslation>,
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
    language_modules: &LanguageModuleCatalog,
) -> Result<ManualCommandSummary, ManualCommandError> {
    execute_manual_database_command(
        database_path,
        operation,
        file,
        |connection| load_generic_manual_snapshot(connection, language_modules),
        apply_generic_manual_translations,
    )
}

pub(crate) fn execute_rpg_maker_manual_command(
    database_path: &Path,
    engine: RpgMakerEngine,
    operation: ManualOperation,
    file: &Path,
    language_modules: &LanguageModuleCatalog,
) -> Result<ManualCommandSummary, ManualCommandError> {
    execute_manual_database_command(
        database_path,
        operation,
        file,
        |connection| load_rpg_maker_manual_snapshot(connection, engine, language_modules),
        apply_rpg_maker_manual_translations,
    )
}

fn execute_manual_database_command(
    database_path: &Path,
    operation: ManualOperation,
    file: &Path,
    mut load_snapshot: impl FnMut(&Connection) -> Result<ManualProjectSnapshot, ManualDatabaseError>,
    mut apply: impl FnMut(
        &Connection,
        &[ValidatedManualTranslation],
    ) -> Result<usize, ManualDatabaseError>,
) -> Result<ManualCommandSummary, ManualCommandError> {
    match operation {
        ManualOperation::Export | ManualOperation::Check => {
            let mut connection =
                open_read_only(database_path).map_err(ManualCommandError::Database)?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Deferred)
                .map_err(ManualDatabaseError::from)
                .map_err(ManualCommandError::Database)?;
            let snapshot = load_snapshot(&transaction).map_err(ManualCommandError::Database)?;
            transaction
                .commit()
                .map_err(ManualDatabaseError::from)
                .map_err(ManualCommandError::Database)?;
            execute_manual_read_operation(operation, file, &snapshot)
        }
        ManualOperation::Apply => {
            let mut connection =
                open_read_write(database_path).map_err(ManualCommandError::Database)?;
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(ManualDatabaseError::from)
                .map_err(ManualCommandError::Database)?;
            let snapshot = load_snapshot(&transaction).map_err(ManualCommandError::Database)?;
            let summary =
                apply_manual_snapshot(file, &snapshot, |writes| apply(&transaction, writes))?;
            transaction
                .commit()
                .map_err(ManualDatabaseError::from)
                .map_err(ManualCommandError::Database)?;
            Ok(summary)
        }
    }
}

fn execute_manual_read_operation(
    operation: ManualOperation,
    file: &Path,
    snapshot: &ManualProjectSnapshot,
) -> Result<ManualCommandSummary, ManualCommandError> {
    match operation {
        ManualOperation::Export => export_manual_document(file, &snapshot.index)
            .map(|entries| ManualCommandSummary::Exported {
                entries,
                file: file.to_path_buf(),
            })
            .map_err(ManualCommandError::Document),
        ManualOperation::Check => {
            let report = check_manual_snapshot(file, snapshot)?;
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
) -> Result<ManualCheckReport, ManualCommandError> {
    check_manual_document(file, &snapshot.index, |entry, translation| {
        snapshot.placeholders.validate(entry, translation)
    })
    .map_err(ManualCommandError::Document)
}

fn apply_manual_snapshot(
    file: &Path,
    snapshot: &ManualProjectSnapshot,
    apply: impl FnOnce(&[ValidatedManualTranslation]) -> Result<usize, ManualDatabaseError>,
) -> Result<ManualCommandSummary, ManualCommandError> {
    let report = check_manual_snapshot(file, snapshot)?;
    if !report.is_valid() {
        return Err(ManualCommandError::InvalidEntries(report));
    }
    let applied = apply(&report.writes).map_err(ManualCommandError::Database)?;
    Ok(ManualCommandSummary::Applied { report, applied })
}

#[derive(Debug)]
pub(crate) enum ManualCommandError {
    Document(ManualDocumentError),
    Database(ManualDatabaseError),
    InvalidEntries(ManualCheckReport),
}

impl fmt::Display for ManualCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
            Self::InvalidEntries(_) => None,
        }
    }
}

pub(crate) fn render_manual_command_error(
    error: &ManualCommandError,
    stderr: &mut dyn Write,
) -> io::Result<()> {
    match error {
        ManualCommandError::InvalidEntries(report) => {
            writeln!(
                stderr,
                "有效 {}，未填写 {}，错误 {}",
                report.valid,
                report.unfilled,
                report.errors.len()
            )?;
            for issue in &report.errors {
                writeln!(
                    stderr,
                    "{}：{}；{}。",
                    issue.id,
                    issue.reason.trim_end_matches(['。', '；']),
                    issue.help.trim_end_matches(['。', '；'])
                )?;
            }
            Ok(())
        }
        ManualCommandError::Document(source) => {
            writeln!(stderr, "有效 0，未填写 0，错误 1")?;
            let (object, reason, help) = manual_document_issue(source);
            writeln!(stderr, "{object}：{reason}；{help}。")
        }
        ManualCommandError::Database(source) => {
            writeln!(stderr, "有效 0，未填写 0，错误 1")?;
            let (reason, help) = manual_database_issue(source);
            writeln!(stderr, "项目数据库：{reason}；{help}。")
        }
    }
}

fn manual_document_issue(source: &ManualDocumentError) -> (String, &'static str, &'static str) {
    match source {
        ManualDocumentError::Read { path, .. } => (
            path.display().to_string(),
            "无法读取文件",
            "确认文件存在并且当前用户可以读取",
        ),
        ManualDocumentError::InvalidUtf8 { path } => (
            path.display().to_string(),
            "文件不是 UTF-8 TOML",
            "把文件保存为 UTF-8 后重试",
        ),
        ManualDocumentError::InvalidToml { path, .. } => (
            path.display().to_string(),
            "TOML 语法、字段或值类型无效",
            "只保留 [[translation]] 及 id、type、source、translation，并把 source 和 translation 写成字符串数组",
        ),
        ManualDocumentError::Encode(_) => (
            "Manual TOML".to_owned(),
            "无法生成导出内容",
            "重新运行 manual export；问题持续时报告该故障",
        ),
        ManualDocumentError::Write { path, .. } => (
            path.display().to_string(),
            "无法写入文件",
            "确认目标目录存在并且文件没有被其他程序占用",
        ),
    }
}

fn manual_database_issue(source: &ManualDatabaseError) -> (String, &'static str) {
    match source {
        ManualDatabaseError::Sqlite(_) => (
            "无法读取或修改当前项目".to_owned(),
            "确认项目已经 Init 和 Extract，并且数据库没有被其他程序占用",
        ),
        ManualDatabaseError::InvalidProject(reason) => (
            reason.clone(),
            "按提示修正项目或配置后重新运行 manual export",
        ),
        ManualDatabaseError::Index(source) => (
            source.to_string(),
            "修正产生冲突位置的 Extract 或 Rules 配置后重新导出",
        ),
    }
}

#[derive(Debug)]
pub(crate) enum ManualDatabaseError {
    Sqlite(rusqlite::Error),
    InvalidProject(String),
    Index(ManualIndexError),
}

impl fmt::Display for ManualDatabaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
            Self::InvalidProject(_) => None,
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

pub(crate) fn load_generic_manual_snapshot(
    connection: &Connection,
    language_modules: &LanguageModuleCatalog,
) -> Result<ManualProjectSnapshot, ManualDatabaseError> {
    let source_language = load_generic_source_language(connection, language_modules)?;
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
    let entries = load_generic_entries(connection, &service, &compiled, source_language.as_ref())?;
    let detached = load_detached_generic_manual_translations(connection)?;
    Ok(ManualProjectSnapshot {
        index: ManualTranslationIndex::new(entries)?,
        placeholders: ManualPlaceholderValidator::Generic { service, compiled },
        detached,
    })
}

fn load_generic_entries(
    connection: &Connection,
    placeholder_service: &GenericPlaceholderService,
    placeholder_rules: &GenericCompiledPlaceholderRules,
    source_language: &dyn LanguageModule,
) -> Result<Vec<ManualTranslationEntry>, ManualDatabaseError> {
    let mut statement = connection.prepare(
        "SELECT f.relative_path, g.group_id, g.ordinal, g.kind,
                u.unit_id, u.ordinal, u.source_text, u.translation,
                manual.readable_id, manual.translation_type,
                manual.source_json, manual.translation_json,
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
        let stored_manual = parse_stored_manual_translation(
            row.get(8)?,
            row.get(9)?,
            row.get(10)?,
            row.get(11)?,
            row.get(12)?,
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
        let active = generic_source_needs_translation(
            &id,
            &kind,
            &source_text,
            placeholder_service,
            placeholder_rules,
            source_language,
        )?;
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

pub(crate) fn load_rpg_maker_manual_snapshot(
    connection: &Connection,
    engine: RpgMakerEngine,
    language_modules: &LanguageModuleCatalog,
) -> Result<ManualProjectSnapshot, ManualDatabaseError> {
    let (language_pair, source_language) =
        load_rpg_maker_language_context(connection, language_modules)?;
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
    let semantics = ResolvedTranslationSemantics::new(
        engine,
        language_pair,
        Arc::new(CompiledTerminology::empty()),
        service.clone(),
        compiled.clone(),
        source_language,
        Sha256Fingerprint::from_bytes([0; 32]),
    );
    let entries = load_rpg_maker_entries(connection, &semantics)?;
    let detached = load_detached_rpg_maker_manual_translations(connection)?;
    Ok(ManualProjectSnapshot {
        index: ManualTranslationIndex::new(entries)?,
        placeholders: ManualPlaceholderValidator::RpgMaker {
            engine,
            service,
            compiled,
        },
        detached,
    })
}

fn load_rpg_maker_entries(
    connection: &Connection,
    semantics: &ResolvedTranslationSemantics,
) -> Result<Vec<ManualTranslationEntry>, ManualDatabaseError> {
    let mut statement = connection.prepare(
        "SELECT g.owner, g.group_location, g.group_kind, g.projection_recipe_json,
                u.unit_role, u.source_content_json, u.source_context_json,
                u.translation_content_json, manual.readable_id,
                manual.translation_type, manual.source_json,
                manual.translation_json, manual.applicability_fingerprint
         FROM rpg_maker_text_group AS g
         JOIN rpg_maker_text_unit AS u
           ON u.owner = g.owner AND u.group_id = g.group_id
         LEFT JOIN rpg_maker_manual_translation AS manual
           ON manual.owner = g.owner
          AND manual.group_location = g.group_location
          AND manual.unit_role = u.unit_role
         ORDER BY g.owner, g.semantic_order_key, u.semantic_order_key",
    )?;
    let mut rows = statement.query([])?;
    let mut entries = Vec::new();
    while let Some(row) = rows.next()? {
        let owner_raw: String = row.get(0)?;
        let group_location_raw: String = row.get(1)?;
        let kind_raw: String = row.get(2)?;
        let recipe_json: String = row.get(3)?;
        let role_raw: String = row.get(4)?;
        let source_json: String = row.get(5)?;
        let context_json: String = row.get(6)?;
        let automatic: Option<String> = row.get(7)?;
        let stored_manual = parse_stored_manual_translation(
            row.get(8)?,
            row.get(9)?,
            row.get(10)?,
            row.get(11)?,
            row.get(12)?,
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
        let prepared = semantics
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
            })?;
        entries.push(ManualTranslationEntry {
            id,
            kind: manual_type,
            source,
            locator: ManualTranslationLocator::RpgMaker {
                owner: owner_raw,
                group_location: group_location_raw,
                unit_role: role_raw,
            },
            applicability,
            needs_translation: current_translation.is_none()
                && prepared.status() == PreparedTranslationStatus::Active,
            placeholder_scope: kind_raw,
            current_translation,
            origin,
            outdated_manual,
        });
    }
    Ok(entries)
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
                manual.translation_type, manual.source_json,
                manual.translation_json, manual.applicability_fingerprint
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
        let stored = parse_stored_manual_translation(
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
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
        return Err(ManualCheckIssue {
            id: id.to_owned(),
            reason: "translation 尚未填写".to_owned(),
            help: "提供至少一个字符串数组项".to_owned(),
        });
    }
    let Some(current) = snapshot.index.get(id) else {
        return Err(ManualCheckIssue {
            id: id.to_owned(),
            reason: "当前项目中没有这个位置".to_owned(),
            help: "先用 translation.list 查找当前可读 ID".to_owned(),
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
    report.writes.pop().ok_or_else(|| ManualCheckIssue {
        id: id.to_owned(),
        reason: "人工译文没有通过结构检查".to_owned(),
        help: "按当前位置的 type、空槽和 Placeholder 规则修改译文".to_owned(),
    })
}

pub(crate) fn apply_generic_manual_translations(
    connection: &Connection,
    writes: &[ValidatedManualTranslation],
) -> Result<usize, ManualDatabaseError> {
    for write in writes {
        let ManualTranslationLocator::Generic { group_id, unit_id } = &write.locator else {
            return Err(ManualDatabaseError::InvalidProject(
                "人工译文位置不属于 Generic".to_owned(),
            ));
        };
        connection.execute(
            "INSERT INTO generic_manual_translation (
                 group_id, unit_id, readable_id, translation_type,
                 source_json, translation_json, applicability_fingerprint
             ) VALUES (?1, ?2, ?3, 'free', ?4, ?5, ?6)
             ON CONFLICT (group_id, unit_id) DO UPDATE SET
                 readable_id = excluded.readable_id,
                 translation_type = excluded.translation_type,
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
    for write in writes {
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

fn open_read_only(path: &Path) -> Result<Connection, ManualDatabaseError> {
    Ok(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?)
}

fn open_read_write(path: &Path) -> Result<Connection, ManualDatabaseError> {
    Ok(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?)
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
        assert!(report.errors[0].reason.contains("换行或 NUL"));
        assert!(report.errors[1].reason.contains("出现了多次"));
        assert_eq!(report.errors[2].id, "Unknown.json:1:name");
        assert!(report.errors[2].reason.contains("没有这个位置"));
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
        assert!(report.errors[0].reason.contains("source 第 1 项"));

        write_document(
            &path,
            "[[translation]]\nid = \"Skills.json:798:name\"\ntype = \"fixed\"\nsource = [\"source\"]\ntranslation = [\"译文\"]\n",
        );
        let report = check_manual_document(&path, &index, |_, _| {
            Err("译文没有保留原文中的 Placeholder".to_owned())
        })
        .unwrap();
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].reason.contains("Placeholder"));
        assert!(report.errors[0].help.contains("控制码"));
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
        assert!(report.errors[0].reason.contains("空槽"));

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
            detached: Vec::new(),
        };
        let mut called = false;
        let result = apply_manual_snapshot(&path, &snapshot, |_| {
            called = true;
            Ok(0)
        });
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
