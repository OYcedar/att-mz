//! Generic 项目的专用 SQLite 状态与重复 Extract。

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};
use uuid::Uuid;

use crate::fingerprint::{SHA256_FINGERPRINT_BYTES, Sha256Fingerprint, Sha256FramedHasher};
use crate::language::{LanguageId, LanguageIdError, LanguagePair};
use crate::project_name::ProjectName;
use crate::runtime::sqlite::{
    apply_att_sqlite_new_database_page_policy, apply_att_sqlite_read_write_policy,
};

use super::jsonl::{GenericInputSnapshot, GenericJsonlError, scan_input_tree};
use super::placeholder::{
    GenericPlaceholderService, placeholder_binding_fingerprint,
    validate_manual_translation_placeholders,
};
use super::translate::{GenericUnitKey, manual_translation_state_fingerprint};

const DATABASE_FILE_NAME: &str = "project.db";
const TERMINOLOGY_RESOURCE: &str = "terminology";
const PLACEHOLDER_RULES_RESOURCE: &str = "placeholder_rules";
const CREATE_PENDING_TRANSLATION_COMMIT_SQL: &str = "
    CREATE TEMP TABLE pending_translation_commit (
        group_id TEXT NOT NULL,
        unit_id TEXT NOT NULL,
        expected_source_text TEXT NOT NULL,
        expected_group_context BLOB NOT NULL,
        translation TEXT NOT NULL,
        translation_origin TEXT NOT NULL,
        translation_state BLOB NOT NULL,
        expected_translation TEXT,
        expected_translation_origin TEXT,
        expected_translation_state BLOB,
        PRIMARY KEY (group_id, unit_id)
    ) STRICT, WITHOUT ROWID
";
const INSERT_PENDING_TRANSLATION_COMMIT_SQL: &str = "
    INSERT INTO temp.pending_translation_commit (
        group_id,
        unit_id,
        expected_source_text,
        expected_group_context,
        translation,
        translation_origin,
        translation_state,
        expected_translation,
        expected_translation_origin,
        expected_translation_state
    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
";
const APPLY_PENDING_TRANSLATION_COMMIT_SQL: &str = "
    UPDATE main.generic_unit AS unit
    SET translation = pending.translation,
        translation_origin = pending.translation_origin,
        translation_state = pending.translation_state
    FROM temp.pending_translation_commit AS pending
    JOIN main.generic_group AS group_record
      ON group_record.group_id = pending.group_id
    WHERE unit.group_id = pending.group_id
      AND unit.unit_id = pending.unit_id
      AND unit.source_text = pending.expected_source_text
      AND group_record.context_fingerprint = pending.expected_group_context
      AND (
          (
              pending.expected_translation IS NULL
              AND unit.translation IS NULL
              AND unit.translation_origin IS NULL
              AND unit.translation_state IS NULL
          )
          OR
          (
              unit.translation = pending.expected_translation
              AND unit.translation_origin = pending.expected_translation_origin
              AND unit.translation_state = pending.expected_translation_state
          )
      )
    RETURNING group_id, unit_id
";
const LOAD_UNITS_NATURAL_SQL: &str = "
    SELECT u.group_id, u.unit_id, u.ordinal, u.source_text,
           u.translation, u.translation_origin, u.translation_state
    FROM main.generic_file AS f
    CROSS JOIN main.generic_group AS g
    CROSS JOIN main.generic_unit AS u
    WHERE g.relative_path = f.relative_path
      AND u.group_id = g.group_id
    ORDER BY f.ordinal, g.ordinal, u.ordinal
";

/// Generic Init 的全部领域输入。
#[derive(Clone, Debug)]
pub(crate) struct GenericInitRequest {
    pub(crate) project_name: ProjectName,
    pub(crate) workspace_root: PathBuf,
    pub(crate) source_root: Option<PathBuf>,
    pub(crate) source_language: Option<LanguageId>,
    pub(crate) target_language: Option<LanguageId>,
}

/// Generic 项目最近一次明确选择并通过解析的翻译资源。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationResources {
    terminology_json: String,
    placeholder_rules_json: String,
}

impl TranslationResources {
    pub(crate) fn terminology_json(&self) -> &str {
        &self.terminology_json
    }

    pub(crate) fn placeholder_rules_json(&self) -> &str {
        &self.placeholder_rules_json
    }
}

/// 已开启的 Generic 项目事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GenericProject {
    pub(super) project_name: ProjectName,
    pub(super) workspace_root: PathBuf,
    pub(super) database_path: PathBuf,
    pub(super) source_root: PathBuf,
    pub(super) language_pair: LanguagePair,
    pub(super) extracted_raw_fingerprint: Option<Sha256Fingerprint>,
    pub(super) extracted_asset_fingerprint: Option<Sha256Fingerprint>,
    pub(super) last_profile_id: Option<String>,
}

impl GenericProject {
    pub(crate) fn project_name(&self) -> &ProjectName {
        &self.project_name
    }

    pub(crate) fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub(crate) fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub(crate) fn source_root(&self) -> &Path {
        &self.source_root
    }

    pub(crate) fn write_back_root(&self) -> PathBuf {
        self.workspace_root.join("write_back")
    }

    pub(crate) fn language_pair(&self) -> &LanguagePair {
        &self.language_pair
    }

    pub(crate) fn extracted_raw_fingerprint(&self) -> Option<Sha256Fingerprint> {
        self.extracted_raw_fingerprint
    }

    pub(crate) fn extracted_asset_fingerprint(&self) -> Option<Sha256Fingerprint> {
        self.extracted_asset_fingerprint
    }

    pub(crate) fn last_profile_id(&self) -> Option<&str> {
        self.last_profile_id.as_deref()
    }
}

/// 持久化中的一个译文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GenericStoredTranslation {
    pub(super) translation: String,
    pub(super) origin: TranslationOrigin,
    pub(super) state_fingerprint: Sha256Fingerprint,
}

impl GenericStoredTranslation {
    pub(crate) fn translation(&self) -> &str {
        &self.translation
    }

    pub(crate) const fn origin(&self) -> TranslationOrigin {
        self.origin
    }

    pub(crate) const fn state_fingerprint(&self) -> Sha256Fingerprint {
        self.state_fingerprint
    }
}

/// 持久化中的一个 Unit。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GenericStoredUnit {
    pub(super) id: String,
    pub(super) ordinal: usize,
    pub(super) source_text: String,
    pub(super) translation: Option<GenericStoredTranslation>,
}

impl GenericStoredUnit {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn source_text(&self) -> &str {
        &self.source_text
    }

    pub(crate) fn translation(&self) -> Option<&GenericStoredTranslation> {
        self.translation.as_ref()
    }
}

/// 持久化中的一个 Group。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GenericStoredGroup {
    pub(super) id: String,
    pub(super) ordinal: usize,
    pub(super) kind: String,
    pub(super) context_fingerprint: Sha256Fingerprint,
    pub(super) units: Vec<GenericStoredUnit>,
}

impl GenericStoredGroup {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn kind(&self) -> &str {
        &self.kind
    }

    pub(crate) const fn context_fingerprint(&self) -> Sha256Fingerprint {
        self.context_fingerprint
    }

    pub(crate) fn units(&self) -> &[GenericStoredUnit] {
        &self.units
    }
}

/// 一次 Extract 后可供 Translate 与 WriteBack 使用的数据库快照。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GenericStoredSnapshot {
    pub(super) project: GenericProject,
    pub(super) files: Vec<GenericStoredFile>,
}

impl GenericStoredSnapshot {
    pub(crate) fn project(&self) -> &GenericProject {
        &self.project
    }

    pub(crate) fn files(&self) -> &[GenericStoredFile] {
        &self.files
    }

    pub(crate) fn unit_count(&self) -> usize {
        self.files
            .iter()
            .flat_map(|file| &file.groups)
            .map(|group| group.units.len())
            .sum()
    }
}

/// 数据库中一个 JSONL 文件的位置及内容。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GenericStoredFile {
    pub(super) relative_path: PathBuf,
    pub(super) ordinal: usize,
    pub(super) groups: Vec<GenericStoredGroup>,
}

impl GenericStoredFile {
    pub(crate) fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    pub(crate) fn groups(&self) -> &[GenericStoredGroup] {
        &self.groups
    }
}

/// 译文如何进入当前 Unit。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TranslationOrigin {
    Automatic,
    Manual,
}

impl TranslationOrigin {
    const fn storage_name(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::Manual => "manual",
        }
    }

    fn parse(value: &str) -> Result<Self, GenericProjectError> {
        match value {
            "automatic" => Ok(Self::Automatic),
            "manual" => Ok(Self::Manual),
            _ => Err(GenericProjectError::InvalidDatabase {
                detail: format!("未知 translation_origin：{value:?}"),
            }),
        }
    }
}

/// 一个经过上游验收、准备原子提交的 Unit 译文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationWrite {
    pub(crate) group_id: String,
    pub(crate) unit_id: String,
    pub(crate) expected_source_text: String,
    pub(crate) expected_group_context: Sha256Fingerprint,
    pub(crate) translation: String,
    pub(crate) origin: TranslationOrigin,
    pub(crate) state_fingerprint: Sha256Fingerprint,
    pub(crate) expected_translation: Option<GenericStoredTranslation>,
}

/// 一条已经由当前 Translate 语义确认失效、准备以 CAS 清除的旧译文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationClear {
    pub(crate) group_id: String,
    pub(crate) unit_id: String,
    pub(crate) expected_source_text: String,
    pub(crate) expected_group_context: Sha256Fingerprint,
    pub(crate) expected_translation: GenericStoredTranslation,
}

/// Extract 对已有译文造成的可观察结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtractOutcome {
    Unchanged {
        files: usize,
        groups: usize,
        units: usize,
    },
    Updated {
        files: usize,
        groups: usize,
        units: usize,
        preserved_translations: usize,
        cleared_translations: usize,
    },
}

/// 一批译文提交的结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommitTranslationsOutcome {
    pub(crate) committed: usize,
    pub(crate) conflicts: Vec<(String, String)>,
}

/// Generic 项目数据库的直接领域入口。
#[derive(Clone, Debug)]
pub(crate) struct GenericProjectStore {
    workspace_root: PathBuf,
    database_path: PathBuf,
}

impl GenericProjectStore {
    pub(crate) fn for_workspace(workspace_root: PathBuf) -> Self {
        let database_path = workspace_root.join(DATABASE_FILE_NAME);
        Self {
            workspace_root,
            database_path,
        }
    }

    pub(crate) fn initialize(
        request: GenericInitRequest,
    ) -> Result<(Self, GenericProject), GenericProjectError> {
        if request.workspace_root.exists() && !request.workspace_root.is_dir() {
            return Err(GenericProjectError::WorkspaceNotDirectory {
                path: request.workspace_root,
            });
        }
        let store = Self::for_workspace(request.workspace_root.clone());
        let exists = store.database_path.is_file();

        if exists {
            let mut connection = store.open_connection(false)?;
            validate_schema(&connection)?;
            let current = store.read_project_with_connection(&connection)?;
            if current.project_name != request.project_name {
                return Err(GenericProjectError::ProjectIdentityMismatch {
                    expected: current.project_name.to_string(),
                    observed: request.project_name.to_string(),
                });
            }

            let source_root = resolve_source_root(
                request
                    .source_root
                    .as_deref()
                    .unwrap_or(&current.source_root),
            )?;
            validate_source_write_back_separation(&source_root, &request.workspace_root)?;
            let source_language = request
                .source_language
                .unwrap_or_else(|| current.language_pair.source().clone());
            let target_language = request
                .target_language
                .unwrap_or_else(|| current.language_pair.target().clone());
            validate_distinct_languages(&source_language, &target_language)?;
            let source_changed = source_root != current.source_root;
            let language_changed = source_language != *current.language_pair.source()
                || target_language != *current.language_pair.target();

            let transaction =
                connection
                    .transaction()
                    .map_err(|source| GenericProjectError::Sqlite {
                        operation: "开始 Generic Init 事务",
                        source,
                    })?;
            transaction
                .execute(
                    "UPDATE generic_project
                     SET source_root = ?1, source_language = ?2, target_language = ?3
                     WHERE singleton = 1",
                    params![
                        encode_path(&source_root),
                        source_language.as_str(),
                        target_language.as_str()
                    ],
                )
                .map_err(|source| GenericProjectError::Sqlite {
                    operation: "更新 Generic 项目事实",
                    source,
                })?;
            if source_changed {
                clear_extracted_assets(&transaction)?;
            } else if language_changed {
                clear_all_translations(&transaction)?;
            }
            transaction
                .commit()
                .map_err(|source| GenericProjectError::Sqlite {
                    operation: "提交 Generic Init",
                    source,
                })?;
        } else {
            let source_root = request
                .source_root
                .as_deref()
                .ok_or(GenericProjectError::MissingInitialField("path"))
                .and_then(resolve_source_root)?;
            let source_language = request
                .source_language
                .ok_or(GenericProjectError::MissingInitialField("source-language"))?;
            let target_language = request
                .target_language
                .ok_or(GenericProjectError::MissingInitialField("target-language"))?;
            validate_distinct_languages(&source_language, &target_language)?;
            validate_source_write_back_separation(&source_root, &request.workspace_root)?;
            fs::create_dir_all(&request.workspace_root).map_err(|source| {
                GenericProjectError::Io {
                    operation: "建立 Generic 项目目录",
                    path: request.workspace_root.clone(),
                    source,
                }
            })?;
            store.create_initial_database(
                &request.project_name,
                &source_root,
                &source_language,
                &target_language,
            )?;
        }

        let project = store.open()?;
        Ok((store, project))
    }

