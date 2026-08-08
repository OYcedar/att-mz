//! Generic JSONL 的严格外部格式与动态输入扫描。

use std::collections::BTreeMap;
use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::diagnostic::{
    Diagnostic, FileSystemDiagnosticContext, FileSystemDiagnosticStage, FileSystemIssue,
    FileSystemOperation, FileSystemOrdinalKeyPhase, FileSystemProblem, GenericDiagnosticStage,
    GenericIssue, GenericJsonErrorCategory, GenericJsonlLocation, GenericProblem,
    GenericTextViolation, IoFailure, SafeIdentifier, SafePath,
};
use crate::execution::CooperativeCancellation;
use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::json_diagnostic::JsonErrorCategory;
use crate::runtime::windows::{
    FileIdentity, PinnedPath, WindowsFsError, number_of_links, open_directory,
    open_regular_file_for_snapshot_read, pin_directory_without_reparse,
};
use crate::windows_path::{WindowsOrdinalCaseKey, WindowsOrdinalCaseKeyError};

use super::identity::CancellableTextMap;

const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;
const CANCELLATION_CHECK_CHUNK_BYTES: NonZeroUsize =
    NonZeroUsize::new(CANCELLATION_CHECK_BYTES).expect("取消检查块大小必须非零");

/// JSONL 中的一个可翻译单元。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GenericUnit {
    id: String,
    text: String,
}

/// 只由完成字段校验的 Unit 构造路径产生，供 WriteBack 组装 Group 时保留不变量。
pub(crate) struct ValidatedGenericUnit(GenericUnit);

impl GenericUnit {
    #[cfg(test)]
    pub(crate) fn new(id: String, text: String) -> Result<Self, GenericJsonlError> {
        validate_nonempty("unit.id", &id)?;
        validate_text(&text)?;
        Ok(Self { id, text })
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn clone_with_text_with_cancellation(
        &self,
        text: &str,
        cancellation: &CooperativeCancellation,
    ) -> Result<ValidatedGenericUnit, GenericJsonlError> {
        validate_nonempty("unit.id", &self.id)?;
        validate_text_with_cancellation(text, cancellation)?;
        Ok(ValidatedGenericUnit(Self {
            id: clone_string_with_cancellation(&self.id, cancellation)?,
            text: clone_string_with_cancellation(text, cancellation)?,
        }))
    }
}

/// JSONL 的一条物理行，也是翻译任务不可拆开的语义组。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GenericGroup {
    id: String,
    kind: String,
    units: Vec<GenericUnit>,
}

impl GenericGroup {
    #[cfg(test)]
    pub(crate) fn new(
        id: String,
        kind: String,
        units: Vec<GenericUnit>,
    ) -> Result<Self, GenericJsonlError> {
        let value = Self { id, kind, units };
        value.validate()?;
        Ok(value)
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn kind(&self) -> &str {
        &self.kind
    }

    pub(crate) fn units(&self) -> &[GenericUnit] {
        &self.units
    }

    #[cfg(test)]
    fn validate(&self) -> Result<(), GenericJsonlError> {
        self.validate_with_cancellation(&NeverCancelled)
    }

    fn validate_with_cancellation(
        &self,
        cancellation: &(impl JsonlCancellation + ?Sized),
    ) -> Result<(), GenericJsonlError> {
        self.validate_with_unit_check(cancellation, |unit, cancellation| {
            validate_text_with_cancellation(unit.text(), cancellation)
        })
    }

    fn validate_with_unit_check<C: JsonlCancellation + ?Sized>(
        &self,
        cancellation: &C,
        mut validate_unit: impl FnMut(&GenericUnit, &C) -> Result<(), GenericJsonlError>,
    ) -> Result<(), GenericJsonlError> {
        ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Group)?;
        validate_nonempty("group.id", &self.id)?;
        validate_nonempty("group.kind", &self.kind)?;
        if self.units.is_empty() {
            return Err(GenericJsonlError::EmptyUnits {
                group_id: clone_string_with_cancellation(&self.id, cancellation)?,
            });
        }

        let mut unit_ids = CancellableTextMap::with_capacity(self.units.len());
        for (ordinal, unit) in self.units.iter().enumerate() {
            ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Unit)?;
            validate_nonempty("unit.id", unit.id())?;
            validate_unit(unit, cancellation)?;
            if let Some(previous) = unit_ids.insert_with_cancellation(unit.id(), ordinal, || {
                ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Unit)
            })? {
                return Err(GenericJsonlError::DuplicateUnitId {
                    group_id: clone_string_with_cancellation(&self.id, cancellation)?,
                    unit_id: clone_string_with_cancellation(unit.id(), cancellation)?,
                    first_ordinal: previous,
                    second_ordinal: ordinal,
                });
            }
        }
        Ok(())
    }

    pub(crate) fn clone_with_units_with_cancellation(
        &self,
        units: Vec<ValidatedGenericUnit>,
        cancellation: &CooperativeCancellation,
    ) -> Result<Self, GenericJsonlError> {
        let cloned = Self {
            id: clone_string_with_cancellation(&self.id, cancellation)?,
            kind: clone_string_with_cancellation(&self.kind, cancellation)?,
            units: units.into_iter().map(|unit| unit.0).collect(),
        };
        cloned.validate_with_known_valid_units(cancellation)?;
        Ok(cloned)
    }

    fn validate_with_known_valid_units(
        &self,
        cancellation: &(impl JsonlCancellation + ?Sized),
    ) -> Result<(), GenericJsonlError> {
        self.validate_with_unit_check(cancellation, |_unit, _cancellation| Ok(()))
    }
}

/// 一个输入 JSONL 文件及其自然顺序内容。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GenericFile {
    relative_path: PathBuf,
    groups: Vec<GenericGroup>,
    raw_bytes: Vec<u8>,
}

impl GenericFile {
    pub(crate) fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    pub(crate) fn groups(&self) -> &[GenericGroup] {
        &self.groups
    }

    pub(crate) fn raw_bytes(&self) -> &[u8] {
        &self.raw_bytes
    }
}

/// 一次完整扫描产生的内存快照。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GenericInputSnapshot {
    files: Vec<GenericFile>,
    raw_fingerprint: Sha256Fingerprint,
    asset_fingerprint: Sha256Fingerprint,
}

impl GenericInputSnapshot {
    pub(crate) fn files(&self) -> &[GenericFile] {
        &self.files
    }

    pub(crate) const fn raw_fingerprint(&self) -> Sha256Fingerprint {
        self.raw_fingerprint
    }

    pub(crate) const fn asset_fingerprint(&self) -> Sha256Fingerprint {
        self.asset_fingerprint
    }

    pub(crate) fn group_count(&self) -> usize {
        self.files.iter().map(|file| file.groups.len()).sum()
    }

    pub(crate) fn unit_count(&self) -> usize {
        self.files
            .iter()
            .flat_map(|file| &file.groups)
            .map(|group| group.units.len())
            .sum()
    }
}

/// 扫描或解析 Generic JSONL 失败。
#[derive(Debug)]
pub(crate) enum GenericJsonlError {
    Cancelled,
    SourceNotDirectory {
        path: PathBuf,
    },
    Io {
        operation: FileSystemOperation,
        path: PathBuf,
        source: io::Error,
    },
    Windows {
        operation: FileSystemOperation,
        path: PathBuf,
        source: WindowsFsError,
    },
    WindowsOrdinalCaseKey {
        path: PathBuf,
        source: WindowsOrdinalCaseKeyError,
    },
    HardLinkedFile {
        path: PathBuf,
        link_count: u32,
    },
    WindowsCaseConflict {
        first_path: PathBuf,
        second_path: PathBuf,
    },
    NonRegularFileSystemObject {
        path: PathBuf,
    },
    PathEscaped {
        root: PathBuf,
        path: PathBuf,
    },
    InvalidUtf8 {
        path: PathBuf,
        source: GenericUtf8Error,
    },
    BlankLine {
        path: PathBuf,
        line: usize,
    },
    InvalidJson {
        path: PathBuf,
        line: usize,
        serde_line: usize,
        serde_column: usize,
        source: serde_json::Error,
    },
    InvalidGroup {
        path: PathBuf,
        line: usize,
        source: Box<GenericJsonlError>,
    },
    BlankField {
        field: &'static str,
    },
    InvalidText {
        violation: GenericTextViolation,
    },
    EmptyUnits {
        group_id: String,
    },
    DuplicateUnitId {
        group_id: String,
        unit_id: String,
        first_ordinal: usize,
        second_ordinal: usize,
    },
    DuplicateGroupId {
        group_id: String,
        first_path: PathBuf,
        first_line: usize,
        second_path: PathBuf,
        second_line: usize,
    },
    Serialize {
        source: serde_json::Error,
    },
}

/// 分块 UTF-8 校验产生的位置事实。
///
/// 标准库没有公开 `Utf8Error` 构造函数，因此这里保存与其相同的绝对字节位置和错误长度，
/// 避免为了构造错误值再次不可取消地扫描整个文件。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GenericUtf8Error {
    valid_up_to: usize,
    error_len: Option<usize>,
}

impl GenericUtf8Error {
    const fn valid_up_to(&self) -> usize {
        self.valid_up_to
    }

    const fn error_len(&self) -> Option<usize> {
        self.error_len
    }
}

impl fmt::Display for GenericUtf8Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.error_len {
            Some(error_len) => write!(
                formatter,
                "字节 {} 处存在长度为 {error_len} 的非法 UTF-8 序列",
                self.valid_up_to
            ),
            None => write!(
                formatter,
                "字节 {} 之后存在不完整的 UTF-8 序列",
                self.valid_up_to
            ),
        }
    }
}

