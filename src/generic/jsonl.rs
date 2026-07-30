//! Generic JSONL 的严格外部格式与动态输入扫描。

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};

/// JSONL 中的一个可翻译单元。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GenericUnit {
    id: String,
    text: String,
}

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

    pub(crate) fn set_text(&mut self, text: String) -> Result<(), GenericJsonlError> {
        validate_text(&text)?;
        self.text = text;
        Ok(())
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

    pub(crate) fn units_mut(&mut self) -> &mut [GenericUnit] {
        &mut self.units
    }

    fn validate(&self) -> Result<(), GenericJsonlError> {
        validate_nonempty("group.id", &self.id)?;
        validate_nonempty("group.kind", &self.kind)?;
        if self.units.is_empty() {
            return Err(GenericJsonlError::EmptyUnits {
                group_id: self.id.clone(),
            });
        }

        let mut unit_ids = HashMap::with_capacity(self.units.len());
        for (ordinal, unit) in self.units.iter().enumerate() {
            validate_nonempty("unit.id", unit.id())?;
            validate_text(unit.text())?;
            if let Some(previous) = unit_ids.insert(unit.id(), ordinal) {
                return Err(GenericJsonlError::DuplicateUnitId {
                    group_id: self.id.clone(),
                    unit_id: unit.id().to_owned(),
                    first_ordinal: previous,
                    second_ordinal: ordinal,
                });
            }
        }
        Ok(())
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
    SourceNotDirectory {
        path: PathBuf,
    },
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    InvalidUtf8 {
        path: PathBuf,
        source: std::str::Utf8Error,
    },
    BlankLine {
        path: PathBuf,
        line: usize,
    },
    InvalidJson {
        path: PathBuf,
        line: usize,
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
        character: &'static str,
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

impl fmt::Display for GenericJsonlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceNotDirectory { path } => {
                write!(formatter, "Generic 输入根不是现存目录：{}", path.display())
            }
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "{operation} Generic 输入失败：{}（{source}）",
                path.display()
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
            Self::InvalidJson { path, line, source } => write!(
                formatter,
                "Generic JSONL 行不符合固定格式：{}:{line}（{source}）",
                path.display()
            ),
            Self::InvalidGroup { path, line, source } => write!(
                formatter,
                "Generic JSONL Group 无效：{}:{line}（{source}）",
                path.display()
            ),
            Self::BlankField { field } => write!(formatter, "{field} 不能为空"),
            Self::InvalidText { character } => {
                write!(formatter, "unit.text 不允许包含 {character}")
            }
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
            Self::InvalidUtf8 { source, .. } => Some(source),
            Self::InvalidJson { source, .. } | Self::Serialize { source } => Some(source),
            Self::InvalidGroup { source, .. } => Some(source.as_ref()),
            Self::SourceNotDirectory { .. }
            | Self::BlankLine { .. }
            | Self::BlankField { .. }
            | Self::InvalidText { .. }
            | Self::EmptyUnits { .. }
            | Self::DuplicateUnitId { .. }
            | Self::DuplicateGroupId { .. } => None,
        }
    }
}

/// 递归并发读取输入根中的普通 `.jsonl` 文件。
pub(crate) fn scan_input_tree(
    source_root: &Path,
) -> Result<GenericInputSnapshot, GenericJsonlError> {
    if !source_root.is_dir() {
        return Err(GenericJsonlError::SourceNotDirectory {
            path: source_root.to_path_buf(),
        });
    }

    let mut paths = collect_jsonl_paths(source_root, source_root)?;
    paths.sort();

    let parsed = paths
        .par_iter()
        .map(|relative_path| {
            let absolute_path = source_root.join(relative_path);
            let raw_bytes = fs::read(&absolute_path).map_err(|source| GenericJsonlError::Io {
                operation: "读取",
                path: absolute_path,
                source,
            })?;
            parse_file(relative_path.clone(), raw_bytes)
        })
        .collect::<Vec<_>>();

    let mut files = Vec::with_capacity(parsed.len());
    for result in parsed {
        files.push(result?);
    }
    validate_project_group_ids(&files)?;

    let raw_fingerprint = fingerprint_raw_files(&files);
    let asset_fingerprint = fingerprint_assets(&files);
    Ok(GenericInputSnapshot {
        files,
        raw_fingerprint,
        asset_fingerprint,
    })
}