    fn create_initial_database(
        &self,
        project_name: &ProjectName,
        source_root: &Path,
        source_language: &LanguageId,
        target_language: &LanguageId,
    ) -> Result<(), GenericProjectError> {
        let candidate_path = self
            .workspace_root
            .join(format!(".{DATABASE_FILE_NAME}.init-{}.tmp", Uuid::new_v4()));
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate_path)
            .map_err(|source| GenericProjectError::Io {
                operation: "建立 Generic 初始数据库候选",
                path: candidate_path.clone(),
                source,
            })?;

        let build_result = (|| {
            let mut connection = open_sqlite_connection(&candidate_path, true)?;
            create_initial_schema(
                &mut connection,
                project_name,
                source_root,
                source_language,
                target_language,
            )?;
            validate_schema(&connection)?;
            drop(connection);
            fs::rename(&candidate_path, &self.database_path).map_err(|source| {
                GenericProjectError::Io {
                    operation: "发布 Generic 初始数据库",
                    path: self.database_path.clone(),
                    source,
                }
            })
        })();

        match build_result {
            Ok(()) => Ok(()),
            Err(original) => match cleanup_initial_database_candidate(&candidate_path) {
                Ok(()) => Err(original),
                Err((path, cleanup)) => Err(GenericProjectError::InitialCandidateCleanup {
                    original: Box::new(original),
                    path,
                    cleanup,
                }),
            },
        }
    }

    pub(crate) fn open(&self) -> Result<GenericProject, GenericProjectError> {
        let connection = self.open_connection(false)?;
        validate_schema(&connection)?;
        self.read_project_with_connection(&connection)
    }

    pub(crate) fn extract(&self) -> Result<ExtractOutcome, GenericProjectError> {
        let project = self.open()?;
        let scanned = scan_input_tree(project.source_root())?;
        if project.extracted_raw_fingerprint() == Some(scanned.raw_fingerprint())
            && project.extracted_asset_fingerprint() == Some(scanned.asset_fingerprint())
        {
            let observed_again = scan_input_tree(project.source_root())?;
            if observed_again.raw_fingerprint() != scanned.raw_fingerprint() {
                return Err(GenericProjectError::InputChangedDuringExtract);
            }
            return Ok(ExtractOutcome::Unchanged {
                files: scanned.files().len(),
                groups: scanned.group_count(),
                units: scanned.unit_count(),
            });
        }

        let mut connection = self.open_connection(false)?;
        let transaction =
            connection
                .transaction()
                .map_err(|source| GenericProjectError::Sqlite {
                    operation: "开始 Generic Extract 事务",
                    source,
                })?;
        let previous = load_snapshot_rows(&transaction, &project)?;
        let reconciled = reconcile_snapshot(&previous, &scanned);
        replace_snapshot(&transaction, &scanned, &reconciled.files)?;

        let observed_again = scan_input_tree(project.source_root())?;
        if observed_again.raw_fingerprint() != scanned.raw_fingerprint() {
            return Err(GenericProjectError::InputChangedDuringExtract);
        }
        transaction
            .commit()
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "提交 Generic Extract",
                source,
            })?;

        Ok(ExtractOutcome::Updated {
            files: scanned.files().len(),
            groups: scanned.group_count(),
            units: scanned.unit_count(),
            preserved_translations: reconciled.preserved_translations,
            cleared_translations: reconciled.cleared_translations,
        })
    }

    #[cfg(test)]
    pub(crate) fn load_snapshot(&self) -> Result<GenericStoredSnapshot, GenericProjectError> {
        let connection = self.open_connection(false)?;
        validate_schema(&connection)?;
        let project = self.read_project_with_connection(&connection)?;
        if project.extracted_raw_fingerprint().is_none() {
            return Err(GenericProjectError::ExtractRequired);
        }
        load_snapshot_rows(&connection, &project)
    }

    /// 重新扫描外部输入，并只在内容仍与最近一次 Extract 相同时返回快照。
    #[cfg(test)]
    pub(crate) fn ensure_input_current(
        &self,
    ) -> Result<(GenericStoredSnapshot, GenericInputSnapshot), GenericProjectError> {
        let stored = self.load_snapshot()?;
        let live = scan_current_input(&stored)?;
        Ok((stored, live))
    }

    /// 为 Translate 或 WriteBack 在同一连接上检查数据库并读取项目、资产和翻译资源。
    ///
    /// 完整数据库检查只执行一次；外部输入仍在数据库读取完成后独立扫描并与两个 Extract
    /// 指纹比较。调用方随后在同一项目排他租约内复用返回的内存快照建立候选。
    pub(crate) fn load_current_translation_state(
        &self,
    ) -> Result<
        (
            GenericStoredSnapshot,
            GenericInputSnapshot,
            TranslationResources,
        ),
        GenericProjectError,
    > {
        let connection = self.open_connection(false)?;
        validate_schema(&connection)?;
        let project = self.read_project_with_connection(&connection)?;
        if project.extracted_raw_fingerprint().is_none() {
            return Err(GenericProjectError::ExtractRequired);
        }
        let stored = load_snapshot_rows(&connection, &project)?;
        let resources = load_translation_resources_rows(&connection)?;
        drop(connection);
        let live = scan_current_input(&stored)?;
        Ok((stored, live, resources))
    }

    #[cfg(test)]
    pub(crate) fn commit_translations(
        &self,
        expected_raw_fingerprint: Sha256Fingerprint,
        writes: &[TranslationWrite],
    ) -> Result<CommitTranslationsOutcome, GenericProjectError> {
        self.commit_translations_inner(expected_raw_fingerprint, writes, None)
    }

    /// 提交自动翻译进展，并在至少一项写入成功时于同一事务记录所用 Profile。
    pub(crate) fn commit_translations_for_profile(
        &self,
        expected_raw_fingerprint: Sha256Fingerprint,
        writes: &[TranslationWrite],
        profile_id: &str,
    ) -> Result<CommitTranslationsOutcome, GenericProjectError> {
        if profile_id.is_empty() || profile_id.chars().all(char::is_whitespace) {
            return Err(GenericProjectError::BlankProfileId);
        }
        self.commit_translations_inner(expected_raw_fingerprint, writes, Some(profile_id))
    }

    fn commit_translations_inner(
        &self,
        expected_raw_fingerprint: Sha256Fingerprint,
        writes: &[TranslationWrite],
        profile_id: Option<&str>,
    ) -> Result<CommitTranslationsOutcome, GenericProjectError> {
        let mut write_indexes = HashMap::with_capacity(writes.len());
        for (index, write) in writes.iter().enumerate() {
            validate_translation(&write.translation)?;
            if write_indexes
                .insert((write.group_id.as_str(), write.unit_id.as_str()), index)
                .is_some()
            {
                return Err(GenericProjectError::DuplicateTranslationWrite {
                    group_id: write.group_id.clone(),
                    unit_id: write.unit_id.clone(),
                });
            }
        }

        let mut connection = self.open_connection(false)?;
        let transaction =
            connection
                .transaction()
                .map_err(|source| GenericProjectError::Sqlite {
                    operation: "开始 Generic 译文提交事务",
                    source,
                })?;
        let actual = read_optional_fingerprint(
            transaction
                .query_row(
                    "SELECT extracted_raw_fingerprint
                     FROM generic_project WHERE singleton = 1",
                    [],
                    |row| row.get(0),
                )
                .map_err(|source| GenericProjectError::Sqlite {
                    operation: "读取 Generic Extract 指纹",
                    source,
                })?,
            "extracted_raw_fingerprint",
        )?;
        if actual != Some(expected_raw_fingerprint) {
            return Err(GenericProjectError::TranslationSnapshotChanged);
        }

        let mut applied = vec![false; writes.len()];
        {
            transaction
                .execute(CREATE_PENDING_TRANSLATION_COMMIT_SQL, [])
                .map_err(|source| GenericProjectError::Sqlite {
                    operation: "建立 Generic 译文提交暂存表",
                    source,
                })?;
            let mut insert = transaction
                .prepare_cached(INSERT_PENDING_TRANSLATION_COMMIT_SQL)
                .map_err(|source| GenericProjectError::Sqlite {
                    operation: "准备暂存 Generic 译文",
                    source,
                })?;
            for write in writes {
                let (expected_translation, expected_origin, expected_state) = write
                    .expected_translation
                    .as_ref()
                    .map_or((None, None, None), |translation| {
                        (
                            Some(translation.translation.as_str()),
                            Some(translation.origin.storage_name()),
                            Some(translation.state_fingerprint.as_bytes().as_slice()),
                        )
                    });
                insert
                    .execute(params![
                        write.group_id,
                        write.unit_id,
                        write.expected_source_text,
                        write.expected_group_context.as_bytes().as_slice(),
                        write.translation,
                        write.origin.storage_name(),
                        write.state_fingerprint.as_bytes().as_slice(),
                        expected_translation,
                        expected_origin,
                        expected_state,
                    ])
                    .map_err(|source| GenericProjectError::Sqlite {
                        operation: "暂存 Generic Unit 译文",
                        source,
                    })?;
            }
            drop(insert);

            let mut update = transaction
                .prepare(APPLY_PENDING_TRANSLATION_COMMIT_SQL)
                .map_err(|source| GenericProjectError::Sqlite {
                    operation: "准备批量提交 Generic 译文",
                    source,
                })?;
            let updated = update
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|source| GenericProjectError::Sqlite {
                    operation: "批量提交 Generic 译文",
                    source,
                })?;
            for updated in updated {
                let (group_id, unit_id) =
                    updated.map_err(|source| GenericProjectError::Sqlite {
                        operation: "读取 Generic 译文提交结果",
                        source,
                    })?;
                let Some(&index) = write_indexes.get(&(group_id.as_str(), unit_id.as_str())) else {
                    return Err(GenericProjectError::InvalidDatabase {
                        detail: format!(
                            "Generic 译文批量提交返回了未请求的 Unit：{group_id:?}/{unit_id:?}"
                        ),
                    });
                };
                if std::mem::replace(&mut applied[index], true) {
                    return Err(GenericProjectError::InvalidDatabase {
                        detail: format!(
                            "Generic 译文批量提交重复返回 Unit：{group_id:?}/{unit_id:?}"
                        ),
                    });
                }
            }
        }
        let committed = applied.iter().filter(|applied| **applied).count();
        let conflicts = writes
            .iter()
            .zip(applied)
            .filter(|(_, applied)| !*applied)
            .map(|(write, _)| (write.group_id.clone(), write.unit_id.clone()))
            .collect::<Vec<_>>();
        if committed > 0
            && let Some(profile_id) = profile_id
        {
            transaction
                .execute(
                    "UPDATE generic_project
                     SET last_profile_id = ?1
                     WHERE singleton = 1",
                    [profile_id],
                )
                .map_err(|source| GenericProjectError::Sqlite {
                    operation: "保存 Generic 最近 Profile",
                    source,
                })?;
        }
        transaction
            .commit()
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "完成 Generic 译文提交",
                source,
            })?;
        Ok(CommitTranslationsOutcome {
            committed,
            conflicts,
        })
    }

    pub(crate) fn remember_profile(&self, profile_id: &str) -> Result<(), GenericProjectError> {
        if profile_id.is_empty() || profile_id.chars().all(char::is_whitespace) {
            return Err(GenericProjectError::BlankProfileId);
        }
        let connection = self.open_connection(false)?;
        connection
            .execute(
                "UPDATE generic_project SET last_profile_id = ?1 WHERE singleton = 1",
                [profile_id],
            )
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "保存 Generic 最近 Profile",
                source,
            })?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn load_translation_resources(
        &self,
    ) -> Result<TranslationResources, GenericProjectError> {
        let connection = self.open_connection(false)?;
        validate_schema(&connection)?;
        load_translation_resources_rows(&connection)
    }

    /// 原子保存本轮资源，并以 CAS 清除规划时已经确认失效的旧译文。
    pub(crate) fn apply_translation_resources(
        &self,
        expected_raw_fingerprint: Sha256Fingerprint,
        terminology_json: &str,
        placeholder_rules_json: &str,
        invalidations: &[TranslationClear],
    ) -> Result<CommitTranslationsOutcome, GenericProjectError> {
        validate_canonical_resource(TERMINOLOGY_RESOURCE, terminology_json)?;
        validate_canonical_resource(PLACEHOLDER_RULES_RESOURCE, placeholder_rules_json)?;
        validate_placeholder_resource(placeholder_rules_json)?;
        let mut seen = HashSet::with_capacity(invalidations.len());
        for invalidation in invalidations {
            if !seen.insert((&invalidation.group_id, &invalidation.unit_id)) {
                return Err(GenericProjectError::DuplicateTranslationClear {
                    group_id: invalidation.group_id.clone(),
                    unit_id: invalidation.unit_id.clone(),
                });
            }
        }
        let mut connection = self.open_connection(false)?;
        let transaction =
            connection
                .transaction()
                .map_err(|source| GenericProjectError::Sqlite {
                    operation: "开始保存 Generic 翻译资源",
                    source,
                })?;
        let actual = read_optional_fingerprint(
            transaction
                .query_row(
                    "SELECT extracted_raw_fingerprint
                     FROM generic_project WHERE singleton = 1",
                    [],
                    |row| row.get(0),
                )
                .map_err(|source| GenericProjectError::Sqlite {
                    operation: "读取 Generic Extract 指纹",
                    source,
                })?,
            "extracted_raw_fingerprint",
        )?;
        if actual != Some(expected_raw_fingerprint) {
            return Err(GenericProjectError::TranslationSnapshotChanged);
        }
        for (kind, canonical_json) in [
            (TERMINOLOGY_RESOURCE, terminology_json),
            (PLACEHOLDER_RULES_RESOURCE, placeholder_rules_json),
        ] {
            transaction
                .execute(
                    "UPDATE translation_resource
                     SET canonical_json = ?1 WHERE resource_kind = ?2",
                    params![canonical_json, kind],
                )
                .map_err(|source| GenericProjectError::Sqlite {
                    operation: "更新 Generic 翻译资源",
                    source,
                })?;
        }
        let mut conflicts = Vec::new();
        let mut committed = 0;
        {
            let mut statement = transaction
                .prepare_cached(
                    "UPDATE generic_unit
                     SET translation = NULL,
                         translation_origin = NULL,
                         translation_state = NULL
                     WHERE group_id = ?1 AND unit_id = ?2
                       AND source_text = ?3
                       AND EXISTS (
                           SELECT 1 FROM generic_group
                           WHERE group_id = ?1 AND context_fingerprint = ?4
                       )
                       AND translation = ?5
                       AND translation_origin = ?6
                       AND translation_state = ?7",
                )
                .map_err(|source| GenericProjectError::Sqlite {
                    operation: "准备清除失效 Generic 译文",
                    source,
                })?;
            for invalidation in invalidations {
                let changed = statement
                    .execute(params![
                        invalidation.group_id,
                        invalidation.unit_id,
                        invalidation.expected_source_text,
                        invalidation.expected_group_context.as_bytes().as_slice(),
                        invalidation.expected_translation.translation,
                        invalidation.expected_translation.origin.storage_name(),
                        invalidation
                            .expected_translation
                            .state_fingerprint
                            .as_bytes()
                            .as_slice(),
                    ])
                    .map_err(|source| GenericProjectError::Sqlite {
                        operation: "清除失效 Generic 译文",
                        source,
                    })?;
                if changed == 1 {
                    committed += 1;
                } else {
                    conflicts.push((invalidation.group_id.clone(), invalidation.unit_id.clone()));
                }
            }
        }
        transaction
            .commit()
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "提交 Generic 翻译资源",
                source,
            })?;
        Ok(CommitTranslationsOutcome {
            committed,
            conflicts,
        })
    }

    fn open_connection(&self, create: bool) -> Result<Connection, GenericProjectError> {
        if !create && !self.database_path.is_file() {
            return Err(GenericProjectError::ProjectNotFound {
                path: self.database_path.clone(),
            });
        }
        open_sqlite_connection(&self.database_path, create)
    }

    fn read_project_with_connection(
        &self,
        connection: &Connection,
    ) -> Result<GenericProject, GenericProjectError> {
        type ProjectRow = (
            String,
            Vec<u8>,
            String,
            String,
            Option<Vec<u8>>,
            Option<Vec<u8>>,
            Option<String>,
        );
        let row: ProjectRow = connection
            .query_row(
                "SELECT project_name, source_root, source_language, target_language,
                        extracted_raw_fingerprint, extracted_asset_fingerprint, last_profile_id
                 FROM main.generic_project WHERE singleton = 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "读取 Generic 项目记录",
                source,
            })?
            .ok_or_else(|| GenericProjectError::InvalidDatabase {
                detail: "缺少 generic_project 单例记录".to_owned(),
            })?;
        let project_name = row.0.parse::<ProjectName>().map_err(|detail| {
            GenericProjectError::InvalidDatabase {
                detail: format!("项目名称无效：{detail}"),
            }
        })?;
        let source_root = decode_path(&row.1)?;
        let source = LanguageId::parse(&row.2)?;
        let target = LanguageId::parse(&row.3)?;
        validate_distinct_languages(&source, &target)?;
        Ok(GenericProject {
            project_name,
            workspace_root: self.workspace_root.clone(),
            database_path: self.database_path.clone(),
            source_root,
            language_pair: LanguagePair::new(source, target),
            extracted_raw_fingerprint: read_optional_fingerprint(
                row.4,
                "extracted_raw_fingerprint",
            )?,
            extracted_asset_fingerprint: read_optional_fingerprint(
                row.5,
                "extracted_asset_fingerprint",
            )?,
            last_profile_id: row.6,
        })
    }
}