impl Error for GenericUtf8Error {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JsonlCancellationBoundary {
    Scan,
    FileReadChunk,
    Utf8Chunk,
    Line,
    LineScanChunk,
    BlankLineChunk,
    JsonDeserializeChunk,
    JsonSerializeChunk,
    Group,
    Unit,
    TextChunk,
    ProjectGroup,
    RawFingerprintChunk,
    AssetFingerprintChunk,
}

trait JsonlCancellation: Sync {
    fn ensure_not_cancelled(
        &self,
        boundary: JsonlCancellationBoundary,
    ) -> Result<(), GenericJsonlError>;
}

impl JsonlCancellation for CooperativeCancellation {
    fn ensure_not_cancelled(
        &self,
        _boundary: JsonlCancellationBoundary,
    ) -> Result<(), GenericJsonlError> {
        if self.is_requested() {
            Err(GenericJsonlError::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
struct NeverCancelled;

#[cfg(test)]
impl JsonlCancellation for NeverCancelled {
    fn ensure_not_cancelled(
        &self,
        _boundary: JsonlCancellationBoundary,
    ) -> Result<(), GenericJsonlError> {
        Ok(())
    }
}

impl GenericJsonlError {
    pub(crate) fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
            || matches!(self, Self::InvalidGroup { source, .. } if source.is_cancelled())
    }
}

impl fmt::Display for GenericJsonlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("Generic JSONL 扫描已取消"),
            Self::SourceNotDirectory { path } => {
                write!(formatter, "Generic 输入根不是现存目录：{}", path.display())
            }
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "{} Generic 输入失败：{}（{source}）",
                operation.as_str(),
                path.display()
            ),
            Self::Windows {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "{} Generic 输入失败：{}（{source}）",
                operation.as_str(),
                path.display()
            ),
            Self::WindowsOrdinalCaseKey { path, source } => write!(
                formatter,
                "无法建立 Generic 输入路径 {} 的 Windows ordinal 非大小写身份：{source}",
                path.display()
            ),
            Self::HardLinkedFile { path, link_count } => write!(
                formatter,
                "Generic 输入拒绝硬链接文件：{}（链接数 {link_count}）",
                path.display()
            ),
            Self::WindowsCaseConflict {
                first_path,
                second_path,
            } => write!(
                formatter,
                "Generic 输入包含 Windows 大小写等价路径：{} 与 {}",
                first_path.display(),
                second_path.display()
            ),
            Self::NonRegularFileSystemObject { path } => write!(
                formatter,
                "Generic 输入包含非普通文件系统对象：{}",
                path.display()
            ),
            Self::PathEscaped { root, path } => write!(
                formatter,
                "Generic 输入路径 {} 逃逸出输入根 {}",
                path.display(),
                root.display()
            ),
            Self::InvalidUtf8 { path, source } => {
                write!(
                    formatter,
                    "Generic JSONL 不是有效 UTF-8：{}（{source}）",
                    path.display()
                )
            }
            Self::BlankLine { path, line } => write!(
                formatter,
                "Generic JSONL 不允许空白物理行：{}:{line}",
                path.display()
            ),
            Self::InvalidJson {
                path,
                line,
                serde_line,
                serde_column,
                ..
            } => write!(
                formatter,
                "Generic JSONL 行不符合固定格式：{}:{line}（JSON {serde_line}:{serde_column}）",
                path.display(),
            ),
            Self::InvalidGroup { path, line, source } => write!(
                formatter,
                "Generic JSONL Group 无效：{}:{line}（{source}）",
                path.display()
            ),
            Self::BlankField { field } => write!(formatter, "{field} 不能为空"),
            Self::InvalidText { violation } => write!(
                formatter,
                "unit.text 不允许包含 {}",
                match violation {
                    GenericTextViolation::CarriageReturn => "CR（U+000D）",
                    GenericTextViolation::Nul => "NUL（U+0000）",
                }
            ),
            Self::EmptyUnits { group_id } => {
                write!(formatter, "Generic Group {group_id:?} 的 units 不能为空")
            }
            Self::DuplicateUnitId {
                group_id,
                unit_id,
                first_ordinal,
                second_ordinal,
            } => write!(
                formatter,
                "Generic Group {group_id:?} 内的 Unit ID {unit_id:?} 重复（位置 {first_ordinal} 与 {second_ordinal}）"
            ),
            Self::DuplicateGroupId {
                group_id,
                first_path,
                first_line,
                second_path,
                second_line,
            } => write!(
                formatter,
                "Generic Group ID {group_id:?} 在项目内重复：{}:{first_line} 与 {}:{second_line}",
                first_path.display(),
                second_path.display()
            ),
            Self::Serialize { source } => {
                write!(formatter, "无法序列化 Generic JSONL：{source}")
            }
        }
    }
}

impl Error for GenericJsonlError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Windows { source, .. } => Some(source),
            Self::WindowsOrdinalCaseKey { source, .. } => Some(source),
            Self::InvalidUtf8 { source, .. } => Some(source),
            Self::InvalidJson { source, .. } | Self::Serialize { source } => Some(source),
            Self::InvalidGroup { source, .. } => Some(source.as_ref()),
            Self::Cancelled
            | Self::SourceNotDirectory { .. }
            | Self::HardLinkedFile { .. }
            | Self::WindowsCaseConflict { .. }
            | Self::NonRegularFileSystemObject { .. }
            | Self::PathEscaped { .. }
            | Self::BlankLine { .. }
            | Self::BlankField { .. }
            | Self::InvalidText { .. }
            | Self::EmptyUnits { .. }
            | Self::DuplicateUnitId { .. }
            | Self::DuplicateGroupId { .. } => None,
        }
    }
}

impl GenericJsonlError {
    /// 在仍持有 JSONL 物理位置和后端类别的位置建立当前诊断契约。
    /// 状态影响由调用方按命令事务事实组合，不由解析器猜测。
    pub(crate) fn diagnostic(&self, stage: GenericDiagnosticStage) -> Diagnostic {
        match self {
            Self::SourceNotDirectory { path } => file_system_diagnostic(
                stage,
                FileSystemOperation::Open,
                FileSystemProblem::NotDirectory {
                    path: SafePath::new(path),
                },
            ),
            Self::Io {
                operation,
                path,
                source,
            } => file_system_diagnostic(
                stage,
                *operation,
                FileSystemProblem::Io {
                    path: SafePath::new(path),
                    failure: IoFailure::from_error(source),
                },
            ),
            Self::Windows {
                operation, source, ..
            } => source.diagnostic(FileSystemDiagnosticContext::new(
                file_system_stage(stage),
                *operation,
            )),
            Self::WindowsOrdinalCaseKey { path, source } => match source {
                WindowsOrdinalCaseKeyError::InputTooLarge { maximum, observed } => {
                    file_system_diagnostic(
                        stage,
                        FileSystemOperation::WindowsOrdinalCaseKey,
                        FileSystemProblem::OrdinalKeyTooLarge {
                            path: SafePath::new(path),
                            observed: *observed,
                            maximum: *maximum,
                        },
                    )
                }
                WindowsOrdinalCaseKeyError::WindowsApi { phase, source } => file_system_diagnostic(
                    stage,
                    FileSystemOperation::WindowsOrdinalCaseKey,
                    FileSystemProblem::OrdinalKeyIo {
                        path: SafePath::new(path),
                        phase: match phase {
                            crate::windows_path::WindowsOrdinalCaseKeyPhase::Measure => {
                                FileSystemOrdinalKeyPhase::Measure
                            }
                            crate::windows_path::WindowsOrdinalCaseKeyPhase::Map => {
                                FileSystemOrdinalKeyPhase::Map
                            }
                        },
                        failure: IoFailure::from_error(source),
                    },
                ),
            },
            Self::HardLinkedFile { path, link_count } => file_system_diagnostic(
                stage,
                FileSystemOperation::Metadata,
                FileSystemProblem::HardLink {
                    path: SafePath::new(path),
                    link_count: *link_count,
                },
            ),
            Self::WindowsCaseConflict {
                first_path,
                second_path,
            } => file_system_diagnostic(
                stage,
                FileSystemOperation::WindowsOrdinalCaseKey,
                FileSystemProblem::CaseCollision {
                    first_path: SafePath::new(first_path),
                    second_path: SafePath::new(second_path),
                },
            ),
            Self::NonRegularFileSystemObject { path } => file_system_diagnostic(
                stage,
                FileSystemOperation::Metadata,
                FileSystemProblem::UnexpectedObject {
                    path: SafePath::new(path),
                },
            ),
            Self::PathEscaped { root, path } => file_system_diagnostic(
                stage,
                FileSystemOperation::ResolveDirectory,
                FileSystemProblem::OutsideScope {
                    root: SafePath::new(root),
                    path: SafePath::new(path),
                },
            ),
            Self::InvalidGroup { path, line, source } => {
                group_problem(source, Some(jsonl_location(path, *line))).map_or_else(
                    || source.diagnostic(stage),
                    |problem| generic_diagnostic(stage, problem),
                )
            }
            Self::Cancelled
            | Self::InvalidUtf8 { .. }
            | Self::BlankLine { .. }
            | Self::InvalidJson { .. }
            | Self::BlankField { .. }
            | Self::InvalidText { .. }
            | Self::EmptyUnits { .. }
            | Self::DuplicateUnitId { .. }
            | Self::DuplicateGroupId { .. }
            | Self::Serialize { .. } => generic_diagnostic(
                stage,
                group_problem(self, None).expect("Generic JSONL 格式错误必须具有封闭问题投影"),
            ),
        }
    }
}

fn generic_diagnostic(stage: GenericDiagnosticStage, problem: GenericProblem) -> Diagnostic {
    Diagnostic::generic(GenericIssue::jsonl(stage, problem))
}

fn file_system_diagnostic(
    stage: GenericDiagnosticStage,
    operation: FileSystemOperation,
    problem: FileSystemProblem,
) -> Diagnostic {
    Diagnostic::file_system(FileSystemIssue::new(
        FileSystemDiagnosticContext::new(file_system_stage(stage), operation),
        problem,
    ))
}

const fn file_system_stage(stage: GenericDiagnosticStage) -> FileSystemDiagnosticStage {
    match stage {
        GenericDiagnosticStage::ProjectOpening | GenericDiagnosticStage::Init => {
            FileSystemDiagnosticStage::Project
        }
        GenericDiagnosticStage::Extract => FileSystemDiagnosticStage::Extract,
        GenericDiagnosticStage::Translate | GenericDiagnosticStage::TaskRecord => {
            FileSystemDiagnosticStage::Translate
        }
        GenericDiagnosticStage::WriteBack => FileSystemDiagnosticStage::WriteBack,
    }
}

fn jsonl_location(path: &Path, line: usize) -> GenericJsonlLocation {
    GenericJsonlLocation::line(
        path,
        NonZeroUsize::new(line).expect("Generic JSONL 物理行号必须从一开始"),
    )
}

