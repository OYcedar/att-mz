//! Generic JSONL 写回候选的构造与往返验证。

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use super::jsonl::{
    GenericFile, GenericInputSnapshot, GenericJsonlError, parse_file, serialize_groups,
};
use super::project::GenericStoredSnapshot;

/// 候选中的一个 JSONL 文件。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GenericWriteBackFile {
    validated: GenericFile,
}

impl GenericWriteBackFile {
    pub(crate) fn relative_path(&self) -> &Path {
        self.validated.relative_path()
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        self.validated.raw_bytes()
    }
}

/// 已通过生产解析器往返验证、可以交给目录发布能力的完整候选。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GenericWriteBackCandidate {
    files: Vec<GenericWriteBackFile>,
    translated_units: usize,
    retained_source_units: usize,
}

impl GenericWriteBackCandidate {
    pub(crate) fn files(&self) -> &[GenericWriteBackFile] {
        &self.files
    }

    pub(crate) const fn translated_units(&self) -> usize {
        self.translated_units
    }

    pub(crate) const fn retained_source_units(&self) -> usize {
        self.retained_source_units
    }
}

/// 候选无法证明只修改了 Unit text。
#[derive(Debug)]
pub(crate) enum GenericWriteBackError {
    SourceChanged,
    SnapshotMismatch {
        detail: String,
    },
    MaterializedMismatch {
        path: PathBuf,
        bytes_changed: bool,
        structure_changed: bool,
    },
    Jsonl(GenericJsonlError),
}

impl fmt::Display for GenericWriteBackError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceChanged => formatter.write_str("Generic 输入已变化，请先运行 Extract"),
            Self::SnapshotMismatch { detail } => {
                write!(formatter, "Generic 数据库快照与当前输入不一致：{detail}")
            }
            Self::MaterializedMismatch {
                path,
                bytes_changed,
                structure_changed,
            } => write!(
                formatter,
                "暂存 Generic JSONL 与已验证候选不一致：{}（字节变化：{bytes_changed}，结构变化：{structure_changed}）",
                path.display()
            ),
            Self::Jsonl(source) => source.fmt(formatter),
        }
    }
}

impl Error for GenericWriteBackError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Jsonl(source) => Some(source),
            Self::SourceChanged
            | Self::SnapshotMismatch { .. }
            | Self::MaterializedMismatch { .. } => None,
        }
    }
}

impl From<GenericJsonlError> for GenericWriteBackError {
    fn from(source: GenericJsonlError) -> Self {
        Self::Jsonl(source)
    }
}

/// 以当前外部 JSONL 为结构来源，只把数据库中的当前译文写入 `text`。
pub(crate) fn build_write_back_candidate(
    stored: &GenericStoredSnapshot,
    live: &GenericInputSnapshot,
    current_translations: &HashMap<(String, String), String>,
) -> Result<GenericWriteBackCandidate, GenericWriteBackError> {
    if stored.project().extracted_raw_fingerprint() != Some(live.raw_fingerprint()) {
        return Err(GenericWriteBackError::SourceChanged);
    }
    if stored.files().len() != live.files().len() {
        return Err(GenericWriteBackError::SnapshotMismatch {
            detail: "文件数量不同".to_owned(),
        });
    }

    let translations = index_current_translations(current_translations);
    let built_files = stored
        .files()
        .par_iter()
        .zip(live.files().par_iter())
        .map(|(stored_file, live_file)| {
            build_write_back_file(stored_file, live_file, &translations)
        })
        .collect::<Vec<_>>();

    let mut translated_units = 0;
    let mut retained_source_units = 0;
    let mut files = Vec::with_capacity(live.files().len());
    // Rayon 的 indexed collect 保留文件顺序；这里再按自然顺序取出结果，
    // 因而多个文件同时失败时仍返回自然顺序最早的错误。
    for result in built_files {
        let built = result?;
        translated_units += built.translated_units;
        retained_source_units += built.retained_source_units;
        files.push(built.file);
    }

    Ok(GenericWriteBackCandidate {
        files,
        translated_units,
        retained_source_units,
    })
}

struct BuiltWriteBackFile {
    file: GenericWriteBackFile,
    translated_units: usize,
    retained_source_units: usize,
}

type CurrentTranslationIndex<'a> = HashMap<&'a str, HashMap<&'a str, &'a str>>;

fn index_current_translations(
    translations: &HashMap<(String, String), String>,
) -> CurrentTranslationIndex<'_> {
    let mut index: CurrentTranslationIndex<'_> = HashMap::new();
    for ((group_id, unit_id), translation) in translations {
        index
            .entry(group_id.as_str())
            .or_default()
            .insert(unit_id.as_str(), translation.as_str());
    }
    index
}