fn open_sqlite_connection(
    database_path: &Path,
    create: bool,
) -> Result<Connection, GenericProjectError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | if create {
            OpenFlags::SQLITE_OPEN_CREATE
        } else {
            OpenFlags::empty()
        };
    let connection = Connection::open_with_flags(database_path, flags).map_err(|source| {
        GenericProjectError::Sqlite {
            operation: "打开 Generic 项目数据库",
            source,
        }
    })?;
    if create {
        apply_att_sqlite_new_database_page_policy(&connection).map_err(|source| {
            GenericProjectError::Sqlite {
                operation: "设置 Generic 新数据库物理页策略",
                source,
            }
        })?;
    }
    apply_att_sqlite_read_write_policy(&connection).map_err(|source| {
        GenericProjectError::Sqlite {
            operation: "应用 Generic SQLite 读写策略",
            source,
        }
    })?;
    Ok(connection)
}

/// Generic 项目行为失败。
#[derive(Debug)]
pub(crate) enum GenericProjectError {
    MissingInitialField(&'static str),
    WorkspaceNotDirectory {
        path: PathBuf,
    },
    SourceNotDirectory {
        path: PathBuf,
    },
    SourceWriteBackOverlap {
        source_root: PathBuf,
        write_back_root: PathBuf,
    },
    ProjectNotFound {
        path: PathBuf,
    },
    ProjectIdentityMismatch {
        expected: String,
        observed: String,
    },
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Sqlite {
        operation: &'static str,
        source: rusqlite::Error,
    },
    InitialCandidateCleanup {
        original: Box<GenericProjectError>,
        path: PathBuf,
        cleanup: io::Error,
    },
    InvalidDatabase {
        detail: String,
    },
    InvalidLanguage(LanguageIdError),
    SameSourceAndTargetLanguage {
        language: String,
    },
    Jsonl(GenericJsonlError),
    InputChangedDuringExtract,
    ExtractRequired,
    TranslationSnapshotChanged,
    InvalidTranslation(String),
    DuplicateTranslationWrite {
        group_id: String,
        unit_id: String,
    },
    DuplicateTranslationClear {
        group_id: String,
        unit_id: String,
    },
    BlankProfileId,
    InvalidResource {
        kind: &'static str,
        detail: String,
    },
    UnitNotFound {
        group_id: String,
        unit_id: String,
    },
}

impl fmt::Display for GenericProjectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingInitialField(field) => {
                write!(formatter, "首次 Generic Init 必须提供 --{field}")
            }
            Self::WorkspaceNotDirectory { path } => {
                write!(formatter, "Generic 工作区路径不是目录：{}", path.display())
            }
            Self::SourceNotDirectory { path } => {
                write!(
                    formatter,
                    "Generic 输入路径不是现存目录：{}",
                    path.display()
                )
            }
            Self::SourceWriteBackOverlap {
                source_root,
                write_back_root,
            } => write!(
                formatter,
                "Generic 输入目录与写回目录不能相同或互为祖先：输入={}，写回={}",
                source_root.display(),
                write_back_root.display()
            ),
            Self::ProjectNotFound { path } => {
                write!(formatter, "Generic 项目不存在：{}", path.display())
            }
            Self::ProjectIdentityMismatch { expected, observed } => write!(
                formatter,
                "Generic 项目数据库属于 {expected:?}，不能作为 {observed:?} 打开"
            ),
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "{operation}失败：{}（{source}）", path.display()),
            Self::Sqlite { operation, source } => write!(formatter, "{operation}失败：{source}"),
            Self::InitialCandidateCleanup {
                original,
                path,
                cleanup,
            } => write!(
                formatter,
                "{original}；清理初始数据库候选失败：{}（{cleanup}）",
                path.display()
            ),
            Self::InvalidDatabase { detail } => {
                write!(formatter, "Generic 项目数据库无效：{detail}")
            }
            Self::InvalidLanguage(source) => write!(formatter, "Generic 项目语言无效：{source}"),
            Self::SameSourceAndTargetLanguage { language } => {
                write!(formatter, "Generic 源语言与目标语言不能相同：{language}")
            }
            Self::Jsonl(source) => source.fmt(formatter),
            Self::InputChangedDuringExtract => {
                formatter.write_str("Generic 输入在 Extract 期间发生变化，数据库未提交")
            }
            Self::ExtractRequired => {
                formatter.write_str("Generic 输入已变化或尚未提取，请先运行 Extract")
            }
            Self::TranslationSnapshotChanged => {
                formatter.write_str("Generic 翻译依据的 Extract 快照已经变化")
            }
            Self::InvalidTranslation(detail) => {
                write!(formatter, "Generic 译文无效：{detail}")
            }
            Self::DuplicateTranslationWrite { group_id, unit_id } => write!(
                formatter,
                "同一批次重复提交 Generic Unit：{group_id:?}/{unit_id:?}"
            ),
            Self::DuplicateTranslationClear { group_id, unit_id } => write!(
                formatter,
                "同一批次重复清除 Generic Unit：{group_id:?}/{unit_id:?}"
            ),
            Self::BlankProfileId => formatter.write_str("Generic Profile ID 不能为空白"),
            Self::InvalidResource { kind, detail } => {
                write!(formatter, "Generic {kind} 资源无效：{detail}")
            }
            Self::UnitNotFound { group_id, unit_id } => {
                write!(formatter, "Generic Unit 不存在：{group_id:?}/{unit_id:?}")
            }
        }
    }
}

impl Error for GenericProjectError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Sqlite { source, .. } => Some(source),
            Self::InitialCandidateCleanup { original, .. } => Some(original.as_ref()),
            Self::InvalidLanguage(source) => Some(source),
            Self::Jsonl(source) => Some(source),
            Self::MissingInitialField(_)
            | Self::WorkspaceNotDirectory { .. }
            | Self::SourceNotDirectory { .. }
            | Self::SourceWriteBackOverlap { .. }
            | Self::ProjectNotFound { .. }
            | Self::ProjectIdentityMismatch { .. }
            | Self::InvalidDatabase { .. }
            | Self::SameSourceAndTargetLanguage { .. }
            | Self::InputChangedDuringExtract
            | Self::ExtractRequired
            | Self::TranslationSnapshotChanged
            | Self::InvalidTranslation(_)
            | Self::DuplicateTranslationWrite { .. }
            | Self::DuplicateTranslationClear { .. }
            | Self::BlankProfileId
            | Self::InvalidResource { .. }
            | Self::UnitNotFound { .. } => None,
        }
    }
}

impl From<GenericJsonlError> for GenericProjectError {
    fn from(source: GenericJsonlError) -> Self {
        Self::Jsonl(source)
    }
}

impl From<LanguageIdError> for GenericProjectError {
    fn from(source: LanguageIdError) -> Self {
        Self::InvalidLanguage(source)
    }
}