fn group_problem(
    source: &GenericJsonlError,
    location: Option<GenericJsonlLocation>,
) -> Option<GenericProblem> {
    match source {
        GenericJsonlError::Cancelled => Some(GenericProblem::Cancelled),
        GenericJsonlError::InvalidUtf8 { path, source } => Some(GenericProblem::InvalidUtf8 {
            path: SafePath::new(path),
            valid_up_to: source.valid_up_to(),
            error_len: source.error_len(),
        }),
        GenericJsonlError::BlankLine { path, line } => Some(GenericProblem::BlankJsonlLine {
            location: jsonl_location(path, *line),
        }),
        GenericJsonlError::InvalidJson {
            path,
            line,
            serde_line,
            serde_column,
            source,
        } => Some(GenericProblem::InvalidJson {
            location: jsonl_location(path, *line),
            json_line: *serde_line,
            json_column: *serde_column,
            category: GenericJsonErrorCategory::from(JsonErrorCategory::from(source)),
        }),
        GenericJsonlError::InvalidGroup { source, .. } => group_problem(source, location),
        GenericJsonlError::BlankField { field } => Some(GenericProblem::BlankField {
            location,
            field: SafeIdentifier::from_validated(field),
        }),
        GenericJsonlError::InvalidText { violation } => Some(GenericProblem::InvalidText {
            location,
            violation: *violation,
        }),
        GenericJsonlError::EmptyUnits { group_id } => Some(GenericProblem::EmptyUnits {
            location,
            group_id: SafeIdentifier::new(group_id).ok(),
        }),
        GenericJsonlError::DuplicateUnitId {
            group_id,
            unit_id,
            first_ordinal,
            second_ordinal,
        } => Some(GenericProblem::DuplicateUnitId {
            location,
            group_id: SafeIdentifier::new(group_id).ok(),
            unit_id: SafeIdentifier::new(unit_id).ok(),
            first_ordinal: *first_ordinal,
            second_ordinal: *second_ordinal,
        }),
        GenericJsonlError::DuplicateGroupId {
            group_id,
            first_path,
            first_line,
            second_path,
            second_line,
        } => Some(GenericProblem::DuplicateGroupId {
            group_id: SafeIdentifier::new(group_id).ok(),
            first: jsonl_location(first_path, *first_line),
            second: jsonl_location(second_path, *second_line),
        }),
        GenericJsonlError::Serialize { source } => Some(GenericProblem::SerializeJson {
            category: GenericJsonErrorCategory::from(JsonErrorCategory::from(source)),
            line: source.line(),
            column: source.column(),
        }),
        GenericJsonlError::SourceNotDirectory { .. }
        | GenericJsonlError::Io { .. }
        | GenericJsonlError::Windows { .. }
        | GenericJsonlError::WindowsOrdinalCaseKey { .. }
        | GenericJsonlError::HardLinkedFile { .. }
        | GenericJsonlError::WindowsCaseConflict { .. }
        | GenericJsonlError::NonRegularFileSystemObject { .. }
        | GenericJsonlError::PathEscaped { .. } => None,
    }
}

/// 按目录层级迭代并发读取输入根中的普通 `.jsonl` 文件。
#[cfg(test)]
pub(crate) fn scan_input_tree(
    source_root: &Path,
) -> Result<GenericInputSnapshot, GenericJsonlError> {
    scan_input_tree_with_cancellation(source_root, &CooperativeCancellation::default())
}

struct PinnedInputDirectory {
    relative_path: PathBuf,
    path: PathBuf,
    lifetime: Arc<PinnedDirectoryLifetime>,
}

struct PinnedDirectoryLifetime {
    _root: Option<PinnedPath>,
    _child: Option<fs::File>,
    _parent: Option<Arc<PinnedDirectoryLifetime>>,
}

struct PinnedJsonlFile {
    relative_path: PathBuf,
    path: PathBuf,
    file: fs::File,
    _parent: Arc<PinnedDirectoryLifetime>,
    expected_identity: FileIdentity,
}

pub(crate) fn scan_input_tree_with_cancellation(
    source_root: &Path,
    cancellation: &CooperativeCancellation,
) -> Result<GenericInputSnapshot, GenericJsonlError> {
    ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Scan)?;
    if !source_root.is_dir() {
        return Err(GenericJsonlError::SourceNotDirectory {
            path: source_root.to_path_buf(),
        });
    }

    let pinned_files = collect_jsonl_files(source_root, cancellation)?;

    let parsed = pinned_files
        .into_par_iter()
        .map(|mut pinned_file| {
            ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Scan)?;
            let raw_bytes = read_pinned_jsonl_file_with_probe(&mut pinned_file, cancellation)?;
            parse_file_with_cancellation(pinned_file.relative_path, raw_bytes, cancellation)
        })
        .collect::<Vec<_>>();

    let mut files = Vec::with_capacity(parsed.len());
    for result in parsed {
        files.push(result?);
    }
    ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Scan)?;
    validate_project_group_ids(&files, cancellation)?;

    let raw_fingerprint = fingerprint_raw_files(&files, cancellation)?;
    let asset_fingerprint = fingerprint_assets(&files, cancellation)?;
    Ok(GenericInputSnapshot {
        files,
        raw_fingerprint,
        asset_fingerprint,
    })
}

#[cfg(test)]
fn read_jsonl_file_with_probe(
    path: &Path,
    cancellation: &(impl JsonlCancellation + ?Sized),
) -> Result<Vec<u8>, GenericJsonlError> {
    ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Scan)?;
    let mut pinned =
        crate::runtime::windows::pin_regular_file_for_snapshot_read(path).map_err(|source| {
            GenericJsonlError::Windows {
                operation: FileSystemOperation::Open,
                path: path.to_path_buf(),
                source,
            }
        })?;
    let link_count =
        number_of_links(pinned.file(), path).map_err(|source| GenericJsonlError::Windows {
            operation: FileSystemOperation::Metadata,
            path: path.to_path_buf(),
            source,
        })?;
    if link_count != 1 {
        return Err(GenericJsonlError::HardLinkedFile {
            path: path.to_path_buf(),
            link_count,
        });
    }
    let expected_identity =
        FileIdentity::of(pinned.file(), path).map_err(|source| GenericJsonlError::Windows {
            operation: FileSystemOperation::Metadata,
            path: path.to_path_buf(),
            source,
        })?;
    read_snapshot_file_with_probe(pinned.file_mut(), path, expected_identity, cancellation)
}

fn read_pinned_jsonl_file_with_probe(
    pinned_file: &mut PinnedJsonlFile,
    cancellation: &(impl JsonlCancellation + ?Sized),
) -> Result<Vec<u8>, GenericJsonlError> {
    read_snapshot_file_with_probe(
        &mut pinned_file.file,
        &pinned_file.path,
        pinned_file.expected_identity,
        cancellation,
    )
}

fn read_snapshot_file_with_probe(
    file: &mut fs::File,
    path: &Path,
    expected_identity: FileIdentity,
    cancellation: &(impl JsonlCancellation + ?Sized),
) -> Result<Vec<u8>, GenericJsonlError> {
    ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Scan)?;
    let path = path.to_path_buf();
    let mut raw_bytes = Vec::new();
    let mut buffer = [0_u8; CANCELLATION_CHECK_BYTES];
    loop {
        ensure_not_cancelled(cancellation, JsonlCancellationBoundary::FileReadChunk)?;
        let read = match file.read(&mut buffer) {
            Ok(read) => read,
            Err(source) if source.kind() == io::ErrorKind::Interrupted => continue,
            Err(source) => {
                return Err(GenericJsonlError::Io {
                    operation: FileSystemOperation::Read,
                    path: path.clone(),
                    source,
                });
            }
        };
        if read == 0 {
            break;
        }
        raw_bytes
            .try_reserve(read)
            .map_err(|source| GenericJsonlError::Io {
                operation: FileSystemOperation::Read,
                path: path.clone(),
                source: io::Error::new(io::ErrorKind::OutOfMemory, source),
            })?;
        raw_bytes.extend_from_slice(&buffer[..read]);
    }
    ensure_not_cancelled(cancellation, JsonlCancellationBoundary::FileReadChunk)?;
    let actual_identity =
        FileIdentity::of(file, &path).map_err(|source| GenericJsonlError::Windows {
            operation: FileSystemOperation::Metadata,
            path: path.clone(),
            source,
        })?;
    if actual_identity != expected_identity {
        return Err(GenericJsonlError::Windows {
            operation: FileSystemOperation::Metadata,
            path: path.clone(),
            source: WindowsFsError::FileIdentityChanged { path },
        });
    }
    let link_count = number_of_links(file, &path).map_err(|source| GenericJsonlError::Windows {
        operation: FileSystemOperation::Metadata,
        path: path.clone(),
        source,
    })?;
    if link_count != 1 {
        return Err(GenericJsonlError::HardLinkedFile { path, link_count });
    }
    Ok(raw_bytes)
}

fn windows_input_error(
    operation: FileSystemOperation,
    path: &Path,
    source: WindowsFsError,
) -> GenericJsonlError {
    GenericJsonlError::Windows {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
pub(crate) fn parse_file(
    relative_path: PathBuf,
    raw_bytes: Vec<u8>,
) -> Result<GenericFile, GenericJsonlError> {
    parse_file_with_probe(relative_path, raw_bytes, &NeverCancelled)
}

pub(crate) fn parse_file_with_cancellation(
    relative_path: PathBuf,
    raw_bytes: Vec<u8>,
    cancellation: &CooperativeCancellation,
) -> Result<GenericFile, GenericJsonlError> {
    parse_file_with_probe(relative_path, raw_bytes, cancellation)
}

fn parse_file_with_probe(
    relative_path: PathBuf,
    raw_bytes: Vec<u8>,
    cancellation: &(impl JsonlCancellation + ?Sized),
) -> Result<GenericFile, GenericJsonlError> {
    ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Scan)?;
    match validate_utf8_with_probe(&raw_bytes, cancellation) {
        Ok(()) => {}
        Err(GenericUtf8ErrorOrCancellation::Invalid(source)) => {
            return Err(GenericJsonlError::InvalidUtf8 {
                path: relative_path,
                source,
            });
        }
        Err(GenericUtf8ErrorOrCancellation::Cancellation(source)) => return Err(source),
    }
    // SAFETY: `validate_utf8_with_probe` 已经逐块校验完整字节串；后续代码不再修改
    // `raw_bytes`，物理行切点也只位于 ASCII LF/CR 边界。
    let text = unsafe { std::str::from_utf8_unchecked(&raw_bytes) };
    let mut json_reader = BufReader::with_capacity(
        CANCELLATION_CHECK_BYTES,
        CancellableSliceReader::new(&raw_bytes[..0], cancellation),
    );
    let mut groups = Vec::new();
    let mut start = 0;
    let mut line = 1;
    while start < text.len() {
        ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Line)?;
        let (line_end, consumed) = find_physical_line_end(text.as_bytes(), start, cancellation)?;
        let raw_line = &text[start..line_end];
        let json_line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if is_blank_line_with_probe(json_line, cancellation)? {
            return Err(GenericJsonlError::BlankLine {
                path: relative_path,
                line,
            });
        }
        reset_buffered_slice_reader(&mut json_reader, json_line.as_bytes());
        let group = match deserialize_buffered_group_with_probe(
            &mut json_reader,
            json_line.as_bytes(),
            cancellation,
        ) {
            Ok(group) => group,
            Err(JsonDeserializeError::Cancelled) => return Err(GenericJsonlError::Cancelled),
            Err(JsonDeserializeError::Json {
                source,
                line: serde_line,
                column: serde_column,
            }) => {
                return Err(GenericJsonlError::InvalidJson {
                    path: relative_path,
                    line,
                    serde_line,
                    serde_column,
                    source,
                });
            }
        };
        group
            .validate_with_cancellation(cancellation)
            .map_err(|source| {
                if source.is_cancelled() {
                    source
                } else {
                    GenericJsonlError::InvalidGroup {
                        path: relative_path.clone(),
                        line,
                        source: Box::new(source),
                    }
                }
            })?;
        groups.push(group);
        start += consumed;
        line += 1;
    }
    drop(json_reader);
    ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Scan)?;
    Ok(GenericFile {
        relative_path,
        groups,
        raw_bytes,
    })
}