fn build_write_back_file(
    stored_file: &super::project::GenericStoredFile,
    live_file: &GenericFile,
    translations: &CurrentTranslationIndex<'_>,
) -> Result<BuiltWriteBackFile, GenericWriteBackError> {
    if stored_file.relative_path() != live_file.relative_path() {
        return Err(GenericWriteBackError::SnapshotMismatch {
            detail: format!(
                "文件位置不同：数据库 {}，输入 {}",
                stored_file.relative_path().display(),
                live_file.relative_path().display()
            ),
        });
    }
    if stored_file.groups().len() != live_file.groups().len() {
        return Err(GenericWriteBackError::SnapshotMismatch {
            detail: format!("{} 的 Group 数量不同", live_file.relative_path().display()),
        });
    }

    let mut translated_units = 0;
    let mut retained_source_units = 0;
    let mut output_groups = live_file.groups().to_vec();
    for ((stored_group, live_group), output_group) in stored_file
        .groups()
        .iter()
        .zip(live_file.groups())
        .zip(&mut output_groups)
    {
        validate_group_shape(stored_group, live_group, live_file.relative_path())?;
        let group_translations = translations.get(output_group.id());
        for unit in output_group.units_mut() {
            if let Some(translation) = group_translations
                .and_then(|group_translations| group_translations.get(unit.id()))
                .copied()
            {
                unit.set_text(translation.to_owned())?;
                translated_units += 1;
            } else {
                retained_source_units += 1;
            }
        }
    }

    let bytes = serialize_groups(&output_groups)?;
    let validated = validate_round_trip(live_file, bytes, translations)?;
    Ok(BuiltWriteBackFile {
        file: GenericWriteBackFile { validated },
        translated_units,
        retained_source_units,
    })
}

fn validate_group_shape(
    stored: &super::project::GenericStoredGroup,
    live: &super::jsonl::GenericGroup,
    path: &Path,
) -> Result<(), GenericWriteBackError> {
    if stored.id() != live.id()
        || stored.kind() != live.kind()
        || stored.units().len() != live.units().len()
    {
        return Err(GenericWriteBackError::SnapshotMismatch {
            detail: format!("{} 中的 Group {} 结构不同", path.display(), live.id()),
        });
    }
    for (stored_unit, live_unit) in stored.units().iter().zip(live.units()) {
        if stored_unit.id() != live_unit.id() || stored_unit.source_text() != live_unit.text() {
            return Err(GenericWriteBackError::SnapshotMismatch {
                detail: format!(
                    "{} 中的 Unit {}/{} 结构或原文不同",
                    path.display(),
                    live.id(),
                    live_unit.id()
                ),
            });
        }
    }
    Ok(())
}

fn validate_round_trip(
    source: &GenericFile,
    candidate_bytes: Vec<u8>,
    translations: &CurrentTranslationIndex<'_>,
) -> Result<GenericFile, GenericWriteBackError> {
    let candidate = parse_file(source.relative_path().to_path_buf(), candidate_bytes)?;
    if source.groups().len() != candidate.groups().len() {
        return Err(GenericWriteBackError::SnapshotMismatch {
            detail: format!(
                "{} 的候选往返后 Group 数量变化",
                source.relative_path().display()
            ),
        });
    }
    for (original_group, candidate_group) in source.groups().iter().zip(candidate.groups()) {
        if original_group.id() != candidate_group.id()
            || original_group.kind() != candidate_group.kind()
            || original_group.units().len() != candidate_group.units().len()
        {
            return Err(GenericWriteBackError::SnapshotMismatch {
                detail: format!(
                    "{} 的候选改变了 Group 结构",
                    source.relative_path().display()
                ),
            });
        }
        for (original, candidate) in original_group.units().iter().zip(candidate_group.units()) {
            if original.id() != candidate.id() {
                return Err(GenericWriteBackError::SnapshotMismatch {
                    detail: format!("{} 的候选改变了 Unit ID", source.relative_path().display()),
                });
            }
            let expected = translations
                .get(original_group.id())
                .and_then(|group_translations| group_translations.get(original.id()))
                .copied()
                .unwrap_or(original.text());
            if candidate.text() != expected {
                return Err(GenericWriteBackError::SnapshotMismatch {
                    detail: format!(
                        "{} 的候选 Unit {}/{} text 不符合预期",
                        source.relative_path().display(),
                        original_group.id(),
                        original.id()
                    ),
                });
            }
        }
    }
    Ok(candidate)
}