struct ReconciledSnapshot {
    files: Vec<GenericStoredFile>,
    preserved_translations: usize,
    cleared_translations: usize,
}

/// 在 Lua 已打开的同一项目事务内，从当前项目资源重建人工译文状态。
pub(crate) fn manual_translation_state_for_connection(
    connection: &Connection,
    group_id: &str,
    unit_id: &str,
) -> Result<Sha256Fingerprint, GenericProjectError> {
    let (source_language, target_language): (String, String) = connection
        .query_row(
            "SELECT source_language, target_language
             FROM main.generic_project WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|source| GenericProjectError::Sqlite {
            operation: "读取 Generic Lua 语言事实",
            source,
        })?;
    let language_pair = LanguagePair::new(
        LanguageId::parse(&source_language)?,
        LanguageId::parse(&target_language)?,
    );
    let row: Option<(String, Vec<u8>, String)> = connection
        .query_row(
            "SELECT generic_group.kind, generic_group.context_fingerprint,
                    generic_unit.source_text
             FROM main.generic_unit AS generic_unit
             JOIN main.generic_group AS generic_group USING (group_id)
             WHERE generic_unit.group_id = ?1 AND generic_unit.unit_id = ?2",
            params![group_id, unit_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|source| GenericProjectError::Sqlite {
            operation: "读取 Generic Lua Unit",
            source,
        })?;
    let Some((kind, context, source_text)) = row else {
        return Err(GenericProjectError::UnitNotFound {
            group_id: group_id.to_owned(),
            unit_id: unit_id.to_owned(),
        });
    };
    let placeholder_json: String = connection
        .query_row(
            "SELECT canonical_json FROM main.translation_resource
             WHERE resource_kind = 'placeholder_rules'",
            [],
            |row| row.get(0),
        )
        .map_err(|source| GenericProjectError::Sqlite {
            operation: "读取 Generic Lua Placeholder 资源",
            source,
        })?;
    let binding = placeholder_binding_fingerprint(&placeholder_json, &kind, &source_text).map_err(
        |source| GenericProjectError::InvalidResource {
            kind: PLACEHOLDER_RULES_RESOURCE,
            detail: source.to_string(),
        },
    )?;
    Ok(manual_translation_state_fingerprint(
        &language_pair,
        &GenericUnitKey::new(group_id.to_owned(), unit_id.to_owned()),
        &source_text,
        read_fingerprint(context, "context_fingerprint")?,
        binding,
    ))
}

/// 在同一事务内校验人工译文的 Placeholder，并建立它的 Current 状态。
pub(crate) fn validated_manual_translation_state_for_connection(
    connection: &Connection,
    group_id: &str,
    unit_id: &str,
    translation: &str,
) -> Result<Sha256Fingerprint, GenericProjectError> {
    let (kind, source_text, placeholder_json): (String, String, String) = connection
        .query_row(
            "SELECT generic_group.kind, generic_unit.source_text,
                    translation_resource.canonical_json
             FROM main.generic_unit AS generic_unit
             JOIN main.generic_group AS generic_group USING (group_id)
             JOIN main.translation_resource AS translation_resource
               ON translation_resource.resource_kind = 'placeholder_rules'
             WHERE generic_unit.group_id = ?1 AND generic_unit.unit_id = ?2",
            params![group_id, unit_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|source| GenericProjectError::Sqlite {
            operation: "读取 Generic Lua Placeholder 事实",
            source,
        })?
        .ok_or_else(|| GenericProjectError::UnitNotFound {
            group_id: group_id.to_owned(),
            unit_id: unit_id.to_owned(),
        })?;
    validate_manual_translation_placeholders(&placeholder_json, &kind, &source_text, translation)
        .map_err(|source| GenericProjectError::InvalidTranslation(source.to_string()))?;
    manual_translation_state_for_connection(connection, group_id, unit_id)
}

fn reconcile_snapshot(
    previous: &GenericStoredSnapshot,
    scanned: &GenericInputSnapshot,
) -> ReconciledSnapshot {
    let previous_groups = previous
        .files
        .iter()
        .flat_map(|file| &file.groups)
        .map(|group| (group.id.as_str(), group))
        .collect::<HashMap<_, _>>();
    let previous_translation_count = previous
        .files
        .iter()
        .flat_map(|file| &file.groups)
        .flat_map(|group| &group.units)
        .filter(|unit| unit.translation.is_some())
        .count();

    let mut preserved_translations = 0;
    let files = scanned
        .files()
        .iter()
        .enumerate()
        .map(|(file_ordinal, file)| {
            let groups = file
                .groups()
                .iter()
                .enumerate()
                .map(|(group_ordinal, group)| {
                    let context_fingerprint = group_context_fingerprint(
                        group.kind(),
                        group.units().iter().map(|unit| unit.text()),
                    );
                    let previous_group = previous_groups.get(group.id()).copied();
                    let preserve_units = previous_group
                        .is_some_and(|old| group_allows_unit_preservation(old, group));
                    let previous_units = previous_group
                        .filter(|_| preserve_units)
                        .map(|old| old.units.as_slice());
                    let units = group
                        .units()
                        .iter()
                        .enumerate()
                        .map(|(unit_ordinal, unit)| {
                            let translation = previous_units
                                .and_then(|units| units.get(unit_ordinal))
                                .filter(|old| old.id == unit.id() && old.source_text == unit.text())
                                .and_then(|old| old.translation.clone());
                            if translation.is_some() {
                                preserved_translations += 1;
                            }
                            GenericStoredUnit {
                                id: unit.id().to_owned(),
                                ordinal: unit_ordinal,
                                source_text: unit.text().to_owned(),
                                translation,
                            }
                        })
                        .collect();
                    GenericStoredGroup {
                        id: group.id().to_owned(),
                        ordinal: group_ordinal,
                        kind: group.kind().to_owned(),
                        context_fingerprint,
                        units,
                    }
                })
                .collect();
            GenericStoredFile {
                relative_path: file.relative_path().to_path_buf(),
                ordinal: file_ordinal,
                groups,
            }
        })
        .collect();

    ReconciledSnapshot {
        files,
        preserved_translations,
        cleared_translations: previous_translation_count.saturating_sub(preserved_translations),
    }
}

fn group_allows_unit_preservation(
    previous: &GenericStoredGroup,
    current: &super::jsonl::GenericGroup,
) -> bool {
    if previous.kind != current.kind() || previous.units.len() != current.units().len() {
        return false;
    }
    if previous
        .units
        .iter()
        .zip(current.units())
        .any(|(left, right)| left.source_text != right.text())
    {
        return false;
    }
    // 新 ID 表示原位置 Unit 改名；旧 ID 出现在新位置则说明稳定 Unit 确实发生了调序。
    let previous_ordinals = previous
        .units
        .iter()
        .enumerate()
        .map(|(ordinal, unit)| (unit.id.as_str(), ordinal))
        .collect::<HashMap<_, _>>();
    if current.units().iter().enumerate().any(|(ordinal, unit)| {
        previous_ordinals
            .get(unit.id())
            .is_some_and(|previous_ordinal| *previous_ordinal != ordinal)
    }) {
        return false;
    }
    true
}

fn group_context_fingerprint<'a>(
    kind: &str,
    texts: impl IntoIterator<Item = &'a str>,
) -> Sha256Fingerprint {
    let mut hasher = Sha256FramedHasher::new(b"att.generic.group-context");
    hasher.frame(1, kind.as_bytes());
    for text in texts {
        hasher.frame(2, text.as_bytes());
    }
    hasher.finish()
}

fn scan_current_input(
    stored: &GenericStoredSnapshot,
) -> Result<GenericInputSnapshot, GenericProjectError> {
    let live = scan_input_tree(stored.project.source_root())?;
    if Some(live.raw_fingerprint()) != stored.project.extracted_raw_fingerprint
        || Some(live.asset_fingerprint()) != stored.project.extracted_asset_fingerprint
    {
        return Err(GenericProjectError::ExtractRequired);
    }
    validate_stored_assets_match_live(stored, &live)?;
    Ok(live)
}

/// 候选已经建立后再次完整扫描外部输入，并同时比较原始与资产指纹。
///
/// 调用方在同一项目排他租约内持有首次完整验证得到的项目事实，因此这里不再打开
/// 64 MB 级项目数据库或重复执行 SQLite 完整性检查。
pub(crate) fn ensure_input_fingerprints_current(
    project: &GenericProject,
) -> Result<(), GenericProjectError> {
    let expected_raw_fingerprint = project
        .extracted_raw_fingerprint
        .ok_or(GenericProjectError::ExtractRequired)?;
    let expected_asset_fingerprint = project
        .extracted_asset_fingerprint
        .ok_or(GenericProjectError::ExtractRequired)?;
    let live = scan_input_tree(project.source_root())?;
    if live.raw_fingerprint() != expected_raw_fingerprint
        || live.asset_fingerprint() != expected_asset_fingerprint
    {
        return Err(GenericProjectError::ExtractRequired);
    }
    Ok(())
}

fn validate_stored_assets_match_live(
    stored: &GenericStoredSnapshot,
    live: &GenericInputSnapshot,
) -> Result<(), GenericProjectError> {
    if stored.files.len() != live.files().len() {
        return Err(GenericProjectError::InvalidDatabase {
            detail: "Generic 文件记录与当前 Extract 快照不一致".to_owned(),
        });
    }
    for (file_ordinal, (stored_file, live_file)) in
        stored.files.iter().zip(live.files()).enumerate()
    {
        if stored_file.ordinal != file_ordinal
            || stored_file.relative_path != live_file.relative_path()
            || stored_file.groups.len() != live_file.groups().len()
        {
            return Err(GenericProjectError::InvalidDatabase {
                detail: format!(
                    "Generic 文件记录与当前 Extract 快照不一致：{}",
                    live_file.relative_path().display()
                ),
            });
        }
        for (group_ordinal, (stored_group, live_group)) in stored_file
            .groups
            .iter()
            .zip(live_file.groups())
            .enumerate()
        {
            let expected_context = group_context_fingerprint(
                live_group.kind(),
                live_group.units().iter().map(|unit| unit.text()),
            );
            if stored_group.ordinal != group_ordinal
                || stored_group.id != live_group.id()
                || stored_group.kind != live_group.kind()
                || stored_group.context_fingerprint != expected_context
                || stored_group.units.len() != live_group.units().len()
            {
                return Err(GenericProjectError::InvalidDatabase {
                    detail: format!(
                        "Generic Group 记录与当前 Extract 快照不一致：{:?}",
                        live_group.id()
                    ),
                });
            }
            for (unit_ordinal, (stored_unit, live_unit)) in stored_group
                .units
                .iter()
                .zip(live_group.units())
                .enumerate()
            {
                if stored_unit.ordinal != unit_ordinal
                    || stored_unit.id != live_unit.id()
                    || stored_unit.source_text != live_unit.text()
                {
                    return Err(GenericProjectError::InvalidDatabase {
                        detail: format!(
                            "Generic Unit 记录与当前 Extract 快照不一致：{:?}/{:?}",
                            live_group.id(),
                            live_unit.id()
                        ),
                    });
                }
            }
        }
    }
    Ok(())
}

fn replace_snapshot(
    transaction: &Transaction<'_>,
    scanned: &GenericInputSnapshot,
    files: &[GenericStoredFile],
) -> Result<(), GenericProjectError> {
    transaction
        .execute("DELETE FROM generic_file", [])
        .map_err(|source| GenericProjectError::Sqlite {
            operation: "清理上一份 Generic Extract 快照",
            source,
        })?;

    {
        let mut file_statement = transaction
            .prepare_cached("INSERT INTO generic_file (relative_path, ordinal) VALUES (?1, ?2)")
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "写入 Generic 文件",
                source,
            })?;
        let mut group_statement = transaction
            .prepare_cached(
                "INSERT INTO generic_group (
                         group_id, relative_path, ordinal, kind, context_fingerprint
                     ) VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "写入 Generic Group",
                source,
            })?;
        let mut unit_statement = transaction
            .prepare_cached(
                "INSERT INTO generic_unit (
                             group_id, unit_id, ordinal, source_text,
                             translation, translation_origin, translation_state
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "写入 Generic Unit",
                source,
            })?;

        for file in files {
            let relative_path = encode_path(&file.relative_path);
            file_statement
                .execute(params![&relative_path, to_i64(file.ordinal)?])
                .map_err(|source| GenericProjectError::Sqlite {
                    operation: "写入 Generic 文件",
                    source,
                })?;
            for group in &file.groups {
                group_statement
                    .execute(params![
                        group.id,
                        &relative_path,
                        to_i64(group.ordinal)?,
                        group.kind,
                        group.context_fingerprint.as_bytes().as_slice()
                    ])
                    .map_err(|source| GenericProjectError::Sqlite {
                        operation: "写入 Generic Group",
                        source,
                    })?;
                for unit in &group.units {
                    let (translation, origin, state) =
                        unit.translation
                            .as_ref()
                            .map_or((None, None, None), |translation| {
                                (
                                    Some(translation.translation.as_str()),
                                    Some(translation.origin.storage_name()),
                                    Some(translation.state_fingerprint.as_bytes().as_slice()),
                                )
                            });
                    unit_statement
                        .execute(params![
                            group.id,
                            unit.id,
                            to_i64(unit.ordinal)?,
                            unit.source_text,
                            translation,
                            origin,
                            state
                        ])
                        .map_err(|source| GenericProjectError::Sqlite {
                            operation: "写入 Generic Unit",
                            source,
                        })?;
                }
            }
        }
    }
    transaction
        .execute(
            "UPDATE generic_project
             SET extracted_raw_fingerprint = ?1, extracted_asset_fingerprint = ?2
             WHERE singleton = 1",
            params![
                scanned.raw_fingerprint().as_bytes().as_slice(),
                scanned.asset_fingerprint().as_bytes().as_slice()
            ],
        )
        .map_err(|source| GenericProjectError::Sqlite {
            operation: "保存 Generic Extract 指纹",
            source,
        })?;
    Ok(())
}