fn validate_utf8_with_probe(
    raw_bytes: &[u8],
    cancellation: &(impl JsonlCancellation + ?Sized),
) -> Result<(), GenericUtf8ErrorOrCancellation> {
    let mut start = 0;
    while start < raw_bytes.len() {
        ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Utf8Chunk)
            .map_err(GenericUtf8ErrorOrCancellation::Cancellation)?;
        let end = start
            .saturating_add(CANCELLATION_CHECK_BYTES)
            .min(raw_bytes.len());
        match std::str::from_utf8(&raw_bytes[start..end]) {
            Ok(_) => start = end,
            Err(source) => {
                let valid_up_to = start.saturating_add(source.valid_up_to());
                match source.error_len() {
                    Some(error_len) => {
                        ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Utf8Chunk)
                            .map_err(GenericUtf8ErrorOrCancellation::Cancellation)?;
                        return Err(GenericUtf8ErrorOrCancellation::Invalid(GenericUtf8Error {
                            valid_up_to,
                            error_len: Some(error_len),
                        }));
                    }
                    None if end == raw_bytes.len() => {
                        ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Utf8Chunk)
                            .map_err(GenericUtf8ErrorOrCancellation::Cancellation)?;
                        return Err(GenericUtf8ErrorOrCancellation::Invalid(GenericUtf8Error {
                            valid_up_to,
                            error_len: None,
                        }));
                    }
                    None => start = valid_up_to,
                }
            }
        }
    }
    ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Utf8Chunk)
        .map_err(GenericUtf8ErrorOrCancellation::Cancellation)?;
    Ok(())
}

#[derive(Debug)]
enum GenericUtf8ErrorOrCancellation {
    Invalid(GenericUtf8Error),
    Cancellation(GenericJsonlError),
}

fn find_physical_line_end(
    raw_bytes: &[u8],
    start: usize,
    cancellation: &(impl JsonlCancellation + ?Sized),
) -> Result<(usize, usize), GenericJsonlError> {
    let mut cursor = start;
    while cursor < raw_bytes.len() {
        ensure_not_cancelled(cancellation, JsonlCancellationBoundary::LineScanChunk)?;
        let chunk_end = cursor
            .saturating_add(CANCELLATION_CHECK_BYTES)
            .min(raw_bytes.len());
        if let Some(relative_end) = raw_bytes[cursor..chunk_end]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let line_end = cursor + relative_end;
            ensure_not_cancelled(cancellation, JsonlCancellationBoundary::LineScanChunk)?;
            return Ok((line_end, line_end - start + 1));
        }
        cursor = chunk_end;
    }
    ensure_not_cancelled(cancellation, JsonlCancellationBoundary::LineScanChunk)?;
    Ok((raw_bytes.len(), raw_bytes.len() - start))
}

fn is_blank_line_with_probe(
    line: &str,
    cancellation: &(impl JsonlCancellation + ?Sized),
) -> Result<bool, GenericJsonlError> {
    let mut next_check = 0;
    for (offset, character) in line.char_indices() {
        if offset >= next_check {
            ensure_not_cancelled(cancellation, JsonlCancellationBoundary::BlankLineChunk)?;
            next_check = offset.saturating_add(CANCELLATION_CHECK_BYTES);
        }
        if !character.is_whitespace() {
            ensure_not_cancelled(cancellation, JsonlCancellationBoundary::BlankLineChunk)?;
            return Ok(false);
        }
    }
    ensure_not_cancelled(cancellation, JsonlCancellationBoundary::BlankLineChunk)?;
    Ok(true)
}

#[cfg(test)]
fn deserialize_group_with_probe(
    json_line: &[u8],
    cancellation: &(impl JsonlCancellation + ?Sized),
) -> Result<GenericGroup, JsonDeserializeError> {
    let slice_reader = CancellableSliceReader::new(json_line, cancellation);
    let mut reader = BufReader::with_capacity(CANCELLATION_CHECK_BYTES, slice_reader);
    deserialize_buffered_group_with_probe(&mut reader, json_line, cancellation)
}

fn reset_buffered_slice_reader<'bytes, C: JsonlCancellation + ?Sized>(
    reader: &mut BufReader<CancellableSliceReader<'bytes, '_, C>>,
    remaining: &'bytes [u8],
) {
    let buffered = reader.buffer().len();
    reader.consume(buffered);
    reader.get_mut().reset(remaining);
}

fn deserialize_buffered_group_with_probe<C: JsonlCancellation + ?Sized>(
    reader: &mut BufReader<CancellableSliceReader<'_, '_, C>>,
    json_line: &[u8],
    cancellation: &C,
) -> Result<GenericGroup, JsonDeserializeError> {
    match deserialize_buffered_group_once(reader, cancellation) {
        Ok(group) => Ok(group),
        Err(JsonDeserializeAttempt::Cancelled) => Err(JsonDeserializeError::Cancelled),
        Err(JsonDeserializeAttempt::Json(source)) => {
            let (line, column) = normalized_json_error_position(json_line, &source, cancellation)?;
            Err(JsonDeserializeError::Json {
                source,
                line,
                column,
            })
        }
    }
}

fn deserialize_buffered_group_once<C: JsonlCancellation + ?Sized>(
    reader: &mut BufReader<CancellableSliceReader<'_, '_, C>>,
    cancellation: &C,
) -> Result<GenericGroup, JsonDeserializeAttempt> {
    let result = serde_json::from_reader(&mut *reader);
    if reader.get_ref().cancelled
        || cancellation
            .ensure_not_cancelled(JsonlCancellationBoundary::JsonDeserializeChunk)
            .is_err()
    {
        Err(JsonDeserializeAttempt::Cancelled)
    } else {
        result.map_err(JsonDeserializeAttempt::Json)
    }
}

fn normalized_json_error_position(
    json_line: &[u8],
    source: &serde_json::Error,
    cancellation: &(impl JsonlCancellation + ?Sized),
) -> Result<(usize, usize), JsonDeserializeError> {
    let line = source.line();
    let column = source.column();
    if !source.is_data() || line != 1 || column == 0 {
        return finish_json_error_position((line, column), cancellation);
    }

    // IoRead 为了实现 `peek` 会从底层多读取一个字节，而 SliceRead 的 `peek`
    // 不推进索引。Data 错误只可能因此比既有 SliceRead 坐标多一列。用少一个
    // 字节的前缀再做一次同样可取消的解析，可以判断该字节是否只是前瞻字符。
    let prefix_end = column - 1;
    if prefix_end > json_line.len() {
        return finish_json_error_position((line, column), cancellation);
    }
    let slice_reader = CancellableSliceReader::new(&json_line[..prefix_end], cancellation);
    let mut reader = BufReader::with_capacity(CANCELLATION_CHECK_BYTES, slice_reader);
    match deserialize_buffered_group_once(&mut reader, cancellation) {
        Err(JsonDeserializeAttempt::Cancelled) => Err(JsonDeserializeError::Cancelled),
        Err(JsonDeserializeAttempt::Json(prefix_source)) if prefix_source.is_data() => {
            finish_json_error_position((line, column - 1), cancellation)
        }
        Ok(_) | Err(JsonDeserializeAttempt::Json(_)) => {
            finish_json_error_position((line, column), cancellation)
        }
    }
}

fn finish_json_error_position(
    position: (usize, usize),
    cancellation: &(impl JsonlCancellation + ?Sized),
) -> Result<(usize, usize), JsonDeserializeError> {
    if cancellation
        .ensure_not_cancelled(JsonlCancellationBoundary::JsonDeserializeChunk)
        .is_err()
    {
        Err(JsonDeserializeError::Cancelled)
    } else {
        Ok(position)
    }
}

#[derive(Debug)]
enum JsonDeserializeError {
    Cancelled,
    Json {
        source: serde_json::Error,
        line: usize,
        column: usize,
    },
}

#[derive(Debug)]
enum JsonDeserializeAttempt {
    Cancelled,
    Json(serde_json::Error),
}

struct CancellableSliceReader<'bytes, 'cancellation, C: ?Sized> {
    remaining: &'bytes [u8],
    cancellation: &'cancellation C,
    bytes_until_check: usize,
    cancelled: bool,
}

impl<'bytes, 'cancellation, C: JsonlCancellation + ?Sized>
    CancellableSliceReader<'bytes, 'cancellation, C>
{
    const fn new(remaining: &'bytes [u8], cancellation: &'cancellation C) -> Self {
        Self {
            remaining,
            cancellation,
            bytes_until_check: 0,
            cancelled: false,
        }
    }

    fn reset(&mut self, remaining: &'bytes [u8]) {
        self.remaining = remaining;
        self.bytes_until_check = 0;
        self.cancelled = false;
    }
}

impl<C: JsonlCancellation + ?Sized> Read for CancellableSliceReader<'_, '_, C> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.remaining.is_empty() {
            return Ok(0);
        }
        if self.cancelled {
            return Err(io::Error::other("Generic JSONL 解析已取消"));
        }
        if self.bytes_until_check == 0 {
            if self
                .cancellation
                .ensure_not_cancelled(JsonlCancellationBoundary::JsonDeserializeChunk)
                .is_err()
            {
                self.cancelled = true;
                return Err(io::Error::other("Generic JSONL 解析已取消"));
            }
            self.bytes_until_check = CANCELLATION_CHECK_BYTES;
        }
        let copied = output
            .len()
            .min(self.remaining.len())
            .min(self.bytes_until_check);
        output[..copied].copy_from_slice(&self.remaining[..copied]);
        self.remaining = &self.remaining[copied..];
        self.bytes_until_check -= copied;
        Ok(copied)
    }
}

#[cfg(test)]
pub(crate) fn serialize_groups(groups: &[GenericGroup]) -> Result<Vec<u8>, GenericJsonlError> {
    serialize_groups_with_probe(groups, &NeverCancelled)
}