pub(crate) fn parse_file(
    relative_path: PathBuf,
    raw_bytes: Vec<u8>,
) -> Result<GenericFile, GenericJsonlError> {
    let text =
        std::str::from_utf8(&raw_bytes).map_err(|source| GenericJsonlError::InvalidUtf8 {
            path: relative_path.clone(),
            source,
        })?;
    let mut groups = Vec::new();
    let mut start = 0;
    let mut line = 1;
    while start < text.len() {
        let remainder = &text[start..];
        let (raw_line, consumed) = remainder
            .find('\n')
            .map_or((remainder, remainder.len()), |end| {
                (&remainder[..end], end + 1)
            });
        let json_line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if json_line.trim().is_empty() {
            return Err(GenericJsonlError::BlankLine {
                path: relative_path,
                line,
            });
        }
        let group: GenericGroup =
            serde_json::from_str(json_line).map_err(|source| GenericJsonlError::InvalidJson {
                path: relative_path.clone(),
                line,
                source,
            })?;
        group
            .validate()
            .map_err(|source| GenericJsonlError::InvalidGroup {
                path: relative_path.clone(),
                line,
                source: Box::new(source),
            })?;
        groups.push(group);
        start += consumed;
        line += 1;
    }
    Ok(GenericFile {
        relative_path,
        groups,
        raw_bytes,
    })
}

pub(crate) fn serialize_groups(groups: &[GenericGroup]) -> Result<Vec<u8>, GenericJsonlError> {
    let mut output = Vec::new();
    for group in groups {
        group.validate()?;
        serde_json::to_writer(&mut output, group)
            .map_err(|source| GenericJsonlError::Serialize { source })?;
        output.push(b'\n');
    }
    Ok(output)
}

fn validate_nonempty(field: &'static str, value: &str) -> Result<(), GenericJsonlError> {
    if value.is_empty() {
        Err(GenericJsonlError::BlankField { field })
    } else {
        Ok(())
    }
}

fn validate_text(text: &str) -> Result<(), GenericJsonlError> {
    if text.contains('\r') {
        return Err(GenericJsonlError::InvalidText {
            character: "CR（U+000D）",
        });
    }
    if text.contains('\0') {
        return Err(GenericJsonlError::InvalidText {
            character: "NUL（U+0000）",
        });
    }
    Ok(())
}

fn collect_jsonl_paths(
    source_root: &Path,
    directory: &Path,
) -> Result<Vec<PathBuf>, GenericJsonlError> {
    let entries = fs::read_dir(directory).map_err(|source| GenericJsonlError::Io {
        operation: "列举",
        path: directory.to_path_buf(),
        source,
    })?;
    let mut child_directories = Vec::new();
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| GenericJsonlError::Io {
            operation: "读取目录项",
            path: directory.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| GenericJsonlError::Io {
            operation: "读取目录项类型",
            path: path.clone(),
            source,
        })?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            child_directories.push(path);
        } else if file_type.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        {
            let relative = path
                .strip_prefix(source_root)
                .expect("递归目录项必须位于输入根内")
                .to_path_buf();
            paths.push(relative);
        }
    }
    child_directories.sort();
    let nested = child_directories
        .par_iter()
        .map(|child| collect_jsonl_paths(source_root, child))
        .collect::<Vec<_>>();
    for result in nested {
        paths.extend(result?);
    }
    Ok(paths)
}

fn validate_project_group_ids(files: &[GenericFile]) -> Result<(), GenericJsonlError> {
    let mut group_ids: HashMap<&str, (&Path, usize)> = HashMap::new();
    for file in files {
        for (ordinal, group) in file.groups.iter().enumerate() {
            let line = ordinal + 1;
            if let Some((first_path, first_line)) =
                group_ids.insert(group.id(), (file.relative_path(), line))
            {
                return Err(GenericJsonlError::DuplicateGroupId {
                    group_id: group.id().to_owned(),
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

fn fingerprint_raw_files(files: &[GenericFile]) -> Sha256Fingerprint {
    let mut hasher = Sha256FramedHasher::new(b"att.generic.raw-input");
    for file in files {
        frame_path(&mut hasher, 1, file.relative_path());
        hasher.frame(2, file.raw_bytes());
    }
    hasher.finish()
}

fn fingerprint_assets(files: &[GenericFile]) -> Sha256Fingerprint {
    let mut hasher = Sha256FramedHasher::new(b"att.generic.assets");
    for file in files {
        frame_path(&mut hasher, 1, file.relative_path());
        for group in &file.groups {
            hasher.frame(2, group.id().as_bytes());
            hasher.frame(3, group.kind().as_bytes());
            for unit in group.units() {
                hasher.frame(4, unit.id().as_bytes());
                hasher.frame(5, unit.text().as_bytes());
            }
        }
    }
    hasher.finish()
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
    use super::*;

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
}