fn load_translation_resources_rows(
    connection: &Connection,
) -> Result<TranslationResources, GenericProjectError> {
    let read = |kind: &'static str| {
        connection
            .query_row(
                "SELECT canonical_json FROM translation_resource WHERE resource_kind = ?1",
                [kind],
                |row| row.get::<_, String>(0),
            )
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "读取 Generic 翻译资源",
                source,
            })
    };
    Ok(TranslationResources {
        terminology_json: read(TERMINOLOGY_RESOURCE)?,
        placeholder_rules_json: read(PLACEHOLDER_RULES_RESOURCE)?,
    })
}

fn load_snapshot_rows(
    connection: &Connection,
    project: &GenericProject,
) -> Result<GenericStoredSnapshot, GenericProjectError> {
    let mut files = Vec::new();
    let mut file_indexes = HashMap::new();
    let mut file_statement = connection
        .prepare(
            "SELECT relative_path, ordinal
             FROM main.generic_file ORDER BY ordinal",
        )
        .map_err(|source| GenericProjectError::Sqlite {
            operation: "准备读取 Generic 文件",
            source,
        })?;
    let file_rows = file_statement
        .query_map([], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|source| GenericProjectError::Sqlite {
            operation: "读取 Generic 文件",
            source,
        })?;
    for file_row in file_rows {
        let (path_bytes, ordinal) = file_row.map_err(|source| GenericProjectError::Sqlite {
            operation: "解码 Generic 文件记录",
            source,
        })?;
        let relative_path = decode_path(&path_bytes)?;
        let file_index = files.len();
        file_indexes.insert(path_bytes, file_index);
        files.push(GenericStoredFile {
            relative_path,
            ordinal: from_i64(ordinal, "file.ordinal")?,
            groups: Vec::new(),
        });
    }
    drop(file_statement);

    let mut group_indexes = HashMap::new();
    let mut group_statement = connection
        .prepare(
            "SELECT g.relative_path, g.group_id, g.ordinal,
                    g.kind, g.context_fingerprint
             FROM main.generic_group AS g
             JOIN main.generic_file AS f
               ON f.relative_path = g.relative_path
             ORDER BY f.ordinal, g.ordinal",
        )
        .map_err(|source| GenericProjectError::Sqlite {
            operation: "准备读取 Generic Group",
            source,
        })?;
    let group_rows = group_statement
        .query_map([], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Vec<u8>>(4)?,
            ))
        })
        .map_err(|source| GenericProjectError::Sqlite {
            operation: "读取 Generic Group",
            source,
        })?;
    for group_row in group_rows {
        let (path_bytes, group_id, group_ordinal, kind, context) =
            group_row.map_err(|source| GenericProjectError::Sqlite {
                operation: "解码 Generic Group",
                source,
            })?;
        let Some(&file_index) = file_indexes.get(&path_bytes) else {
            return Err(GenericProjectError::InvalidDatabase {
                detail: format!("Generic Group {group_id:?} 引用了不存在的文件"),
            });
        };
        let group_index = files[file_index].groups.len();
        group_indexes.insert(group_id.clone(), (file_index, group_index));
        files[file_index].groups.push(GenericStoredGroup {
            id: group_id,
            ordinal: from_i64(group_ordinal, "group.ordinal")?,
            kind,
            context_fingerprint: read_fingerprint(context, "context_fingerprint")?,
            units: Vec::new(),
        });
    }
    drop(group_statement);

    let mut unit_statement = connection
        .prepare(LOAD_UNITS_NATURAL_SQL)
        .map_err(|source| GenericProjectError::Sqlite {
            operation: "准备读取 Generic Unit",
            source,
        })?;
    let unit_rows = unit_statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<Vec<u8>>>(6)?,
            ))
        })
        .map_err(|source| GenericProjectError::Sqlite {
            operation: "读取 Generic Unit",
            source,
        })?;
    for unit_row in unit_rows {
        let (group_id, unit_id, unit_ordinal, source_text, translation, origin, state) =
            unit_row.map_err(|source| GenericProjectError::Sqlite {
                operation: "解码 Generic Unit",
                source,
            })?;
        let translation = match (translation, origin, state) {
            (None, None, None) => None,
            (Some(translation), Some(origin), Some(state)) => Some(GenericStoredTranslation {
                translation,
                origin: TranslationOrigin::parse(&origin)?,
                state_fingerprint: read_fingerprint(state, "translation_state")?,
            }),
            _ => {
                return Err(GenericProjectError::InvalidDatabase {
                    detail: format!("Generic Unit {group_id:?}/{unit_id:?} 的译文状态不完整"),
                });
            }
        };
        let Some(&(file_index, group_index)) = group_indexes.get(&group_id) else {
            return Err(GenericProjectError::InvalidDatabase {
                detail: format!("Generic Unit {group_id:?}/{unit_id:?} 引用了不存在的 Group"),
            });
        };
        files[file_index].groups[group_index]
            .units
            .push(GenericStoredUnit {
                id: unit_id,
                ordinal: from_i64(unit_ordinal, "unit.ordinal")?,
                source_text,
                translation,
            });
    }
    Ok(GenericStoredSnapshot {
        project: project.clone(),
        files,
    })
}

fn create_initial_schema(
    connection: &mut Connection,
    project_name: &ProjectName,
    source_root: &Path,
    source_language: &LanguageId,
    target_language: &LanguageId,
) -> Result<(), GenericProjectError> {
    let transaction = connection
        .transaction()
        .map_err(|source| GenericProjectError::Sqlite {
            operation: "开始建立 Generic schema",
            source,
        })?;
    transaction
        .execute_batch(
            "CREATE TABLE generic_project (
                 singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                 project_name TEXT NOT NULL CHECK (length(project_name) > 0),
                 source_root BLOB NOT NULL CHECK (length(source_root) > 0 AND length(source_root) % 2 = 0),
                 source_language TEXT NOT NULL CHECK (length(source_language) > 0),
                 target_language TEXT NOT NULL CHECK (length(target_language) > 0),
                 extracted_raw_fingerprint BLOB CHECK (
                     extracted_raw_fingerprint IS NULL OR length(extracted_raw_fingerprint) = 32
                 ),
                 extracted_asset_fingerprint BLOB CHECK (
                     extracted_asset_fingerprint IS NULL OR length(extracted_asset_fingerprint) = 32
                 ),
                 last_profile_id TEXT CHECK (last_profile_id IS NULL OR length(last_profile_id) > 0),
                 CHECK (
                     (extracted_raw_fingerprint IS NULL) =
                     (extracted_asset_fingerprint IS NULL)
                 )
             ) STRICT;
             CREATE TABLE generic_file (
                 relative_path BLOB PRIMARY KEY
                     CHECK (length(relative_path) > 0 AND length(relative_path) % 2 = 0),
                 ordinal INTEGER NOT NULL UNIQUE CHECK (ordinal >= 0)
             ) STRICT;
             CREATE TABLE generic_group (
                 group_id TEXT PRIMARY KEY CHECK (length(CAST(group_id AS BLOB)) > 0),
                 relative_path BLOB NOT NULL REFERENCES generic_file(relative_path) ON DELETE CASCADE,
                 ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
                 kind TEXT NOT NULL CHECK (length(CAST(kind AS BLOB)) > 0),
                 context_fingerprint BLOB NOT NULL CHECK (length(context_fingerprint) = 32),
                 UNIQUE (relative_path, ordinal)
             ) STRICT;
             CREATE TABLE generic_unit (
                 group_id TEXT NOT NULL REFERENCES generic_group(group_id) ON DELETE CASCADE,
                 unit_id TEXT NOT NULL CHECK (length(CAST(unit_id AS BLOB)) > 0),
                 ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
                 source_text TEXT NOT NULL CHECK (
                     instr(source_text, char(13)) = 0 AND instr(source_text, char(0)) = 0
                 ),
                 translation TEXT,
                 translation_origin TEXT CHECK (
                     translation_origin IS NULL OR translation_origin IN ('automatic', 'manual')
                 ),
                 translation_state BLOB CHECK (
                     translation_state IS NULL OR length(translation_state) = 32
                 ),
                 PRIMARY KEY (group_id, unit_id),
                 UNIQUE (group_id, ordinal),
                 CHECK (
                     (translation IS NULL AND translation_origin IS NULL AND translation_state IS NULL)
                     OR
                     (translation IS NOT NULL AND length(trim(translation)) > 0
                      AND instr(translation, char(13)) = 0
                      AND instr(translation, char(0)) = 0
                      AND translation_origin IS NOT NULL
                      AND translation_state IS NOT NULL)
                 )
             ) STRICT;
             CREATE TABLE translation_resource (
                 resource_kind TEXT PRIMARY KEY CHECK (
                     resource_kind IN ('terminology', 'placeholder_rules')
                 ),
                 canonical_json TEXT NOT NULL CHECK (length(canonical_json) > 0)
             ) STRICT;
             INSERT INTO translation_resource (resource_kind, canonical_json)
             VALUES ('terminology', '[]'), ('placeholder_rules', '[]');",
        )
        .map_err(|source| GenericProjectError::Sqlite {
            operation: "建立 Generic schema",
            source,
        })?;
    transaction
        .execute(
            "INSERT INTO main.generic_project (
                 singleton, project_name, source_root, source_language, target_language
             ) VALUES (1, ?1, ?2, ?3, ?4)",
            params![
                project_name.as_str(),
                encode_path(source_root),
                source_language.as_str(),
                target_language.as_str()
            ],
        )
        .map_err(|source| GenericProjectError::Sqlite {
            operation: "写入 Generic 项目事实",
            source,
        })?;
    transaction
        .commit()
        .map_err(|source| GenericProjectError::Sqlite {
            operation: "提交 Generic schema",
            source,
        })
}

fn validate_schema(connection: &Connection) -> Result<(), GenericProjectError> {
    for table in [
        "generic_project",
        "generic_file",
        "generic_group",
        "generic_unit",
        "translation_resource",
    ] {
        let exists: i64 = connection
            .query_row(
                "SELECT count(*) FROM main.sqlite_schema
                 WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "检查 Generic schema",
                source,
            })?;
        if exists != 1 {
            return Err(GenericProjectError::InvalidDatabase {
                detail: format!("缺少表 {table}"),
            });
        }
    }
    let resource_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM main.translation_resource
             WHERE (resource_kind = 'terminology' OR resource_kind = 'placeholder_rules')
               AND length(canonical_json) > 0",
            [],
            |row| row.get(0),
        )
        .map_err(|source| GenericProjectError::Sqlite {
            operation: "检查 Generic 翻译资源",
            source,
        })?;
    if resource_count != 2 {
        return Err(GenericProjectError::InvalidDatabase {
            detail: "translation_resource 必须恰好包含术语与 Placeholder 两项".to_owned(),
        });
    }
    for kind in [TERMINOLOGY_RESOURCE, PLACEHOLDER_RULES_RESOURCE] {
        let canonical_json: String = connection
            .query_row(
                "SELECT canonical_json FROM main.translation_resource
                 WHERE resource_kind = ?1",
                [kind],
                |row| row.get(0),
            )
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "读取 Generic 翻译资源以检查",
                source,
            })?;
        validate_canonical_resource(kind, &canonical_json)?;
        if kind == PLACEHOLDER_RULES_RESOURCE {
            validate_placeholder_resource(&canonical_json)?;
        }
    }
    let foreign_key_issue: Option<String> = connection
        .query_row("PRAGMA main.foreign_key_check", [], |row| row.get(0))
        .optional()
        .map_err(|source| GenericProjectError::Sqlite {
            operation: "检查 Generic 外键",
            source,
        })?;
    if let Some(table) = foreign_key_issue {
        return Err(GenericProjectError::InvalidDatabase {
            detail: format!("表 {table} 存在外键错误"),
        });
    }
    let quick_check: String = connection
        .query_row("PRAGMA main.quick_check", [], |row| row.get(0))
        .map_err(|source| GenericProjectError::Sqlite {
            operation: "检查 Generic SQLite 完整性",
            source,
        })?;
    if quick_check != "ok" {
        return Err(GenericProjectError::InvalidDatabase {
            detail: format!("SQLite quick_check：{quick_check}"),
        });
    }
    Ok(())
}