pub(crate) fn serialize_groups_with_cancellation(
    groups: &[GenericGroup],
    cancellation: &CooperativeCancellation,
) -> Result<Vec<u8>, GenericJsonlError> {
    serialize_groups_with_probe(groups, cancellation)
}

fn serialize_groups_with_probe(
    groups: &[GenericGroup],
    cancellation: &(impl JsonlCancellation + ?Sized),
) -> Result<Vec<u8>, GenericJsonlError> {
    ensure_not_cancelled(cancellation, JsonlCancellationBoundary::JsonSerializeChunk)?;
    let mut output = Vec::new();
    for group in groups {
        // 生产路径只会接收解析边界已校验的 Group，或
        // `clone_with_units_with_cancellation` 刚校验过的改写 Group。这里再次全量
        // 校验会让大型 WriteBack 对每个 Unit 和正文做两遍完全相同的扫描。
        let (result, cancelled) = {
            let mut writer = CancellableVecWriter::new(&mut output, cancellation);
            let result = serde_json::to_writer(&mut writer, group);
            (result, writer.cancelled)
        };
        if cancelled {
            return Err(GenericJsonlError::Cancelled);
        }
        ensure_not_cancelled(cancellation, JsonlCancellationBoundary::JsonSerializeChunk)?;
        result.map_err(|source| GenericJsonlError::Serialize { source })?;
        output.push(b'\n');
    }
    ensure_not_cancelled(cancellation, JsonlCancellationBoundary::JsonSerializeChunk)?;
    Ok(output)
}

struct CancellableVecWriter<'a, C: ?Sized> {
    output: &'a mut Vec<u8>,
    cancellation: &'a C,
    bytes_until_check: usize,
    cancelled: bool,
}

impl<'a, C: JsonlCancellation + ?Sized> CancellableVecWriter<'a, C> {
    fn new(output: &'a mut Vec<u8>, cancellation: &'a C) -> Self {
        Self {
            output,
            cancellation,
            bytes_until_check: 0,
            cancelled: false,
        }
    }
}

impl<C: JsonlCancellation + ?Sized> Write for CancellableVecWriter<'_, C> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.bytes_until_check == 0 {
            if self
                .cancellation
                .ensure_not_cancelled(JsonlCancellationBoundary::JsonSerializeChunk)
                .is_err()
            {
                self.cancelled = true;
                return Err(io::Error::other("Generic JSONL 序列化已取消"));
            }
            self.bytes_until_check = CANCELLATION_CHECK_BYTES;
        }
        let written = bytes.len().min(self.bytes_until_check);
        self.output.extend_from_slice(&bytes[..written]);
        self.bytes_until_check -= written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn validate_nonempty(field: &'static str, value: &str) -> Result<(), GenericJsonlError> {
    if value.is_empty() {
        Err(GenericJsonlError::BlankField { field })
    } else {
        Ok(())
    }
}

#[cfg(test)]
fn validate_text(text: &str) -> Result<(), GenericJsonlError> {
    validate_text_with_cancellation(text, &NeverCancelled)
}

fn clone_string_with_cancellation(
    value: &str,
    cancellation: &(impl JsonlCancellation + ?Sized),
) -> Result<String, GenericJsonlError> {
    ensure_not_cancelled(cancellation, JsonlCancellationBoundary::TextChunk)?;
    let mut cloned = String::with_capacity(value.len());
    let mut start = 0;
    while start < value.len() {
        ensure_not_cancelled(cancellation, JsonlCancellationBoundary::TextChunk)?;
        let mut end = start
            .saturating_add(CANCELLATION_CHECK_BYTES)
            .min(value.len());
        while end < value.len() && !value.is_char_boundary(end) {
            end -= 1;
        }
        cloned.push_str(&value[start..end]);
        start = end;
    }
    ensure_not_cancelled(cancellation, JsonlCancellationBoundary::TextChunk)?;
    Ok(cloned)
}

fn validate_text_with_cancellation(
    text: &str,
    cancellation: &(impl JsonlCancellation + ?Sized),
) -> Result<(), GenericJsonlError> {
    let bytes = text.as_bytes();
    for chunk in bytes.chunks(CANCELLATION_CHECK_CHUNK_BYTES.get()) {
        ensure_not_cancelled(cancellation, JsonlCancellationBoundary::TextChunk)?;
        for byte in chunk {
            let violation = match byte {
                b'\r' => Some(GenericTextViolation::CarriageReturn),
                b'\0' => Some(GenericTextViolation::Nul),
                _ => None,
            };
            if let Some(violation) = violation {
                return Err(GenericJsonlError::InvalidText { violation });
            }
        }
    }
    if bytes.is_empty() {
        ensure_not_cancelled(cancellation, JsonlCancellationBoundary::TextChunk)?;
    }
    Ok(())
}

fn collect_jsonl_files(
    source_root: &Path,
    cancellation: &(impl JsonlCancellation + ?Sized),
) -> Result<Vec<PinnedJsonlFile>, GenericJsonlError> {
    ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Scan)?;
    let pinned_root = pin_directory_without_reparse(source_root)
        .map_err(|source| windows_input_error(FileSystemOperation::Open, source_root, source))?;
    let resolved_root = pinned_root.resolved_path().to_path_buf();
    let root_lifetime = Arc::new(PinnedDirectoryLifetime {
        _root: Some(pinned_root),
        _child: None,
        _parent: None,
    });
    let mut pending = vec![PinnedInputDirectory {
        relative_path: PathBuf::new(),
        path: resolved_root.clone(),
        lifetime: root_lifetime,
    }];
    let mut files = BTreeMap::new();
    while !pending.is_empty() {
        ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Scan)?;
        let scanned = pending
            .par_iter()
            .map(|directory| scan_directory(&resolved_root, directory, cancellation))
            .collect::<Vec<_>>();
        pending.clear();
        let mut next_pending = BTreeMap::new();
        for result in scanned {
            let (child_directories, child_files) = result?;
            for child_directory in child_directories {
                ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Scan)?;
                let key = child_directory.relative_path.clone();
                if next_pending.insert(key.clone(), child_directory).is_some() {
                    return Err(GenericJsonlError::WindowsCaseConflict {
                        first_path: key.clone(),
                        second_path: key,
                    });
                }
            }
            for child_file in child_files {
                ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Scan)?;
                let key = child_file.relative_path.clone();
                if files.insert(key.clone(), child_file).is_some() {
                    return Err(GenericJsonlError::WindowsCaseConflict {
                        first_path: key.clone(),
                        second_path: key,
                    });
                }
            }
        }
        for child_directory in next_pending.into_values() {
            ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Scan)?;
            pending.push(child_directory);
        }
    }
    let mut ordered_files = Vec::with_capacity(files.len());
    for file in files.into_values() {
        ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Scan)?;
        ordered_files.push(file);
    }
    ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Scan)?;
    Ok(ordered_files)
}

fn scan_directory(
    resolved_root: &Path,
    directory: &PinnedInputDirectory,
    cancellation: &(impl JsonlCancellation + ?Sized),
) -> Result<(Vec<PinnedInputDirectory>, Vec<PinnedJsonlFile>), GenericJsonlError> {
    ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Scan)?;
    let directory_path = &directory.path;
    let entries = fs::read_dir(directory_path).map_err(|source| GenericJsonlError::Io {
        operation: FileSystemOperation::ListDirectory,
        path: directory_path.to_path_buf(),
        source,
    })?;
    let mut child_directories = Vec::new();
    let mut files = Vec::new();
    let mut windows_names = BTreeMap::<WindowsOrdinalCaseKey, PathBuf>::new();
    for entry in entries {
        ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Scan)?;
        let entry = entry.map_err(|source| GenericJsonlError::Io {
            operation: FileSystemOperation::ListDirectory,
            path: directory_path.to_path_buf(),
            source,
        })?;
        let name = entry.file_name();
        let path = directory_path.join(&name);
        let relative_path = directory.relative_path.join(&name);
        register_windows_name(&mut windows_names, &name, &relative_path, &path)?;

        if !path.starts_with(resolved_root) {
            return Err(GenericJsonlError::PathEscaped {
                root: resolved_root.to_path_buf(),
                path,
            });
        }
        let file_type = entry.file_type().map_err(|source| GenericJsonlError::Io {
            operation: FileSystemOperation::Metadata,
            path: path.clone(),
            source,
        })?;
        if file_type.is_symlink() {
            return Err(GenericJsonlError::Windows {
                operation: FileSystemOperation::ListDirectory,
                path: path.clone(),
                source: WindowsFsError::ReparsePoint { path },
            });
        }
        if file_type.is_dir() {
            let child = open_directory(&path, false)
                .map_err(|source| windows_input_error(FileSystemOperation::Open, &path, source))?;
            let metadata = child.metadata().map_err(|source| GenericJsonlError::Io {
                operation: FileSystemOperation::Metadata,
                path: path.clone(),
                source,
            })?;
            if !metadata.is_dir() {
                return Err(GenericJsonlError::NonRegularFileSystemObject { path });
            }
            let lifetime = Arc::new(PinnedDirectoryLifetime {
                _root: None,
                _child: Some(child),
                _parent: Some(Arc::clone(&directory.lifetime)),
            });
            child_directories.push(PinnedInputDirectory {
                relative_path,
                path,
                lifetime,
            });
            continue;
        }
        if !file_type.is_file() {
            return Err(GenericJsonlError::NonRegularFileSystemObject { path });
        }

        let file = open_regular_file_for_snapshot_read(&path)
            .map_err(|source| windows_input_error(FileSystemOperation::Open, &path, source))?;
        let link_count = number_of_links(&file, &path)
            .map_err(|source| windows_input_error(FileSystemOperation::Metadata, &path, source))?;
        if link_count != 1 {
            return Err(GenericJsonlError::HardLinkedFile { path, link_count });
        }
        if relative_path
            .extension()
            .is_none_or(|extension| extension != "jsonl")
        {
            continue;
        }

        let expected_identity = FileIdentity::of(&file, &path)
            .map_err(|source| windows_input_error(FileSystemOperation::Metadata, &path, source))?;
        files.push(PinnedJsonlFile {
            relative_path,
            path,
            file,
            _parent: Arc::clone(&directory.lifetime),
            expected_identity,
        });
    }
    ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Scan)?;
    Ok((child_directories, files))
}