/// 用生产解析器复查实际落盘内容，并与已经通过候选验证的文件逐字节、逐结构比较。
pub(crate) fn validate_materialized_write_back_file(
    expected: &GenericWriteBackFile,
    materialized_bytes: Vec<u8>,
) -> Result<(), GenericWriteBackError> {
    let materialized = parse_file(expected.relative_path().to_path_buf(), materialized_bytes)?;
    let bytes_changed = materialized.raw_bytes() != expected.bytes();
    let structure_changed = materialized.groups() != expected.validated.groups();
    if bytes_changed || structure_changed {
        return Err(GenericWriteBackError::MaterializedMismatch {
            path: expected.relative_path().to_path_buf(),
            bytes_changed,
            structure_changed,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use crate::fingerprint::Sha256Fingerprint;
    use crate::generic::project::{
        GenericInitRequest, GenericProjectStore, TranslationOrigin, TranslationWrite,
    };
    use crate::language::LanguageId;

    use super::*;

    #[test]
    fn candidate_changes_only_translated_text_and_keeps_empty_files() {
        let temp = tempdir().unwrap();
        let source_root = temp.path().join("source");
        fs::create_dir(&source_root).unwrap();
        fs::write(
            source_root.join("main.jsonl"),
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"a\",\"text\":\"原文\"},{\"id\":\"b\",\"text\":\"保留\"}]}\n",
        )
        .unwrap();
        fs::write(source_root.join("empty.jsonl"), []).unwrap();
        let (store, _) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "game".parse().unwrap(),
            workspace_root: temp.path().join("project"),
            source_root: Some(source_root),
            source_language: Some(LanguageId::parse("ja").unwrap()),
            target_language: Some(LanguageId::parse("zh-Hans").unwrap()),
        })
        .unwrap();
        store.extract().unwrap();
        let snapshot = store.load_snapshot().unwrap();
        let group = snapshot
            .files()
            .iter()
            .flat_map(|file| file.groups())
            .find(|group| group.id() == "g")
            .unwrap();
        let unit = &group.units()[0];
        store
            .commit_translations(
                snapshot.project().extracted_raw_fingerprint().unwrap(),
                &[TranslationWrite {
                    group_id: group.id().to_owned(),
                    unit_id: unit.id().to_owned(),
                    expected_source_text: unit.source_text().to_owned(),
                    expected_group_context: group.context_fingerprint(),
                    translation: "译文\n第二行".to_owned(),
                    origin: TranslationOrigin::Automatic,
                    state_fingerprint: Sha256Fingerprint::from_bytes([9; 32]),
                    expected_translation: None,
                }],
            )
            .unwrap();

        let (stored, live) = store.ensure_input_current().unwrap();
        let current_translations =
            HashMap::from([(("g".to_owned(), "a".to_owned()), "译文\n第二行".to_owned())]);
        let candidate = build_write_back_candidate(&stored, &live, &current_translations).unwrap();
        assert_eq!(candidate.translated_units(), 1);
        assert_eq!(candidate.retained_source_units(), 1);
        assert_eq!(candidate.files().len(), 2);
        assert_eq!(
            candidate
                .files()
                .iter()
                .map(|file| file.relative_path())
                .collect::<Vec<_>>(),
            [Path::new("empty.jsonl"), Path::new("main.jsonl")]
        );
        let main = candidate
            .files()
            .iter()
            .find(|file| file.relative_path() == Path::new("main.jsonl"))
            .unwrap();
        assert_eq!(
            std::str::from_utf8(main.bytes()).unwrap(),
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"a\",\"text\":\"译文\\n第二行\"},{\"id\":\"b\",\"text\":\"保留\"}]}\n"
        );
        assert!(
            candidate
                .files()
                .iter()
                .find(|file| file.relative_path() == Path::new("empty.jsonl"))
                .unwrap()
                .bytes()
                .is_empty()
        );
    }

    #[test]
    fn materialized_file_validation_uses_production_parser_and_keeps_empty_file_valid() {
        let expected = GenericWriteBackFile {
            validated: parse_file(
                PathBuf::from("scene.jsonl"),
                concat!(
                    r#"{"id":"g","kind":"dialogue","units":[{"id":"u","text":"译文"}]}"#,
                    "\n"
                )
                .as_bytes()
                .to_vec(),
            )
            .unwrap(),
        };
        validate_materialized_write_back_file(&expected, expected.bytes().to_vec()).unwrap();

        let mut byte_changed = expected.bytes().to_vec();
        byte_changed.insert(1, b' ');
        assert!(matches!(
            validate_materialized_write_back_file(&expected, byte_changed),
            Err(GenericWriteBackError::MaterializedMismatch {
                bytes_changed: true,
                structure_changed: false,
                ..
            })
        ));
        assert!(matches!(
            validate_materialized_write_back_file(
                &expected,
                concat!(
                    r#"{"id":"g","kind":"dialogue","units":[{"id":"u","text":"被改写"}]}"#,
                    "\n"
                )
                .as_bytes()
                .to_vec()
            ),
            Err(GenericWriteBackError::MaterializedMismatch {
                bytes_changed: true,
                structure_changed: true,
                ..
            })
        ));
        assert!(matches!(
            validate_materialized_write_back_file(&expected, b"not-json\n".to_vec()),
            Err(GenericWriteBackError::Jsonl(_))
        ));

        let empty = GenericWriteBackFile {
            validated: parse_file(PathBuf::from("empty.jsonl"), Vec::new()).unwrap(),
        };
        validate_materialized_write_back_file(&empty, Vec::new()).unwrap();
    }
}