/// 在调用方持有的同一连接上检查 Generic 项目事实、Extract 快照与数据库不变量。
///
/// 原子 Lua 在提交前调用此入口，确保检查能够看见尚未提交的脚本修改。
pub(crate) fn validate_project_connection(
    connection: &Connection,
    expected: &GenericProject,
) -> Result<(), GenericProjectError> {
    validate_schema(connection)?;
    let store = GenericProjectStore::for_workspace(expected.workspace_root.clone());
    let actual = store.read_project_with_connection(connection)?;
    if actual.project_name != expected.project_name {
        return Err(GenericProjectError::InvalidDatabase {
            detail: "Lua 修改了 Generic 项目名称".to_owned(),
        });
    }
    if actual.source_root != expected.source_root {
        return Err(GenericProjectError::InvalidDatabase {
            detail: "Lua 修改了 Generic 输入目录绑定".to_owned(),
        });
    }
    if actual.language_pair != expected.language_pair {
        return Err(GenericProjectError::InvalidDatabase {
            detail: "Lua 修改了 Generic 项目语言对".to_owned(),
        });
    }
    if actual.extracted_raw_fingerprint != expected.extracted_raw_fingerprint
        || actual.extracted_asset_fingerprint != expected.extracted_asset_fingerprint
    {
        return Err(GenericProjectError::InvalidDatabase {
            detail: "Lua 修改了 Generic Extract 指纹".to_owned(),
        });
    }
    validate_source_write_back_separation(&actual.source_root, &actual.workspace_root)?;

    let Some(expected_raw_fingerprint) = expected.extracted_raw_fingerprint else {
        let asset_count: i64 = connection
            .query_row(
                "SELECT
                     (SELECT count(*) FROM main.generic_file)
                   + (SELECT count(*) FROM main.generic_group)
                   + (SELECT count(*) FROM main.generic_unit)",
                [],
                |row| row.get(0),
            )
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "检查未 Extract 的 Generic 资产",
                source,
            })?;
        if asset_count != 0 {
            return Err(GenericProjectError::InvalidDatabase {
                detail: "未 Extract 的 Generic 项目不能包含资产记录".to_owned(),
            });
        }
        return Ok(());
    };

    let live = scan_input_tree(expected.source_root())?;
    if live.raw_fingerprint() != expected_raw_fingerprint
        || Some(live.asset_fingerprint()) != expected.extracted_asset_fingerprint
    {
        return Err(GenericProjectError::InputChangedDuringExtract);
    }
    let stored = load_snapshot_rows(connection, &actual)?;
    validate_stored_assets_match_live(&stored, &live)
}

fn clear_extracted_assets(transaction: &Transaction<'_>) -> Result<(), GenericProjectError> {
    transaction
        .execute("DELETE FROM generic_file", [])
        .and_then(|_| {
            transaction.execute(
                "UPDATE generic_project
                 SET extracted_raw_fingerprint = NULL,
                     extracted_asset_fingerprint = NULL
                 WHERE singleton = 1",
                [],
            )
        })
        .map_err(|source| GenericProjectError::Sqlite {
            operation: "清理 Generic Extract 状态",
            source,
        })?;
    Ok(())
}

fn clear_all_translations(transaction: &Transaction<'_>) -> Result<(), GenericProjectError> {
    transaction
        .execute(
            "UPDATE generic_unit
             SET translation = NULL, translation_origin = NULL, translation_state = NULL",
            [],
        )
        .map_err(|source| GenericProjectError::Sqlite {
            operation: "清理 Generic 语言相关译文",
            source,
        })?;
    Ok(())
}

fn resolve_source_root(path: &Path) -> Result<PathBuf, GenericProjectError> {
    if !path.is_dir() {
        return Err(GenericProjectError::SourceNotDirectory {
            path: path.to_path_buf(),
        });
    }
    fs::canonicalize(path).map_err(|source| GenericProjectError::Io {
        operation: "解析 Generic 输入目录",
        path: path.to_path_buf(),
        source,
    })
}

fn validate_distinct_languages(
    source: &LanguageId,
    target: &LanguageId,
) -> Result<(), GenericProjectError> {
    if source == target {
        return Err(GenericProjectError::SameSourceAndTargetLanguage {
            language: source.as_str().to_owned(),
        });
    }
    Ok(())
}

fn validate_source_write_back_separation(
    source_root: &Path,
    workspace_root: &Path,
) -> Result<(), GenericProjectError> {
    let write_back_root = resolve_planned_path(&workspace_root.join("write_back"))?;
    if source_root == write_back_root
        || source_root.starts_with(&write_back_root)
        || write_back_root.starts_with(source_root)
    {
        return Err(GenericProjectError::SourceWriteBackOverlap {
            source_root: source_root.to_path_buf(),
            write_back_root,
        });
    }
    Ok(())
}

fn resolve_planned_path(path: &Path) -> Result<PathBuf, GenericProjectError> {
    let absolute = std::path::absolute(path).map_err(|source| GenericProjectError::Io {
        operation: "建立 Generic 输出绝对路径",
        path: path.to_path_buf(),
        source,
    })?;
    let mut cursor = absolute.as_path();
    let mut missing = Vec::<OsString>::new();
    loop {
        match fs::canonicalize(cursor) {
            Ok(mut resolved) => {
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                let component = cursor.file_name().ok_or_else(|| GenericProjectError::Io {
                    operation: "解析 Generic 输出路径",
                    path: absolute.clone(),
                    source: io::Error::new(io::ErrorKind::NotFound, "找不到可规范化的现存祖先目录"),
                })?;
                missing.push(component.to_os_string());
                cursor = cursor.parent().ok_or_else(|| GenericProjectError::Io {
                    operation: "解析 Generic 输出路径",
                    path: absolute.clone(),
                    source: io::Error::new(io::ErrorKind::NotFound, "找不到可规范化的现存祖先目录"),
                })?;
            }
            Err(source) => {
                return Err(GenericProjectError::Io {
                    operation: "解析 Generic 输出路径",
                    path: cursor.to_path_buf(),
                    source,
                });
            }
        }
    }
}

fn cleanup_initial_database_candidate(candidate_path: &Path) -> Result<(), (PathBuf, io::Error)> {
    let mut paths = vec![candidate_path.to_path_buf()];
    for suffix in ["-journal", "-wal", "-shm"] {
        let mut sidecar = candidate_path.as_os_str().to_os_string();
        sidecar.push(suffix);
        paths.push(PathBuf::from(sidecar));
    }

    let mut first_failure = None;
    for path in paths {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) if first_failure.is_none() => first_failure = Some((path, source)),
            Err(_) => {}
        }
    }
    match first_failure {
        Some(failure) => Err(failure),
        None => Ok(()),
    }
}

fn validate_translation(translation: &str) -> Result<(), GenericProjectError> {
    if translation.chars().all(char::is_whitespace) {
        return Err(GenericProjectError::InvalidTranslation(
            "译文不能为空白".to_owned(),
        ));
    }
    if translation.contains('\r') {
        return Err(GenericProjectError::InvalidTranslation(
            "译文不能包含 CR（U+000D）".to_owned(),
        ));
    }
    if translation.contains('\0') {
        return Err(GenericProjectError::InvalidTranslation(
            "译文不能包含 NUL（U+0000）".to_owned(),
        ));
    }
    Ok(())
}

fn validate_canonical_resource(
    kind: &'static str,
    canonical_json: &str,
) -> Result<(), GenericProjectError> {
    let value: serde_json::Value = serde_json::from_str(canonical_json).map_err(|source| {
        GenericProjectError::InvalidResource {
            kind,
            detail: source.to_string(),
        }
    })?;
    if !value.is_array() {
        return Err(GenericProjectError::InvalidResource {
            kind,
            detail: "规范快照必须是 JSON array".to_owned(),
        });
    }
    let encoded =
        serde_json::to_string(&value).map_err(|source| GenericProjectError::InvalidResource {
            kind,
            detail: source.to_string(),
        })?;
    if encoded != canonical_json {
        return Err(GenericProjectError::InvalidResource {
            kind,
            detail: "资源快照不是规范紧凑 JSON".to_owned(),
        });
    }
    Ok(())
}

fn validate_placeholder_resource(canonical_json: &str) -> Result<(), GenericProjectError> {
    let service = GenericPlaceholderService::default();
    let definitions = service
        .parse_canonical_json(canonical_json)
        .map_err(|source| GenericProjectError::InvalidResource {
            kind: PLACEHOLDER_RULES_RESOURCE,
            detail: source.to_string(),
        })?;
    service
        .compile(definitions)
        .map_err(|source| GenericProjectError::InvalidResource {
            kind: PLACEHOLDER_RULES_RESOURCE,
            detail: source.to_string(),
        })?;
    Ok(())
}

fn to_i64(value: usize) -> Result<i64, GenericProjectError> {
    i64::try_from(value).map_err(|_| GenericProjectError::InvalidDatabase {
        detail: "序号超过 SQLite INTEGER 可表达范围".to_owned(),
    })
}

fn from_i64(value: i64, field: &'static str) -> Result<usize, GenericProjectError> {
    usize::try_from(value).map_err(|_| GenericProjectError::InvalidDatabase {
        detail: format!("{field} 不是有效非负序号：{value}"),
    })
}

fn read_optional_fingerprint(
    bytes: Option<Vec<u8>>,
    field: &'static str,
) -> Result<Option<Sha256Fingerprint>, GenericProjectError> {
    bytes
        .map(|bytes| read_fingerprint(bytes, field))
        .transpose()
}

fn read_fingerprint(
    bytes: Vec<u8>,
    field: &'static str,
) -> Result<Sha256Fingerprint, GenericProjectError> {
    let array = <[u8; SHA256_FINGERPRINT_BYTES]>::try_from(bytes).map_err(|bytes| {
        GenericProjectError::InvalidDatabase {
            detail: format!("{field} 必须是 32 字节，实际为 {}", bytes.len()),
        }
    })?;
    Ok(Sha256Fingerprint::from_bytes(array))
}

#[cfg(windows)]
fn encode_path(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(windows)]
fn decode_path(bytes: &[u8]) -> Result<PathBuf, GenericProjectError> {
    use std::os::windows::ffi::OsStringExt;

    if bytes.is_empty() || !bytes.len().is_multiple_of(2) {
        return Err(GenericProjectError::InvalidDatabase {
            detail: "路径 BLOB 必须是非空 UTF-16LE code units".to_owned(),
        });
    }
    let units = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    Ok(PathBuf::from(OsString::from_wide(&units)))
}

#[cfg(not(windows))]
fn encode_path(path: &Path) -> Vec<u8> {
    path.as_os_str().as_encoded_bytes().to_vec()
}