fn register_windows_name(
    windows_names: &mut BTreeMap<WindowsOrdinalCaseKey, PathBuf>,
    name: &OsStr,
    relative_path: &Path,
    physical_path: &Path,
) -> Result<(), GenericJsonlError> {
    let windows_key = WindowsOrdinalCaseKey::from_os_str(name).map_err(|source| {
        GenericJsonlError::WindowsOrdinalCaseKey {
            path: physical_path.to_path_buf(),
            source,
        }
    })?;
    if let Some(first_path) = windows_names.insert(windows_key, relative_path.to_path_buf()) {
        return Err(GenericJsonlError::WindowsCaseConflict {
            first_path,
            second_path: relative_path.to_path_buf(),
        });
    }
    Ok(())
}

fn ensure_not_cancelled(
    cancellation: &(impl JsonlCancellation + ?Sized),
    boundary: JsonlCancellationBoundary,
) -> Result<(), GenericJsonlError> {
    cancellation.ensure_not_cancelled(boundary)
}

fn validate_project_group_ids(
    files: &[GenericFile],
    cancellation: &(impl JsonlCancellation + ?Sized),
) -> Result<(), GenericJsonlError> {
    let mut group_ids = CancellableTextMap::with_capacity(files.len());
    for file in files {
        for (ordinal, group) in file.groups.iter().enumerate() {
            ensure_not_cancelled(cancellation, JsonlCancellationBoundary::ProjectGroup)?;
            let line = ordinal + 1;
            if let Some((first_path, first_line)) = group_ids.insert_with_cancellation(
                group.id(),
                (file.relative_path(), line),
                || ensure_not_cancelled(cancellation, JsonlCancellationBoundary::ProjectGroup),
            )? {
                return Err(GenericJsonlError::DuplicateGroupId {
                    group_id: clone_string_with_cancellation(group.id(), cancellation)?,
                    first_path: first_path.to_path_buf(),
                    first_line,
                    second_path: file.relative_path.clone(),
                    second_line: line,
                });
            }
        }
    }
    Ok(())
}

fn fingerprint_raw_files(
    files: &[GenericFile],
    cancellation: &(impl JsonlCancellation + ?Sized),
) -> Result<Sha256Fingerprint, GenericJsonlError> {
    let mut hasher = Sha256FramedHasher::new(b"att.generic.raw-input");
    for file in files {
        ensure_not_cancelled(cancellation, JsonlCancellationBoundary::RawFingerprintChunk)?;
        frame_path(&mut hasher, 1, file.relative_path());
        hasher.try_frame_chunks(2, file.raw_bytes(), CANCELLATION_CHECK_CHUNK_BYTES, || {
            ensure_not_cancelled(cancellation, JsonlCancellationBoundary::RawFingerprintChunk)
        })?;
    }
    ensure_not_cancelled(cancellation, JsonlCancellationBoundary::RawFingerprintChunk)?;
    Ok(hasher.finish())
}

fn fingerprint_assets(
    files: &[GenericFile],
    cancellation: &(impl JsonlCancellation + ?Sized),
) -> Result<Sha256Fingerprint, GenericJsonlError> {
    let mut hasher = Sha256FramedHasher::new(b"att.generic.assets");
    for file in files {
        ensure_not_cancelled(
            cancellation,
            JsonlCancellationBoundary::AssetFingerprintChunk,
        )?;
        frame_path(&mut hasher, 1, file.relative_path());
        for group in &file.groups {
            ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Group)?;
            try_frame_asset_bytes(&mut hasher, 2, group.id().as_bytes(), cancellation)?;
            try_frame_asset_bytes(&mut hasher, 3, group.kind().as_bytes(), cancellation)?;
            for unit in group.units() {
                ensure_not_cancelled(cancellation, JsonlCancellationBoundary::Unit)?;
                try_frame_asset_bytes(&mut hasher, 4, unit.id().as_bytes(), cancellation)?;
                try_frame_asset_bytes(&mut hasher, 5, unit.text().as_bytes(), cancellation)?;
            }
        }
    }
    ensure_not_cancelled(
        cancellation,
        JsonlCancellationBoundary::AssetFingerprintChunk,
    )?;
    Ok(hasher.finish())
}

fn try_frame_asset_bytes(
    hasher: &mut Sha256FramedHasher,
    tag: u8,
    bytes: &[u8],
    cancellation: &(impl JsonlCancellation + ?Sized),
) -> Result<(), GenericJsonlError> {
    hasher.try_frame_chunks(tag, bytes, CANCELLATION_CHECK_CHUNK_BYTES, || {
        ensure_not_cancelled(
            cancellation,
            JsonlCancellationBoundary::AssetFingerprintChunk,
        )
    })?;
    Ok(())
}

#[cfg(windows)]
fn frame_path(hasher: &mut Sha256FramedHasher, tag: u8, path: &Path) {
    use std::os::windows::ffi::OsStrExt;

    let units = path
        .as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    hasher.frame(tag, &units);
}

#[cfg(not(windows))]
fn frame_path(hasher: &mut Sha256FramedHasher, tag: u8, path: &Path) {
    hasher.frame(tag, path.as_os_str().as_encoded_bytes());
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::diagnostic::{DiagnosticReport, StateEffect, render_diagnostic_report};
    use crate::i18n::{UiLocale, UiLocalizer};

    use super::*;
    use tempfile::tempdir;

    fn symlink_unavailable(error: &io::Error) -> bool {
        matches!(
            error.kind(),
            io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
        ) || error.raw_os_error() == Some(1314)
    }

    struct CancelAtBoundary {
        cancellation: CooperativeCancellation,
        boundary: JsonlCancellationBoundary,
        trigger_at: usize,
        observed: AtomicUsize,
    }

    impl CancelAtBoundary {
        fn new(boundary: JsonlCancellationBoundary, trigger_at: usize) -> Self {
            assert!(trigger_at > 0);
            Self {
                cancellation: CooperativeCancellation::default(),
                boundary,
                trigger_at,
                observed: AtomicUsize::new(0),
            }
        }

        fn observed(&self) -> usize {
            self.observed.load(Ordering::Acquire)
        }
    }

    impl JsonlCancellation for CancelAtBoundary {
        fn ensure_not_cancelled(
            &self,
            boundary: JsonlCancellationBoundary,
        ) -> Result<(), GenericJsonlError> {
            if boundary == self.boundary {
                let observed = self.observed.fetch_add(1, Ordering::AcqRel) + 1;
                if observed == self.trigger_at {
                    self.cancellation.request();
                }
            }
            if self.cancellation.is_requested() {
                Err(GenericJsonlError::Cancelled)
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn strict_jsonl_accepts_multiline_text_and_rejects_extra_structure() {
        let file = parse_file(
            PathBuf::from("dialogue.jsonl"),
            br#"{"id":"scene","kind":"dialogue","units":[{"id":"line","text":"one\ntwo"}]}
"#
            .to_vec(),
        )
        .expect("固定格式应通过");
        assert_eq!(file.groups()[0].units()[0].text(), "one\ntwo");

        for invalid in [
            br#"{"id":"scene","kind":"dialogue","extra":1,"units":[{"id":"line","text":"x"}]}"#
                .as_slice(),
            br#"{"id":"scene","kind":"dialogue","units":[{"id":"line","text":"x","extra":1}]}"#
                .as_slice(),
            br#"{"id":"scene","id":"other","kind":"dialogue","units":[{"id":"line","text":"x"}]}"#
                .as_slice(),
        ] {
            assert!(matches!(
                parse_file(PathBuf::from("invalid.jsonl"), invalid.to_vec()),
                Err(GenericJsonlError::InvalidJson { .. })
            ));
        }
    }

    #[test]
    fn blank_lines_and_invalid_text_are_rejected() {
        assert!(matches!(
            parse_file(PathBuf::from("blank.jsonl"), b"\n".to_vec()),
            Err(GenericJsonlError::BlankLine { line: 1, .. })
        ));
        assert!(matches!(
            parse_file(
                PathBuf::from("cr.jsonl"),
                br#"{"id":"g","kind":"k","units":[{"id":"u","text":"a\rb"}]}"#.to_vec()
            ),
            Err(GenericJsonlError::InvalidGroup { .. })
        ));
        assert!(matches!(
            parse_file(
                PathBuf::from("nul.jsonl"),
                br#"{"id":"g","kind":"k","units":[{"id":"u","text":"a\u0000b"}]}"#.to_vec()
            ),
            Err(GenericJsonlError::InvalidGroup { .. })
        ));
        assert!(matches!(
            validate_text("before\0middle\rafter"),
            Err(GenericJsonlError::InvalidText {
                violation: GenericTextViolation::Nul
            })
        ));
    }

    #[test]
    fn ids_and_kind_only_reject_empty_strings_without_trimming() {
        let file = parse_file(
            PathBuf::from("whitespace-identities.jsonl"),
            br#"{"id":" ","kind":"\t","units":[{"id":"  ","text":"x"}]}"#.to_vec(),
        )
        .expect("ID 与 kind 按原值解释，纯空白非空字符串合法");
        assert_eq!(file.groups()[0].id(), " ");
        assert_eq!(file.groups()[0].kind(), "\t");
        assert_eq!(file.groups()[0].units()[0].id(), "  ");

        for invalid in [
            br#"{"id":"","kind":"k","units":[{"id":"u","text":"x"}]}"#.as_slice(),
            br#"{"id":"g","kind":"","units":[{"id":"u","text":"x"}]}"#.as_slice(),
            br#"{"id":"g","kind":"k","units":[{"id":"","text":"x"}]}"#.as_slice(),
        ] {
            assert!(matches!(
                parse_file(PathBuf::from("empty-identity.jsonl"), invalid.to_vec()),
                Err(GenericJsonlError::InvalidGroup { .. })
            ));
        }
    }

    #[test]
    fn empty_file_is_valid_and_nonempty_serialization_ends_with_lf() {
        let empty = parse_file(PathBuf::from("empty.jsonl"), Vec::new()).expect("空文件合法");
        assert!(empty.groups().is_empty());
        assert!(serialize_groups(empty.groups()).unwrap().is_empty());

        let output = serialize_groups(&[GenericGroup::new(
            "g".to_owned(),
            "k".to_owned(),
            vec![GenericUnit::new("u".to_owned(), "text".to_owned()).unwrap()],
        )
        .unwrap()])
        .unwrap();
        assert_eq!(output.last(), Some(&b'\n'));
        assert!(!output.windows(2).any(|bytes| bytes == b"\r\n"));
    }

    #[test]
    fn write_back_construction_validates_unit_text_once_and_keeps_group_invariants() {
        let cancellation = CooperativeCancellation::default();
        let source_unit = GenericUnit::new("u".to_owned(), "source".to_owned()).unwrap();
        assert!(matches!(
            source_unit.clone_with_text_with_cancellation("bad\rtext", &cancellation),
            Err(GenericJsonlError::InvalidText { .. })
        ));

        let source_group =
            GenericGroup::new("g".to_owned(), "k".to_owned(), vec![source_unit.clone()]).unwrap();
        let first = source_unit
            .clone_with_text_with_cancellation("translated", &cancellation)
            .unwrap();
        let second = source_unit
            .clone_with_text_with_cancellation("translated", &cancellation)
            .unwrap();
        assert!(matches!(
            source_group.clone_with_units_with_cancellation(vec![first, second], &cancellation),
            Err(GenericJsonlError::DuplicateUnitId { .. })
        ));

        let rewritten = GenericGroup {
            id: "g".to_owned(),
            kind: "k".to_owned(),
            units: vec![GenericUnit {
                id: "u".to_owned(),
                text: "translated".to_owned(),
            }],
        };
        let text_probe = CancelAtBoundary::new(JsonlCancellationBoundary::TextChunk, 1);
        rewritten
            .validate_with_known_valid_units(&text_probe)
            .expect("已校验 Unit 组成 Group 时不应再次扫描正文");
        assert_eq!(text_probe.observed(), 0);
    }

    #[test]
    fn deeply_nested_directories_are_scanned_without_recursive_calls() {
        let temp = tempdir().unwrap();
        let mut directory = temp.path().to_path_buf();
        let mut relative = PathBuf::new();
        for _ in 0..1_200 {
            directory.push("d");
            relative.push("d");
            fs::create_dir(&directory).unwrap();
        }
        fs::write(
            directory.join("deep.jsonl"),
            b"{\"id\":\"g\",\"kind\":\"k\",\"units\":[{\"id\":\"u\",\"text\":\"x\"}]}\n",
        )
        .unwrap();
        relative.push("deep.jsonl");

        let snapshot = scan_input_tree(temp.path()).expect("深目录中的 JSONL 应可正常扫描");

        assert_eq!(snapshot.files().len(), 1);
        assert_eq!(snapshot.files()[0].relative_path(), relative);
    }

    #[test]
    fn scan_rejects_a_reparse_input_root_without_following_it() {
        let temp = tempdir().unwrap();
        let real = temp.path().join("real");
        fs::create_dir(&real).unwrap();
        fs::write(
            real.join("input.jsonl"),
            b"{\"id\":\"g\",\"kind\":\"k\",\"units\":[{\"id\":\"u\",\"text\":\"outside\"}]}\n",
        )
        .unwrap();
        let link = temp.path().join("linked-root");
        if let Err(error) = std::os::windows::fs::symlink_dir(&real, &link) {
            if symlink_unavailable(&error) {
                return;
            }
            panic!("应该可创建目录符号链接：{error}");
        }

        let error = scan_input_tree(&link).expect_err("Generic 输入根不得穿越 reparse point");

        assert!(matches!(
            error,
            GenericJsonlError::Windows {
                source: WindowsFsError::ReparsePoint { path },
                ..
            } if path == link
        ));
    }

    #[test]
    fn scan_rejects_a_reparse_child_without_reading_its_target() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        let outside = temp.path().join("outside.jsonl");
        fs::create_dir(&source).unwrap();
        fs::write(
            &outside,
            b"{\"id\":\"outside\",\"kind\":\"k\",\"units\":[{\"id\":\"u\",\"text\":\"secret\"}]}\n",
        )
        .unwrap();
        let link = source.join("linked.jsonl");
        let expected_link = source
            .canonicalize()
            .expect("应该可规范化无 reparse 的输入根")
            .join("linked.jsonl");
        if let Err(error) = std::os::windows::fs::symlink_file(&outside, &link) {
            if symlink_unavailable(&error) {
                return;
            }
            panic!("应该可创建文件符号链接：{error}");
        }

        let error = scan_input_tree(&source).expect_err("Generic 输入不得读取 reparse 目标");

        match error {
            GenericJsonlError::Windows {
                path,
                source: WindowsFsError::ReparsePoint { path: reparse_path },
                ..
            } => {
                assert_eq!(path, expected_link, "外层错误必须指向被拒绝的目录项");
                assert_eq!(
                    reparse_path, expected_link,
                    "Windows 错误必须指向被拒绝的 reparse point"
                );
            }
            other => panic!("预期 reparse point 错误，实际：{other:?}"),
        }
        assert!(fs::read_to_string(&outside).unwrap().contains("secret"));
    }

    #[test]
    fn scan_rejects_hard_linked_files_before_parsing_them() {
        let temp = tempdir().unwrap();
        let first = temp.path().join("first.jsonl");
        let second = temp.path().join("second.jsonl");
        fs::write(
            &first,
            b"{\"id\":\"g\",\"kind\":\"k\",\"units\":[{\"id\":\"u\",\"text\":\"x\"}]}\n",
        )
        .unwrap();
        fs::hard_link(&first, &second).expect("本地 NTFS 测试目录应该支持硬链接");

        let error = scan_input_tree(temp.path()).expect_err("Generic 输入必须拒绝硬链接");

        assert!(matches!(
            error,
            GenericJsonlError::HardLinkedFile { link_count: 2, .. }
        ));
    }

    #[test]
    fn windows_case_registry_rejects_equivalent_sibling_names() {
        let mut names = BTreeMap::new();
        register_windows_name(
            &mut names,
            OsStr::new("Scene.jsonl"),
            Path::new("Scene.jsonl"),
            Path::new(r"C:\input\Scene.jsonl"),
        )
        .unwrap();

        let error = register_windows_name(
            &mut names,
            OsStr::new("scene.JSONL"),
            Path::new("scene.JSONL"),
            Path::new(r"C:\input\scene.JSONL"),
        )
        .expect_err("Windows 大小写等价名称必须冲突");

        assert!(matches!(
            error,
            GenericJsonlError::WindowsCaseConflict {
                first_path,
                second_path,
            } if first_path == Path::new("Scene.jsonl")
                && second_path == Path::new("scene.JSONL")
        ));
    }

    #[test]
    fn collected_jsonl_file_remains_a_stable_snapshot_until_read_finishes() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("input.jsonl");
        let original =
            b"{\"id\":\"g\",\"kind\":\"k\",\"units\":[{\"id\":\"u\",\"text\":\"original\"}]}\n";
        fs::write(&path, original).unwrap();

        let mut files =
            collect_jsonl_files(temp.path(), &NeverCancelled).expect("安全扫描应固定 JSONL 文件");
        assert_eq!(files.len(), 1);
        assert!(
            fs::write(&path, b"replaced").is_err(),
            "稳定读取句柄存活时不得允许其他写入者改变文件"
        );

        let bytes = read_pinned_jsonl_file_with_probe(&mut files[0], &NeverCancelled)
            .expect("固定文件应可读取");
        assert_eq!(bytes, original);
        drop(files);
        fs::write(&path, b"released").expect("固定句柄释放后应可重新写入");
    }

    #[test]
    fn chunked_scan_preserves_raw_bytes_and_both_fingerprints() {
        let temp = tempdir().unwrap();
        let relative_path = PathBuf::from("large.jsonl");
        let text = "你".repeat(CANCELLATION_CHECK_BYTES);
        let raw_bytes = format!(
            "{{\"id\":\"g\",\"kind\":\"k\",\"units\":[{{\"id\":\"u\",\"text\":\"{text}\"}}]}}\n"
        )
        .into_bytes();
        fs::write(temp.path().join(&relative_path), &raw_bytes).unwrap();

        let snapshot = scan_input_tree(temp.path()).expect("分块扫描应成功");
        assert_eq!(snapshot.files()[0].raw_bytes(), raw_bytes);

        let mut expected_raw = Sha256FramedHasher::new(b"att.generic.raw-input");
        frame_path(&mut expected_raw, 1, &relative_path);
        expected_raw.frame(2, &raw_bytes);
        assert_eq!(snapshot.raw_fingerprint(), expected_raw.finish());

        let mut expected_assets = Sha256FramedHasher::new(b"att.generic.assets");
        frame_path(&mut expected_assets, 1, &relative_path);
        expected_assets.frame(2, b"g");
        expected_assets.frame(3, b"k");
        expected_assets.frame(4, b"u");
        expected_assets.frame(5, text.as_bytes());
        assert_eq!(snapshot.asset_fingerprint(), expected_assets.finish());
    }

    #[test]
    fn cancelled_scan_stops_before_reading_more_input() {
        let temp = tempdir().unwrap();
        let cancellation = CooperativeCancellation::default();
        cancellation.request();

        assert!(matches!(
            scan_input_tree_with_cancellation(temp.path(), &cancellation),
            Err(GenericJsonlError::Cancelled)
        ));
    }

    #[test]
    fn file_read_observes_cancellation_after_the_first_chunk() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("large.jsonl");
        fs::write(&path, vec![b'x'; CANCELLATION_CHECK_BYTES * 3]).unwrap();
        let cancellation = CancelAtBoundary::new(JsonlCancellationBoundary::FileReadChunk, 2);

        assert!(matches!(
            read_jsonl_file_with_probe(&path, &cancellation),
            Err(GenericJsonlError::Cancelled)
        ));
        assert_eq!(
            cancellation.observed(),
            2,
            "第二次检查前已经成功读取第一个分块"
        );
    }

    #[test]
    fn incremental_utf8_validation_preserves_absolute_error_coordinates() {
        let mut valid_across_boundary = vec![b'a'; CANCELLATION_CHECK_BYTES - 1];
        valid_across_boundary.extend_from_slice("你".as_bytes());
        validate_utf8_with_probe(&valid_across_boundary, &NeverCancelled)
            .expect("跨分块的合法 UTF-8 序列应通过");

        for suffix in [vec![0xf0, 0x28, 0x8c, 0x28], vec![0xe4, 0xbd], vec![0x80]] {
            let mut bytes = vec![b'a'; CANCELLATION_CHECK_BYTES - 1];
            bytes.extend_from_slice(&suffix);
            let expected = std::str::from_utf8(&bytes).expect_err("测试输入必须是非法 UTF-8");
            let actual = match validate_utf8_with_probe(&bytes, &NeverCancelled) {
                Err(GenericUtf8ErrorOrCancellation::Invalid(source)) => source,
                result => panic!("应返回 UTF-8 错误，实际为 {result:?}"),
            };
            assert_eq!(actual.valid_up_to(), expected.valid_up_to());
            assert_eq!(actual.error_len(), expected.error_len());
        }
    }

    #[test]
    fn long_json_deserialization_observes_cancellation_after_it_starts() {
        let text = "x".repeat(CANCELLATION_CHECK_BYTES * 3);
        let raw_bytes = format!(
            "{{\"id\":\"g\",\"kind\":\"k\",\"units\":[{{\"id\":\"u\",\"text\":\"{text}\"}}]}}\n"
        )
        .into_bytes();
        let cancellation =
            CancelAtBoundary::new(JsonlCancellationBoundary::JsonDeserializeChunk, 2);

        assert!(matches!(
            parse_file_with_probe(PathBuf::from("long-line.jsonl"), raw_bytes, &cancellation),
            Err(GenericJsonlError::Cancelled)
        ));
        assert_eq!(
            cancellation.observed(),
            2,
            "第二次检查发生前 serde 已经消费第一个分块"
        );
    }

    #[test]
    fn long_utf8_line_and_blank_scans_poll_between_chunks() {
        let bytes = vec![b'a'; CANCELLATION_CHECK_BYTES * 3];
        let utf8_cancellation = CancelAtBoundary::new(JsonlCancellationBoundary::Utf8Chunk, 2);
        assert!(matches!(
            validate_utf8_with_probe(&bytes, &utf8_cancellation),
            Err(GenericUtf8ErrorOrCancellation::Cancellation(
                GenericJsonlError::Cancelled
            ))
        ));

        let line_cancellation = CancelAtBoundary::new(JsonlCancellationBoundary::LineScanChunk, 2);
        assert!(matches!(
            find_physical_line_end(&bytes, 0, &line_cancellation),
            Err(GenericJsonlError::Cancelled)
        ));

        let blank_line = " ".repeat(CANCELLATION_CHECK_BYTES * 3);
        let blank_cancellation =
            CancelAtBoundary::new(JsonlCancellationBoundary::BlankLineChunk, 2);
        assert!(matches!(
            is_blank_line_with_probe(&blank_line, &blank_cancellation),
            Err(GenericJsonlError::Cancelled)
        ));
    }

    #[test]
    fn cancellable_reader_preserves_serde_error_category_and_coordinates() {
        let long_text = "x".repeat(CANCELLATION_CHECK_BYTES * 2);
        let unicode_text = "中文🙂".repeat(CANCELLATION_CHECK_BYTES / 8);
        let cases = [
            br#"{"id":"g","kind":"k","units":[{"id":"u","text":"x"}],"extra":true}"#.to_vec(),
            br#"{"id":"g","kind":"k","units":["#.to_vec(),
            br#"{"id":],"kind":"k","units":[]}"#.to_vec(),
            br#"{}"#.to_vec(),
            br#"null"#.to_vec(),
            br#"{"id":1,"kind":"k","units":[]}"#.to_vec(),
            br#"{"id":"g","kind":"k","units":[{"id":"u"}]}"#.to_vec(),
            format!(
                "{{\"id\":\"g\",\"kind\":\"k\",\"units\":[{{\"id\":\"u\",\"text\":\"{long_text}\"}}],\"extra\":true}}"
            )
            .into_bytes(),
            format!(
                "{{\"id\":\"组🙂\",\"kind\":\"k\",\"units\":[{{\"id\":\"单元\",\"text\":\"{unicode_text}\"}}],\"extra\":true}}"
            )
            .into_bytes(),
        ];
        for json_line in cases {
            let expected =
                serde_json::from_slice::<GenericGroup>(&json_line).expect_err("测试输入必须失败");
            let (actual, line, column) =
                match deserialize_group_with_probe(&json_line, &NeverCancelled) {
                    Err(JsonDeserializeError::Json {
                        source,
                        line,
                        column,
                    }) => (source, line, column),
                    result => panic!("应返回 JSON 格式错误，实际为 {result:?}"),
                };

            assert_eq!(actual.classify(), expected.classify());
            assert_eq!(line, expected.line());
            assert_eq!(column, expected.column());
        }
    }

    #[test]
    fn long_json_serialization_observes_cancellation_after_it_starts() {
        let groups = [GenericGroup {
            id: "g".to_owned(),
            kind: "k".to_owned(),
            units: vec![GenericUnit {
                id: "u".to_owned(),
                text: "x".repeat(CANCELLATION_CHECK_BYTES * 3),
            }],
        }];
        let cancellation = CancelAtBoundary::new(JsonlCancellationBoundary::JsonSerializeChunk, 3);

        assert!(matches!(
            serialize_groups_with_probe(&groups, &cancellation),
            Err(GenericJsonlError::Cancelled)
        ));
        assert_eq!(
            cancellation.observed(),
            3,
            "第三次检查发生前 serde 已经写出第一个分块"
        );
    }

    #[test]
    fn serialization_does_not_repeat_group_validation() {
        let groups = [GenericGroup::new(
            "g".to_owned(),
            "k".to_owned(),
            vec![GenericUnit::new("u".to_owned(), "text".to_owned()).unwrap()],
        )
        .unwrap()];
        let cancellation = CancelAtBoundary::new(JsonlCancellationBoundary::Unit, 1);

        let output = serialize_groups_with_probe(&groups, &cancellation)
            .expect("已经校验的 Group 应直接序列化");

        assert!(!output.is_empty());
        assert_eq!(cancellation.observed(), 0, "序列化不得再次扫描所有 Unit");
    }

    #[test]
    fn cancellation_is_observed_inside_parse_validation_and_fingerprint_loops() {
        let lines = (0..1_000)
            .map(|index| {
                format!(
                    "{{\"id\":\"g{index}\",\"kind\":\"k\",\"units\":[{{\"id\":\"u\",\"text\":\"x\"}}]}}\n"
                )
            })
            .collect::<String>();
        let line_cancellation = CancelAtBoundary::new(JsonlCancellationBoundary::Line, 500);
        assert!(matches!(
            parse_file_with_probe(
                PathBuf::from("many-lines.jsonl"),
                lines.into_bytes(),
                &line_cancellation,
            ),
            Err(GenericJsonlError::Cancelled)
        ));
        assert_eq!(line_cancellation.observed(), 500);

        let units = (0..1_000)
            .map(|index| format!("{{\"id\":\"u{index}\",\"text\":\"x\"}}"))
            .collect::<Vec<_>>()
            .join(",");
        let unit_cancellation = CancelAtBoundary::new(JsonlCancellationBoundary::Unit, 500);
        let unit_result = parse_file_with_probe(
            PathBuf::from("many-units.jsonl"),
            format!("{{\"id\":\"g\",\"kind\":\"k\",\"units\":[{units}]}}\n").into_bytes(),
            &unit_cancellation,
        );
        assert!(
            matches!(unit_result, Err(GenericJsonlError::Cancelled)),
            "result={unit_result:?}, observed={}",
            unit_cancellation.observed()
        );
        assert_eq!(unit_cancellation.observed(), 500);

        let validation_file = GenericFile {
            relative_path: PathBuf::from("many-groups.jsonl"),
            groups: (0..1_000)
                .map(|index| GenericGroup {
                    id: format!("g{index}"),
                    kind: "k".to_owned(),
                    units: vec![GenericUnit {
                        id: "u".to_owned(),
                        text: "x".to_owned(),
                    }],
                })
                .collect(),
            raw_bytes: Vec::new(),
        };
        let validation_cancellation =
            CancelAtBoundary::new(JsonlCancellationBoundary::ProjectGroup, 500);
        assert!(matches!(
            validate_project_group_ids(&[validation_file], &validation_cancellation),
            Err(GenericJsonlError::Cancelled)
        ));
        assert_eq!(validation_cancellation.observed(), 500);

        let raw_file = GenericFile {
            relative_path: PathBuf::from("large.jsonl"),
            groups: Vec::new(),
            raw_bytes: vec![b'x'; 2 * 1024 * 1024],
        };
        let raw_cancellation =
            CancelAtBoundary::new(JsonlCancellationBoundary::RawFingerprintChunk, 10);
        assert!(matches!(
            fingerprint_raw_files(&[raw_file], &raw_cancellation),
            Err(GenericJsonlError::Cancelled)
        ));
        assert_eq!(raw_cancellation.observed(), 10);

        let asset_file = GenericFile {
            relative_path: PathBuf::from("large-asset.jsonl"),
            groups: vec![GenericGroup {
                id: "g".to_owned(),
                kind: "k".to_owned(),
                units: vec![GenericUnit {
                    id: "u".to_owned(),
                    text: "x".repeat(2 * 1024 * 1024),
                }],
            }],
            raw_bytes: Vec::new(),
        };
        let asset_cancellation =
            CancelAtBoundary::new(JsonlCancellationBoundary::AssetFingerprintChunk, 10);
        assert!(matches!(
            fingerprint_assets(&[asset_file], &asset_cancellation),
            Err(GenericJsonlError::Cancelled)
        ));
        assert_eq!(asset_cancellation.observed(), 10);
    }

    #[test]
    fn invalid_json_diagnostic_excludes_unstructured_serde_text_and_input_payload() {
        const SENTINEL: &str = "PRIVATE_JSON_SENTINEL";
        let source = format!(
            "{{\"id\":\"g\",\"kind\":\"k\",\"units\":[{{\"id\":\"u\",\"text\":\"x\"}}],\"{SENTINEL}\":true}}"
        );
        let error = parse_file(PathBuf::from("nested/bad.jsonl"), source.into_bytes())
            .expect_err("未知字段必须失败");
        assert!(
            matches!(&error, GenericJsonlError::InvalidJson { source, .. } if source.to_string().contains(SENTINEL)),
            "测试输入必须确保 serde 原始错误确实携带 sentinel"
        );

        let report = DiagnosticReport::new(
            StateEffect::Unchanged,
            error.diagnostic(GenericDiagnosticStage::Extract),
        );
        let value = serde_json::to_value(&report).expect("公开诊断应可序列化");
        assert_eq!(value["primary"]["code"], "generic.jsonl.invalid_json");
        assert_eq!(value["primary"]["stage"], "extract");
        let problem = &value["primary"]["issue"]["details"]["problem"];
        assert_eq!(problem["location"]["path"], "nested/bad.jsonl");
        assert_eq!(problem["location"]["line"], 1);
        assert_eq!(problem["json_line"], 1);
        assert_eq!(problem["category"], "data");
        assert!(problem["json_column"].as_u64().is_some());
        let serialized = serde_json::to_string(&report).expect("公开诊断应可序列化");
        assert!(!serialized.contains(SENTINEL));

        let rendered = render_diagnostic_report(&report, &UiLocalizer::new(UiLocale::English));
        assert!(rendered.contains("nested/bad.jsonl:line1"));
        for internal in [
            "json_category=",
            "json_column=",
            "generic.jsonl.invalid_json",
        ] {
            assert!(
                !rendered.contains(internal),
                "CLI 不得显示内部诊断字段 {internal:?}：{rendered}"
            );
        }
        assert!(!rendered.contains(SENTINEL));
    }
}