#[cfg(not(windows))]
fn decode_path(bytes: &[u8]) -> Result<PathBuf, GenericProjectError> {
    let value = std::str::from_utf8(bytes).map_err(|_| GenericProjectError::InvalidDatabase {
        detail: "路径不是有效 UTF-8".to_owned(),
    })?;
    Ok(PathBuf::from(value))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn language(value: &str) -> LanguageId {
        LanguageId::parse(value).expect("测试语言应合法")
    }

    fn init(workspace: &Path, source: &Path) -> GenericProjectStore {
        GenericProjectStore::initialize(GenericInitRequest {
            project_name: "game".parse().unwrap(),
            workspace_root: workspace.to_path_buf(),
            source_root: Some(source.to_path_buf()),
            source_language: Some(language("ja")),
            target_language: Some(language("zh-Hans")),
        })
        .expect("项目应初始化")
        .0
    }

    fn write_source(source: &Path, content: &str) {
        fs::write(source.join("text.jsonl"), content).expect("测试输入应写入");
    }

    #[test]
    fn first_init_requires_all_values_and_does_not_extract() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");
        let (store, project) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "game".parse().unwrap(),
            workspace_root: workspace,
            source_root: Some(source.canonicalize().unwrap()),
            source_language: Some(language("ja")),
            target_language: Some(language("zh-Hans")),
        })
        .expect("首次 Init 应成功");

        assert_eq!(project.project_name().as_str(), "game");
        assert_eq!(project.extracted_raw_fingerprint(), None);
        assert!(matches!(
            store.load_snapshot(),
            Err(GenericProjectError::ExtractRequired)
        ));
    }

    #[test]
    fn initial_database_and_connections_use_the_common_sqlite_policy() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");
        let store = init(&workspace, &source);
        let project = store.open().expect("Generic 项目应可打开");
        let connection =
            open_sqlite_connection(project.database_path(), false).expect("项目数据库应可重开");

        let page_size: i64 = connection
            .query_row("PRAGMA page_size", [], |row| row.get(0))
            .expect("应可读取 page_size");
        let journal_mode: String = connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("应可读取 journal_mode");
        let synchronous: i64 = connection
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .expect("应可读取 synchronous");
        let cache_size: i64 = connection
            .query_row("PRAGMA cache_size", [], |row| row.get(0))
            .expect("应可读取 cache_size");
        let temp_store: i64 = connection
            .query_row("PRAGMA temp_store", [], |row| row.get(0))
            .expect("应可读取 temp_store");

        assert_eq!(page_size, 64 * 1024);
        assert_eq!(journal_mode, "wal");
        assert_eq!(synchronous, 2, "SQLite FULL synchronous 应返回枚举值 2");
        assert_eq!(cache_size, -(3 * 1024 * 1024));
        assert_eq!(temp_store, 2, "SQLite MEMORY TEMP 模式应返回枚举值 2");
    }

    #[test]
    fn extract_accepts_nul_in_stable_ids_and_kind() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");
        let store = init(&workspace, &source);
        write_source(
            &source,
            r#"{"id":"\u0000group","kind":"\u0000kind","units":[{"id":"\u0000unit","text":"本文"}]}
"#,
        );

        store.extract().expect("NUL 是稳定身份和 kind 的合法内容");
        let snapshot = store.load_snapshot().expect("应读取已提取资产");
        let group = &snapshot.files()[0].groups()[0];

        assert_eq!(group.id(), "\0group");
        assert_eq!(group.kind(), "\0kind");
        assert_eq!(group.units()[0].id(), "\0unit");
    }

    #[test]
    fn failed_first_init_leaves_no_database_and_can_retry_same_workspace() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");

        let error = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "game".parse().unwrap(),
            workspace_root: workspace.clone(),
            source_root: Some(source.clone()),
            source_language: Some(language("ja")),
            target_language: None,
        })
        .expect_err("首次 Init 缺少目标语言时应失败");

        assert!(matches!(
            error,
            GenericProjectError::MissingInitialField("target-language")
        ));
        assert!(!workspace.join(DATABASE_FILE_NAME).exists());
        assert!(!workspace.exists());

        let (_, project) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "game".parse().unwrap(),
            workspace_root: workspace.clone(),
            source_root: Some(source.canonicalize().unwrap()),
            source_language: Some(language("ja")),
            target_language: Some(language("zh-Hans")),
        })
        .expect("补齐参数后应能在同一路径成功 Init");

        assert_eq!(project.source_root(), source.canonicalize().unwrap());
        assert!(workspace.join(DATABASE_FILE_NAME).is_file());
        assert!(fs::read_dir(&workspace).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".project.db.init-")
        }));
    }

    #[test]
    fn initial_schema_and_project_singleton_roll_back_together() {
        use rusqlite::hooks::{AuthAction, Authorization};

        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .authorizer(Some(
                |context: rusqlite::hooks::AuthContext<'_>| match context.action {
                    AuthAction::Insert {
                        table_name: "generic_project",
                    } => Authorization::Deny,
                    _ => Authorization::Allow,
                },
            ))
            .unwrap();

        let result = create_initial_schema(
            &mut connection,
            &"game".parse().unwrap(),
            &source,
            &language("ja"),
            &language("zh-Hans"),
        );
        assert!(result.is_err(), "单例写入失败时 Init 事务应失败");

        connection
            .authorizer(None::<fn(rusqlite::hooks::AuthContext<'_>) -> Authorization>)
            .unwrap();
        let table_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_schema
                 WHERE type = 'table'
                   AND name IN (
                       'generic_project', 'generic_file', 'generic_group',
                       'generic_unit', 'translation_resource'
                   )",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 0);
    }

    #[test]
    fn init_rejects_source_that_contains_write_back_root() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = source.join("project");

        let error = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "game".parse().unwrap(),
            workspace_root: workspace.clone(),
            source_root: Some(source.canonicalize().unwrap()),
            source_language: Some(language("ja")),
            target_language: Some(language("zh-Hans")),
        })
        .expect_err("输入包含写回目录时应拒绝");

        assert!(matches!(
            error,
            GenericProjectError::SourceWriteBackOverlap { .. }
        ));
        assert!(!workspace.join(DATABASE_FILE_NAME).exists());
    }

    #[test]
    fn reinit_rejects_source_inside_write_back_and_preserves_project() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");
        let store = init(&workspace, &source);
        let original_source = store.open().unwrap().source_root().to_path_buf();
        let overlapping_source = workspace.join("write_back").join("input");
        fs::create_dir_all(&overlapping_source).unwrap();

        let error = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "game".parse().unwrap(),
            workspace_root: workspace.clone(),
            source_root: Some(overlapping_source),
            source_language: None,
            target_language: None,
        })
        .expect_err("输入位于写回目录内时应拒绝");

        assert!(matches!(
            error,
            GenericProjectError::SourceWriteBackOverlap { .. }
        ));
        assert_eq!(store.open().unwrap().source_root(), original_source);
    }

    #[test]
    fn init_allows_source_and_write_back_as_sibling_directories() {
        let temp = tempdir().unwrap();
        let workspace = temp.path().join("project");
        let source = workspace.join("input");
        fs::create_dir_all(&source).unwrap();

        let (_, project) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "game".parse().unwrap(),
            workspace_root: workspace,
            source_root: Some(source.clone()),
            source_language: Some(language("ja")),
            target_language: Some(language("zh-Hans")),
        })
        .expect("输入与写回目录为兄弟目录时应允许");

        assert_eq!(project.source_root(), source.canonicalize().unwrap());
    }

    #[test]
    fn first_init_rejects_identical_languages_without_creating_database() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");

        let error = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "game".parse().unwrap(),
            workspace_root: workspace.clone(),
            source_root: Some(source),
            source_language: Some(language("JA")),
            target_language: Some(language("ja")),
        })
        .expect_err("相同源语言和目标语言应拒绝");

        assert!(matches!(
            error,
            GenericProjectError::SameSourceAndTargetLanguage { .. }
        ));
        assert!(!workspace.join(DATABASE_FILE_NAME).exists());
    }

    #[test]
    fn reinit_rejects_identical_languages_without_changing_project() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");
        let store = init(&workspace, &source);

        let error = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "game".parse().unwrap(),
            workspace_root: workspace,
            source_root: None,
            source_language: Some(language("ja")),
            target_language: Some(language("ja")),
        })
        .expect_err("再次 Init 也应拒绝相同语言");

        assert!(matches!(
            error,
            GenericProjectError::SameSourceAndTargetLanguage { .. }
        ));
        assert_eq!(
            store.open().unwrap().language_pair(),
            &LanguagePair::new(language("ja"), language("zh-Hans"))
        );
    }

    #[test]
    fn open_rejects_database_with_identical_languages() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");
        let store = init(&workspace, &source);
        let connection = store.open_connection(false).unwrap();
        connection
            .execute(
                "UPDATE main.generic_project
                 SET target_language = source_language
                 WHERE singleton = 1",
                [],
            )
            .unwrap();
        drop(connection);

        assert!(matches!(
            store.open(),
            Err(GenericProjectError::SameSourceAndTargetLanguage { .. })
        ));
    }

    #[test]
    fn extract_preserves_moves_and_id_renames_but_clears_context_changes() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");
        write_source(
            &source,
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"a\",\"text\":\"甲\"},{\"id\":\"b\",\"text\":\"乙\"},{\"id\":\"c\",\"text\":\"丙\"}]}\n",
        );
        let store = init(&workspace, &source);
        store.extract().expect("首次 Extract 应成功");
        let snapshot = store.load_snapshot().unwrap();
        let group = &snapshot.files()[0].groups()[0];
        let writes = group
            .units()
            .iter()
            .map(|unit| TranslationWrite {
                group_id: group.id().to_owned(),
                unit_id: unit.id().to_owned(),
                expected_source_text: unit.source_text().to_owned(),
                expected_group_context: group.context_fingerprint(),
                translation: format!("译{}", unit.source_text()),
                origin: TranslationOrigin::Automatic,
                state_fingerprint: Sha256Fingerprint::from_bytes([7; 32]),
                expected_translation: None,
            })
            .collect::<Vec<_>>();
        store
            .commit_translations(
                snapshot.project().extracted_raw_fingerprint().unwrap(),
                &writes,
            )
            .unwrap();

        fs::rename(source.join("text.jsonl"), source.join("moved.jsonl")).unwrap();
        store.extract().expect("移动文件应成功");
        assert_eq!(
            store.load_snapshot().unwrap().files()[0].groups()[0]
                .units()
                .iter()
                .filter(|unit| unit.translation().is_some())
                .count(),
            3
        );

        fs::write(
            source.join("moved.jsonl"),
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"renamed\",\"text\":\"甲\"},{\"id\":\"b\",\"text\":\"乙\"},{\"id\":\"c\",\"text\":\"丙\"}]}\n",
        )
        .unwrap();
        store.extract().expect("只改 Unit ID 应成功");
        let units = store.load_snapshot().unwrap().files()[0].groups()[0]
            .units()
            .to_vec();
        assert!(units[0].translation().is_none());
        assert!(units[1].translation().is_some());
        assert!(units[2].translation().is_some());

        fs::write(
            source.join("moved.jsonl"),
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"renamed-again\",\"text\":\"甲\"},{\"id\":\"b-renamed\",\"text\":\"乙\"},{\"id\":\"c\",\"text\":\"丙\"}]}\n",
        )
        .unwrap();
        store.extract().expect("同组多个 Unit 只改 ID 应成功");
        let units = store.load_snapshot().unwrap().files()[0].groups()[0]
            .units()
            .to_vec();
        assert!(units[0].translation().is_none());
        assert!(units[1].translation().is_none());
        assert!(units[2].translation().is_some());

        fs::write(
            source.join("moved.jsonl"),
            "{\"id\":\"g\",\"kind\":\"name\",\"units\":[{\"id\":\"renamed-again\",\"text\":\"甲\"},{\"id\":\"b-renamed\",\"text\":\"乙\"},{\"id\":\"c\",\"text\":\"丙\"}]}\n",
        )
        .unwrap();
        store.extract().expect("kind 修改应成功");
        assert!(
            store.load_snapshot().unwrap().files()[0].groups()[0]
                .units()
                .iter()
                .all(|unit| unit.translation().is_none())
        );
    }

    #[test]
    fn load_snapshot_preserves_file_group_and_unit_natural_order() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::write(
            source.join("a.jsonl"),
            concat!(
                "{\"id\":\"group-z\",\"kind\":\"dialogue\",\"units\":[",
                "{\"id\":\"unit-z\",\"text\":\"甲\"},{\"id\":\"unit-a\",\"text\":\"乙\"}]}\n",
                "{\"id\":\"group-a\",\"kind\":\"name\",\"units\":[",
                "{\"id\":\"unit-only\",\"text\":\"丙\"}]}\n"
            ),
        )
        .unwrap();
        fs::write(
            source.join("nested").join("b.jsonl"),
            "{\"id\":\"group-nested\",\"kind\":\"description\",\"units\":[{\"id\":\"unit-nested\",\"text\":\"丁\"}]}\n",
        )
        .unwrap();
        fs::write(source.join("z-empty.jsonl"), "").unwrap();
        let workspace = temp.path().join("project");
        let store = init(&workspace, &source);

        store.extract().expect("多文件输入应该可提取");
        let snapshot = store.load_snapshot().expect("多层快照应该可读取");

        assert_eq!(
            snapshot
                .files()
                .iter()
                .map(|file| file.relative_path().to_path_buf())
                .collect::<Vec<_>>(),
            [
                PathBuf::from("a.jsonl"),
                PathBuf::from("nested").join("b.jsonl"),
                PathBuf::from("z-empty.jsonl"),
            ]
        );
        assert_eq!(
            snapshot.files()[0]
                .groups()
                .iter()
                .map(GenericStoredGroup::id)
                .collect::<Vec<_>>(),
            ["group-z", "group-a"]
        );
        assert_eq!(
            snapshot.files()[0].groups()[0]
                .units()
                .iter()
                .map(GenericStoredUnit::id)
                .collect::<Vec<_>>(),
            ["unit-z", "unit-a"]
        );
        assert_eq!(
            snapshot.files()[1].groups()[0].units()[0].source_text(),
            "丁"
        );
        assert!(snapshot.files()[2].groups().is_empty());
    }

    #[test]
    fn unit_snapshot_query_uses_natural_order_indexes_without_a_temp_sort() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");
        write_source(
            &source,
            "{\"id\":\"g\",\"kind\":\"k\",\"units\":[{\"id\":\"u\",\"text\":\"原文\"}]}\n",
        );
        let store = init(&workspace, &source);
        store.extract().unwrap();
        let connection = store.open_connection(false).unwrap();
        let mut statement = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {LOAD_UNITS_NATURAL_SQL}"))
            .unwrap();
        let plan = statement
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert!(
            plan.iter().all(|step| !step.contains("TEMP B-TREE")),
            "Unit 快照读取不应建立临时排序树：{plan:?}"
        );
        assert!(plan.iter().any(|step| step.starts_with("SCAN f")));
        assert!(plan.iter().any(|step| step.starts_with("SEARCH g")));
        assert!(plan.iter().any(|step| step.starts_with("SEARCH u")));
    }

    #[test]
    fn live_changes_require_an_explicit_extract() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");
        write_source(
            &source,
            "{\"id\":\"g\",\"kind\":\"k\",\"units\":[{\"id\":\"u\",\"text\":\"old\"}]}\n",
        );
        let store = init(&workspace, &source);
        store.extract().unwrap();
        assert!(store.ensure_input_current().is_ok());

        write_source(
            &source,
            "{\"id\":\"g\",\"kind\":\"k\",\"units\":[{\"id\":\"u\",\"text\":\"new\"}]}\n",
        );
        assert!(matches!(
            store.ensure_input_current(),
            Err(GenericProjectError::ExtractRequired)
        ));
    }

    #[test]
    fn publish_recheck_compares_live_raw_and_asset_fingerprints() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");
        let original =
            "{\"id\":\"g\",\"kind\":\"k\",\"units\":[{\"id\":\"u\",\"text\":\"old\"}]}\n";
        write_source(&source, original);
        let store = init(&workspace, &source);
        store.extract().unwrap();
        let project = store.open().unwrap();
        ensure_input_fingerprints_current(&project).expect("未变化的输入应该通过发布前复查");

        write_source(
            &source,
            "{\"id\":\"g\",\"kind\":\"k\",\"units\":[{\"id\":\"u\",\"text\":\"new\"}]}\n",
        );
        assert!(matches!(
            ensure_input_fingerprints_current(&project),
            Err(GenericProjectError::ExtractRequired)
        ));

        write_source(&source, original);
        let mut wrong_asset = project;
        wrong_asset.extracted_asset_fingerprint = Some(Sha256Fingerprint::from_bytes([0_u8; 32]));
        assert!(matches!(
            ensure_input_fingerprints_current(&wrong_asset),
            Err(GenericProjectError::ExtractRequired)
        ));
    }

    #[test]
    fn successful_translation_write_remembers_profile_in_the_same_transaction() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");
        write_source(
            &source,
            "{\"id\":\"g\",\"kind\":\"k\",\"units\":[{\"id\":\"u\",\"text\":\"原文\"}]}\n",
        );
        let store = init(&workspace, &source);
        store.extract().unwrap();
        let snapshot = store.load_snapshot().unwrap();
        let group = &snapshot.files()[0].groups()[0];
        let unit = &group.units()[0];
        let write = TranslationWrite {
            group_id: group.id().to_owned(),
            unit_id: unit.id().to_owned(),
            expected_source_text: unit.source_text().to_owned(),
            expected_group_context: group.context_fingerprint(),
            translation: "译文".to_owned(),
            origin: TranslationOrigin::Automatic,
            state_fingerprint: Sha256Fingerprint::from_bytes([42; 32]),
            expected_translation: None,
        };

        let outcome = store
            .commit_translations_for_profile(
                snapshot.project().extracted_raw_fingerprint().unwrap(),
                std::slice::from_ref(&write),
                "primary",
            )
            .unwrap();
        assert_eq!(outcome.committed, 1);
        assert_eq!(store.open().unwrap().last_profile_id(), Some("primary"));

        let conflict = store
            .commit_translations_for_profile(
                snapshot.project().extracted_raw_fingerprint().unwrap(),
                &[TranslationWrite {
                    translation: "另一译文".to_owned(),
                    ..write
                }],
                "secondary",
            )
            .unwrap();
        assert_eq!(conflict.committed, 0);
        assert_eq!(conflict.conflicts.len(), 1);
        assert_eq!(store.open().unwrap().last_profile_id(), Some("primary"));
    }

    #[test]
    fn batch_translation_commit_preserves_per_unit_cas_and_conflict_order() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");
        write_source(
            &source,
            concat!(
                "{\"id\":\"g\",\"kind\":\"k\",\"units\":[",
                "{\"id\":\"a\",\"text\":\"甲\"},",
                "{\"id\":\"b\",\"text\":\"乙\"},",
                "{\"id\":\"c\",\"text\":\"丙\"}]}\n"
            ),
        );
        let store = init(&workspace, &source);
        store.extract().unwrap();
        let snapshot = store.load_snapshot().unwrap();
        let group = &snapshot.files()[0].groups()[0];
        let state = Sha256Fingerprint::from_bytes([42; 32]);
        let mut writes = group
            .units()
            .iter()
            .map(|unit| TranslationWrite {
                group_id: group.id().to_owned(),
                unit_id: unit.id().to_owned(),
                expected_source_text: unit.source_text().to_owned(),
                expected_group_context: group.context_fingerprint(),
                translation: format!("译文-{}", unit.id()),
                origin: TranslationOrigin::Automatic,
                state_fingerprint: state,
                expected_translation: None,
            })
            .collect::<Vec<_>>();
        writes[1].expected_source_text = "错误原文".to_owned();
        writes[2].expected_group_context = Sha256Fingerprint::from_bytes([7; 32]);

        let outcome = store
            .commit_translations(
                snapshot.project().extracted_raw_fingerprint().unwrap(),
                &writes,
            )
            .unwrap();
        assert_eq!(outcome.committed, 1);
        assert_eq!(
            outcome.conflicts,
            [
                ("g".to_owned(), "b".to_owned()),
                ("g".to_owned(), "c".to_owned())
            ],
            "冲突必须保持调用方提供的自然顺序"
        );

        let snapshot = store.load_snapshot().unwrap();
        let group = &snapshot.files()[0].groups()[0];
        assert!(group.units()[0].translation().is_some());
        assert!(group.units()[1].translation().is_none());
        assert!(group.units()[2].translation().is_none());
        let previous = group.units()[0].translation().unwrap().clone();
        let update = TranslationWrite {
            group_id: group.id().to_owned(),
            unit_id: group.units()[0].id().to_owned(),
            expected_source_text: group.units()[0].source_text().to_owned(),
            expected_group_context: group.context_fingerprint(),
            translation: "人工并发修改后的新正文".to_owned(),
            origin: TranslationOrigin::Automatic,
            state_fingerprint: Sha256Fingerprint::from_bytes([43; 32]),
            expected_translation: Some(previous),
        };
        let outcome = store
            .commit_translations(
                snapshot.project().extracted_raw_fingerprint().unwrap(),
                &[update],
            )
            .unwrap();
        assert_eq!(outcome.committed, 1);
        assert!(outcome.conflicts.is_empty());
    }

    #[test]
    fn reinit_preserves_the_last_successful_profile() {
        let temp = tempdir().unwrap();
        let first_source = temp.path().join("source-a");
        let second_source = temp.path().join("source-b");
        fs::create_dir(&first_source).unwrap();
        fs::create_dir(&second_source).unwrap();
        let workspace = temp.path().join("project");
        let store = init(&workspace, &first_source);
        store.remember_profile("primary").unwrap();

        GenericProjectStore::initialize(GenericInitRequest {
            project_name: "game".parse().unwrap(),
            workspace_root: workspace.clone(),
            source_root: Some(second_source),
            source_language: None,
            target_language: None,
        })
        .expect("改变输入根应成功");
        let after_source_change = store.open().unwrap();
        assert_eq!(after_source_change.last_profile_id(), Some("primary"));
        assert_eq!(after_source_change.extracted_raw_fingerprint(), None);

        GenericProjectStore::initialize(GenericInitRequest {
            project_name: "game".parse().unwrap(),
            workspace_root: workspace,
            source_root: None,
            source_language: None,
            target_language: Some(language("zh-Hant")),
        })
        .expect("改变语言应成功");
        assert_eq!(store.open().unwrap().last_profile_id(), Some("primary"));
    }

    #[test]
    fn extract_treats_reordered_equal_text_units_as_a_group_change() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");
        write_source(
            &source,
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"a\",\"text\":\"同文\"},{\"id\":\"b\",\"text\":\"同文\"},{\"id\":\"c\",\"text\":\"未移动\"}]}\n",
        );
        let store = init(&workspace, &source);
        store.extract().expect("首次 Extract 应成功");
        let snapshot = store.load_snapshot().expect("应该可读取首次快照");
        let group = &snapshot.files()[0].groups()[0];
        let writes = group
            .units()
            .iter()
            .map(|unit| TranslationWrite {
                group_id: group.id().to_owned(),
                unit_id: unit.id().to_owned(),
                expected_source_text: unit.source_text().to_owned(),
                expected_group_context: group.context_fingerprint(),
                translation: format!("译文-{}", unit.id()),
                origin: TranslationOrigin::Automatic,
                state_fingerprint: Sha256Fingerprint::from_bytes([8; 32]),
                expected_translation: None,
            })
            .collect::<Vec<_>>();
        store
            .commit_translations(
                snapshot.project().extracted_raw_fingerprint().unwrap(),
                &writes,
            )
            .expect("测试译文应该可提交");

        write_source(
            &source,
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"b\",\"text\":\"同文\"},{\"id\":\"a\",\"text\":\"同文\"},{\"id\":\"c\",\"text\":\"未移动\"}]}\n",
        );
        store.extract().expect("重排后的输入应该可重新提取");

        assert!(
            store.load_snapshot().unwrap().files()[0].groups()[0]
                .units()
                .iter()
                .all(|unit| unit.translation().is_none()),
            "即使原文相同，Unit 顺序改变也必须清除整个 Group 的译文"
        );
    }

    #[test]
    fn applying_new_resources_clears_confirmed_stale_translations_in_the_same_transaction() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");
        write_source(
            &source,
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"u\",\"text\":\"原文\"}]}\n",
        );
        let store = init(&workspace, &source);
        store.extract().expect("首次 Extract 应成功");
        let snapshot = store.load_snapshot().expect("应该可读取首次快照");
        let group = &snapshot.files()[0].groups()[0];
        let unit = &group.units()[0];
        let previous = GenericStoredTranslation {
            translation: "旧译文".to_owned(),
            origin: TranslationOrigin::Automatic,
            state_fingerprint: Sha256Fingerprint::from_bytes([7; 32]),
        };
        store
            .commit_translations(
                snapshot.project().extracted_raw_fingerprint().unwrap(),
                &[TranslationWrite {
                    group_id: group.id().to_owned(),
                    unit_id: unit.id().to_owned(),
                    expected_source_text: unit.source_text().to_owned(),
                    expected_group_context: group.context_fingerprint(),
                    translation: previous.translation.clone(),
                    origin: previous.origin,
                    state_fingerprint: previous.state_fingerprint,
                    expected_translation: None,
                }],
            )
            .expect("旧译文应该可提交");

        let outcome = store
            .apply_translation_resources(
                snapshot.project().extracted_raw_fingerprint().unwrap(),
                r#"[{"term":"原文","translation":"新术语","triggers":["原文"]}]"#,
                "[]",
                &[TranslationClear {
                    group_id: group.id().to_owned(),
                    unit_id: unit.id().to_owned(),
                    expected_source_text: unit.source_text().to_owned(),
                    expected_group_context: group.context_fingerprint(),
                    expected_translation: previous,
                }],
            )
            .expect("资源和失效译文应该可原子更新");

        assert_eq!(outcome.committed, 1);
        assert!(outcome.conflicts.is_empty());
        assert!(
            store.load_snapshot().unwrap().files()[0].groups()[0].units()[0]
                .translation()
                .is_none(),
            "失效旧译文不得继续被 WriteBack 当成 Current"
        );
        let resources = store
            .load_translation_resources()
            .expect("应该可读取新资源");
        assert!(resources.terminology_json().contains("新术语"));
        assert_eq!(resources.placeholder_rules_json(), "[]");
    }
}
