//! Generic 项目状态、事务边界与重复 Extract。

use super::jsonl::{GenericInputSnapshot, scan_input_tree_with_cancellation};
use super::translate::{GenericUnitKey, GenericUnitMap, readable_generic_unit_id};
use crate::diagnostic::{
    FileSystemOperation, GenericProjectDatabaseProblem, GenericProjectTranslationProblem,
};
use crate::execution::CooperativeCancellation;
use crate::fingerprint::{SHA256_FINGERPRINT_BYTES, Sha256Fingerprint};
use crate::language::{LanguageId, LanguagePair};
use crate::project_name::ProjectName;
use crate::runtime::performance::{RunPerformanceCounters, SqliteTransactionScope};
use crate::runtime::sqlite::AttSqliteCancellableConnection;
use crate::runtime::windows::{
    FileIdentity, create_new_pinned_database_file, pin_directory_without_reparse,
};
use crate::translation::TranslationOrigin;
use crate::translation::candidate_validation::ProvenInvariantViolation;
use crate::translation::layout_rules::LayoutRuleSet;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, Row, params};
use std::collections::HashSet;
use std::ffi::OsString;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fmt, fs};

mod error;
mod initialization;
mod resources;
mod schema;
mod snapshot;
mod transaction;

pub(crate) use error::GenericProjectError;
use error::{
    invalid_database, project_optional_safe_identifier, project_safe_identifier,
    sqlite_operation_error,
};
use initialization::{
    cleanup_initial_database_candidate, ensure_initial_database_path_has_no_sidecars,
    initial_database_file_system_error, observe_initial_database_sidecars,
    publish_initial_database_candidate, resolve_source_root, validate_distinct_languages,
    validate_source_write_back_separation,
};
use resources::{
    GenericCompiledTranslationResources, load_translation_resource_row_with_cancellation,
    validate_translation_resources_with_cancellation,
};
#[cfg(test)]
use resources::{TranslationResources, load_translation_resources_rows_with_cancellation};
pub(crate) use schema::validate_current_generic_schema_with_cancellation;
#[cfg(test)]
pub(crate) use schema::{create_current_generic_schema_for_test, validate_current_generic_schema};
use schema::{create_initial_schema, validate_project_database_with_cancellation};
use transaction::{open_sqlite_connection, run_cancellable_transaction};

#[cfg(test)]
pub(crate) use snapshot::ensure_input_fingerprints_current;
pub(crate) use snapshot::ensure_input_fingerprints_current_with_cancellation;
use snapshot::{load_snapshot_rows, reconcile_snapshot, replace_snapshot, scan_current_input};

const DATABASE_FILE_NAME: &str = "project.db";
const FINGERPRINT_CANCELLATION_CHECK_BYTES: NonZeroUsize =
    NonZeroUsize::new(64 * 1024).expect("Generic 指纹取消检查块大小必须非零");
const RESOURCE_CANCELLATION_CHECK_BYTES: usize = 64 * 1024;
const SQLITE_SIDECAR_SUFFIXES: [&str; 3] = ["-journal", "-wal", "-shm"];
const TERMINOLOGY_RESOURCE: &str = "terminology";
const PLACEHOLDER_RULES_RESOURCE: &str = "placeholder_rules";
const LAYOUT_RULES_RESOURCE: &str = "write_back_layout_rules";
const CREATE_PENDING_TRANSLATION_COMMIT_SQL: &str = "
    CREATE TEMP TABLE pending_translation_commit (
        group_id TEXT NOT NULL,
        unit_id TEXT NOT NULL,
        expected_source_text TEXT NOT NULL,
        expected_group_context BLOB NOT NULL,
        translation TEXT NOT NULL,
        translation_state BLOB NOT NULL,
        expected_translation TEXT,
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
        translation_state,
        expected_translation,
        expected_translation_state
    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
";
const APPLY_PENDING_TRANSLATION_COMMIT_SQL: &str = "
    UPDATE main.generic_unit AS unit
    SET translation = pending.translation,
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
              AND unit.translation_state IS NULL
          )
          OR
          (
              unit.translation = pending.expected_translation
              AND unit.translation_state = pending.expected_translation_state
          )
      )
    RETURNING group_id, unit_id
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

/// 最近一次只因可证明不变量而未能入库的候选译文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GenericStoredRejectedTranslation {
    pub(super) readable_id: String,
    pub(super) origin: TranslationOrigin,
    pub(super) source: Vec<String>,
    pub(super) candidate_json: String,
    pub(super) translation: Option<Vec<String>>,
    pub(super) group_context: Sha256Fingerprint,
    pub(super) violation: ProvenInvariantViolation,
    pub(super) planning_state: Sha256Fingerprint,
}

#[cfg(test)]
impl GenericStoredRejectedTranslation {
    pub(crate) fn translation(&self) -> Option<&[String]> {
        self.translation.as_deref()
    }

    pub(crate) fn violation(&self) -> &ProvenInvariantViolation {
        &self.violation
    }

    pub(crate) fn readable_id(&self) -> &str {
        &self.readable_id
    }

    pub(crate) const fn origin(&self) -> TranslationOrigin {
        self.origin
    }
}

/// 持久化中的一个 Unit。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GenericStoredUnit {
    pub(super) id: String,
    pub(super) ordinal: usize,
    pub(super) source_text: String,
    pub(super) translation: Option<GenericStoredTranslation>,
    pub(super) rejected: Option<GenericStoredRejectedTranslation>,
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

    pub(crate) fn rejected(&self) -> Option<&GenericStoredRejectedTranslation> {
        self.rejected.as_ref()
    }

    pub(crate) const fn ordinal(&self) -> usize {
        self.ordinal
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

    pub(crate) const fn ordinal(&self) -> usize {
        self.ordinal
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

    pub(crate) fn natural_unit_ids(&self) -> HashSet<String> {
        let mut ids = HashSet::new();
        for file in &self.files {
            for (line, group) in file.groups.iter().enumerate() {
                for (unit, _) in group.units.iter().enumerate() {
                    ids.insert(readable_generic_unit_id(
                        &file.relative_path,
                        line + 1,
                        unit + 1,
                    ));
                }
            }
        }
        ids
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

/// 一个经过上游验收、准备原子提交的 Unit 译文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationWrite {
    pub(crate) group_id: String,
    pub(crate) unit_id: String,
    pub(crate) expected_source_text: String,
    pub(crate) expected_group_context: Sha256Fingerprint,
    pub(crate) translation: String,
    pub(crate) state_fingerprint: Sha256Fingerprint,
    pub(crate) expected_translation: Option<GenericStoredTranslation>,
    /// 提交基线中该 Unit 是否是当前 Rejected。
    pub(crate) was_current_rejected: bool,
}

/// 一个已绑定到当前 Generic Unit、准备原子替换的硬拒绝候选。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RejectedTranslationWrite {
    pub(crate) group_id: String,
    pub(crate) unit_id: String,
    pub(crate) readable_id: String,
    pub(crate) origin: TranslationOrigin,
    pub(crate) expected_source_text: String,
    pub(crate) source: Vec<String>,
    pub(crate) expected_group_context: Sha256Fingerprint,
    pub(crate) expected_manual_applicability: Sha256Fingerprint,
    pub(crate) candidate_json: String,
    pub(crate) translation: Option<Vec<String>>,
    pub(crate) violation: ProvenInvariantViolation,
    pub(crate) planning_state: Sha256Fingerprint,
    pub(crate) expected_translation: Option<GenericStoredTranslation>,
    /// 提交基线中该 Unit 是否已经是当前 Rejected。
    pub(crate) was_current_rejected: bool,
}

/// 一条已经证明违反当前强不变量、准备以 CAS 转入 Rejected 的旧译文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationClear {
    pub(crate) group_id: String,
    pub(crate) unit_id: String,
    pub(crate) readable_id: String,
    pub(crate) expected_source_text: String,
    pub(crate) expected_group_context: Sha256Fingerprint,
    pub(crate) expected_translation: GenericStoredTranslation,
    pub(crate) violation: ProvenInvariantViolation,
    pub(crate) rejection_planning_state: Sha256Fingerprint,
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
    pub(crate) resolved_rejected: usize,
    pub(crate) conflicts: Vec<(String, String)>,
}

/// 一次模型任务将有效译文和硬拒绝候选共同提交后的结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommitTranslationResultsOutcome {
    pub(crate) committed: usize,
    pub(crate) rejected: usize,
    pub(crate) resolved_rejected: usize,
    pub(crate) newly_rejected: usize,
    pub(crate) conflicts: Vec<(String, String)>,
}

impl CommitTranslationResultsOutcome {
    fn translations_only(self) -> CommitTranslationsOutcome {
        CommitTranslationsOutcome {
            committed: self.committed,
            resolved_rejected: self.resolved_rejected,
            conflicts: self.conflicts,
        }
    }
}

/// Generic 项目数据库的直接领域入口。
#[derive(Clone)]
pub(crate) struct GenericProjectStore {
    workspace_root: PathBuf,
    database_path: PathBuf,
    cancellation: CooperativeCancellation,
    performance: Arc<RunPerformanceCounters>,
}

impl fmt::Debug for GenericProjectStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GenericProjectStore")
            .field("workspace_root", &self.workspace_root)
            .field("database_path", &self.database_path)
            .field("cancelled", &self.cancellation.is_requested())
            .finish()
    }
}

impl GenericProjectStore {
    #[cfg(test)]
    pub(crate) fn for_workspace(workspace_root: PathBuf) -> Self {
        Self::for_workspace_with_cancellation(
            workspace_root,
            CooperativeCancellation::default(),
            Arc::new(RunPerformanceCounters::default()),
        )
    }

    pub(crate) fn for_workspace_with_cancellation(
        workspace_root: PathBuf,
        cancellation: CooperativeCancellation,
        performance: Arc<RunPerformanceCounters>,
    ) -> Self {
        let database_path = workspace_root.join(DATABASE_FILE_NAME);
        Self {
            workspace_root,
            database_path,
            cancellation,
            performance,
        }
    }

    pub(crate) fn database_path(&self) -> &Path {
        &self.database_path
    }

    #[cfg(test)]
    pub(crate) fn initialize(
        request: GenericInitRequest,
    ) -> Result<(Self, GenericProject), GenericProjectError> {
        Self::initialize_with_cancellation(
            request,
            CooperativeCancellation::default(),
            Arc::new(RunPerformanceCounters::default()),
        )
    }

    pub(crate) fn initialize_with_cancellation(
        request: GenericInitRequest,
        cancellation: CooperativeCancellation,
        performance: Arc<RunPerformanceCounters>,
    ) -> Result<(Self, GenericProject), GenericProjectError> {
        if request.workspace_root.exists() && !request.workspace_root.is_dir() {
            return Err(GenericProjectError::WorkspaceNotDirectory {
                path: request.workspace_root,
            });
        }
        let store = Self::for_workspace_with_cancellation(
            request.workspace_root.clone(),
            cancellation,
            performance,
        );
        let project = store.finish_cancellable(store.initialize_inner(request))?;
        Ok((store, project))
    }

    fn initialize_inner(
        &self,
        request: GenericInitRequest,
    ) -> Result<GenericProject, GenericProjectError> {
        self.ensure_not_cancelled()?;
        let exists = self.database_path.is_file();

        if exists {
            let mut connection = self.open_connection(false)?;
            validate_project_database_with_cancellation(&connection, &self.cancellation)?;
            let current = self.read_project_with_connection(&connection)?;
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
            let language_changed = source_language != *current.language_pair.source()
                || target_language != *current.language_pair.target();

            run_cancellable_transaction(
                &mut connection,
                &self.cancellation,
                self.performance.as_ref(),
                SqliteTransactionScope::DatabaseInitialization,
                "开始 Generic Init 事务",
                "提交 Generic Init",
                "回滚 Generic Init",
                |transaction| {
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
                    if language_changed {
                        transaction
                            .execute(
                                "UPDATE translation_resource
                                 SET canonical_json = '[]'
                                 WHERE resource_kind = ?1",
                                [TERMINOLOGY_RESOURCE],
                            )
                            .map_err(|source| GenericProjectError::Sqlite {
                                operation: "清空旧语言对的 Generic 术语",
                                source,
                            })?;
                    }
                    Ok(())
                },
            )?;
            Ok(GenericProject {
                project_name: current.project_name,
                workspace_root: self.workspace_root.clone(),
                database_path: self.database_path.clone(),
                source_root,
                language_pair: LanguagePair::new(source_language, target_language),
                extracted_raw_fingerprint: current.extracted_raw_fingerprint,
                extracted_asset_fingerprint: current.extracted_asset_fingerprint,
                last_profile_id: current.last_profile_id,
            })
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
                    operation: FileSystemOperation::Create,
                    path: request.workspace_root.clone(),
                    source,
                }
            })?;
            self.create_initial_database(
                &request.project_name,
                &source_root,
                &source_language,
                &target_language,
            )?;
            Ok(GenericProject {
                project_name: request.project_name,
                workspace_root: self.workspace_root.clone(),
                database_path: self.database_path.clone(),
                source_root,
                language_pair: LanguagePair::new(source_language, target_language),
                extracted_raw_fingerprint: None,
                extracted_asset_fingerprint: None,
                last_profile_id: None,
            })
        }
    }

    fn create_initial_database(
        &self,
        project_name: &ProjectName,
        source_root: &Path,
        source_language: &LanguageId,
        target_language: &LanguageId,
    ) -> Result<(), GenericProjectError> {
        self.ensure_not_cancelled()?;
        ensure_initial_database_path_has_no_sidecars(
            &self.database_path,
            FileSystemOperation::Metadata,
            "首次 Init 的发布目标旁存在不属于当前项目的 SQLite sidecar",
            &self.cancellation,
        )?;
        let parent = pin_directory_without_reparse(&self.workspace_root).map_err(|source| {
            initial_database_file_system_error(FileSystemOperation::Open, source)
        })?;
        let candidate_path = parent
            .resolved_path()
            .join(format!(".{DATABASE_FILE_NAME}.init.tmp"));
        ensure_initial_database_path_has_no_sidecars(
            &candidate_path,
            FileSystemOperation::Metadata,
            "首次 Init 的候选路径旁存在需要保留的 SQLite sidecar",
            &self.cancellation,
        )?;
        let file = create_new_pinned_database_file(&candidate_path).map_err(|source| {
            initial_database_file_system_error(FileSystemOperation::Create, source)
        })?;
        let identity = FileIdentity::of(&file, &candidate_path).map_err(|source| {
            GenericProjectError::InitialDatabaseOutcomeUnknown(Box::new(
                initial_database_file_system_error(FileSystemOperation::Metadata, source),
            ))
        })?;
        let mut cleanup_targets = vec![(candidate_path.clone(), identity)];

        let build_result = (|| {
            let opened = open_sqlite_connection(&candidate_path, true, self.cancellation.clone());
            let mut connection = match opened {
                Ok(connection) => connection,
                Err(source) => {
                    observe_initial_database_sidecars(&candidate_path, &mut cleanup_targets)?;
                    return Err(source);
                }
            };
            let initialized = create_initial_schema(
                &mut connection,
                project_name,
                source_root,
                source_language,
                target_language,
                &self.cancellation,
                self.performance.as_ref(),
            )
            .and_then(|()| {
                validate_project_database_with_cancellation(&connection, &self.cancellation)
            });
            // 连接仍持有本次 SQLite sidecar 时记录身份；失败清理不会按名称接管后来出现的文件。
            observe_initial_database_sidecars(&candidate_path, &mut cleanup_targets)?;
            initialized?;
            publish_initial_database_candidate(
                connection,
                file,
                identity,
                &candidate_path,
                &self.database_path,
                &self.cancellation,
            )
        })();
        match build_result {
            Ok(()) => Ok(()),
            Err(original @ GenericProjectError::InitialDatabaseOutcomeUnknown(_)) => Err(original),
            Err(original) => match cleanup_initial_database_candidate(&cleanup_targets) {
                Ok(()) => Err(original),
                Err(cleanup) => Err(GenericProjectError::InitialCandidateCleanup {
                    original: Box::new(original),
                    cleanup,
                }),
            },
        }
    }

    pub(crate) fn open(&self) -> Result<GenericProject, GenericProjectError> {
        self.finish_cancellable(self.open_inner())
    }

    fn open_inner(&self) -> Result<GenericProject, GenericProjectError> {
        self.ensure_not_cancelled()?;
        let connection = self.open_connection(false)?;
        validate_project_database_with_cancellation(&connection, &self.cancellation)?;
        let project = self.read_project_with_connection(&connection)?;
        self.ensure_not_cancelled()?;
        Ok(project)
    }

    pub(crate) fn extract(&self) -> Result<ExtractOutcome, GenericProjectError> {
        self.finish_cancellable(self.extract_inner())
    }

    fn extract_inner(&self) -> Result<ExtractOutcome, GenericProjectError> {
        let project = self.open()?;
        let scanned = scan_input_tree_with_cancellation(project.source_root(), &self.cancellation)?;
        if project.extracted_raw_fingerprint() == Some(scanned.raw_fingerprint())
            && project.extracted_asset_fingerprint() == Some(scanned.asset_fingerprint())
        {
            let observed_again =
                scan_input_tree_with_cancellation(project.source_root(), &self.cancellation)?;
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
        let reconciled = run_cancellable_transaction(
            &mut connection,
            &self.cancellation,
            self.performance.as_ref(),
            SqliteTransactionScope::WritePlan,
            "开始 Generic Extract 事务",
            "提交 Generic Extract",
            "回滚 Generic Extract",
            |transaction| {
                let previous = load_snapshot_rows(transaction, &project, &self.cancellation)?;
                let reconciled = reconcile_snapshot(&previous, &scanned, &self.cancellation)?;
                replace_snapshot(transaction, &scanned, &reconciled.files, &self.cancellation)?;

                let observed_again =
                    scan_input_tree_with_cancellation(project.source_root(), &self.cancellation)?;
                if observed_again.raw_fingerprint() != scanned.raw_fingerprint() {
                    return Err(GenericProjectError::InputChangedDuringExtract);
                }
                Ok(reconciled)
            },
        )?;

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
        self.finish_cancellable(self.load_snapshot_inner())
    }

    #[cfg(test)]
    fn load_snapshot_inner(&self) -> Result<GenericStoredSnapshot, GenericProjectError> {
        let connection = self.open_connection(false)?;
        validate_project_database_with_cancellation(&connection, &self.cancellation)?;
        let project = self.read_project_with_connection(&connection)?;
        if project.extracted_raw_fingerprint().is_none() {
            return Err(GenericProjectError::ExtractRequired);
        }
        load_snapshot_rows(&connection, &project, &self.cancellation)
    }

    /// 重新扫描外部输入，并只在内容仍与最近一次 Extract 相同时返回快照。
    #[cfg(test)]
    pub(crate) fn ensure_input_current(
        &self,
    ) -> Result<(GenericStoredSnapshot, GenericInputSnapshot), GenericProjectError> {
        let stored = self.load_snapshot()?;
        let live = scan_current_input(&stored, &self.cancellation)?;
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
            GenericCompiledTranslationResources,
        ),
        GenericProjectError,
    > {
        self.finish_cancellable(self.load_current_translation_state_inner())
    }

    fn load_current_translation_state_inner(
        &self,
    ) -> Result<
        (
            GenericStoredSnapshot,
            GenericInputSnapshot,
            GenericCompiledTranslationResources,
        ),
        GenericProjectError,
    > {
        let connection = self.open_connection(false)?;
        let resources =
            validate_project_database_with_cancellation(&connection, &self.cancellation)?;
        let project = self.read_project_with_connection(&connection)?;
        if project.extracted_raw_fingerprint().is_none() {
            return Err(GenericProjectError::ExtractRequired);
        }
        let stored = load_snapshot_rows(&connection, &project, &self.cancellation)?;
        drop(connection);
        let live = scan_current_input(&stored, &self.cancellation)?;
        Ok((stored, live, resources))
    }

    #[cfg(test)]
    pub(crate) fn commit_translations(
        &self,
        expected_raw_fingerprint: Sha256Fingerprint,
        writes: &[TranslationWrite],
    ) -> Result<CommitTranslationsOutcome, GenericProjectError> {
        self.finish_cancellable(self.commit_translations_inner(
            expected_raw_fingerprint,
            writes,
            &[],
            None,
        ))
        .map(CommitTranslationResultsOutcome::translations_only)
    }

    /// 提交自动翻译进展，并在至少一项写入成功时于同一事务记录所用 Profile。
    pub(crate) fn commit_translations_for_profile(
        &self,
        expected_raw_fingerprint: Sha256Fingerprint,
        writes: &[TranslationWrite],
        profile_id: &str,
    ) -> Result<CommitTranslationsOutcome, GenericProjectError> {
        self.ensure_not_cancelled()?;
        if profile_id.is_empty() || profile_id.chars().all(char::is_whitespace) {
            return Err(GenericProjectError::BlankProfileId);
        }
        self.finish_cancellable(self.commit_translations_inner(
            expected_raw_fingerprint,
            writes,
            &[],
            Some(profile_id),
        ))
        .map(CommitTranslationResultsOutcome::translations_only)
    }

    /// 原子保存同一模型任务中的有效译文与可证明硬拒绝。
    pub(crate) fn commit_translation_results_for_profile(
        &self,
        expected_raw_fingerprint: Sha256Fingerprint,
        writes: &[TranslationWrite],
        rejections: &[RejectedTranslationWrite],
        profile_id: &str,
    ) -> Result<CommitTranslationResultsOutcome, GenericProjectError> {
        self.ensure_not_cancelled()?;
        if profile_id.is_empty() || profile_id.chars().all(char::is_whitespace) {
            return Err(GenericProjectError::BlankProfileId);
        }
        self.finish_cancellable(self.commit_translations_inner(
            expected_raw_fingerprint,
            writes,
            rejections,
            Some(profile_id),
        ))
    }

    fn commit_translations_inner(
        &self,
        expected_raw_fingerprint: Sha256Fingerprint,
        writes: &[TranslationWrite],
        rejections: &[RejectedTranslationWrite],
        profile_id: Option<&str>,
    ) -> Result<CommitTranslationResultsOutcome, GenericProjectError> {
        let mut write_indexes = GenericUnitMap::with_capacity(writes.len());
        for (index, write) in writes.iter().enumerate() {
            self.ensure_not_cancelled()?;
            validate_translation(&write.translation).map_err(|problem| {
                GenericProjectError::InvalidTranslation {
                    group_id: Some(write.group_id.clone()),
                    unit_id: Some(write.unit_id.clone()),
                    problem,
                    source: None,
                }
            })?;
            let key = GenericUnitKey::new(
                clone_text_with_cancellation(&write.group_id, &self.cancellation)?,
                clone_text_with_cancellation(&write.unit_id, &self.cancellation)?,
            );
            if write_indexes
                .insert_with_cancellation(key, index, || self.ensure_not_cancelled())?
                .is_some()
            {
                return Err(GenericProjectError::DuplicateTranslationWrite {
                    group_id: clone_text_with_cancellation(&write.group_id, &self.cancellation)?,
                    unit_id: clone_text_with_cancellation(&write.unit_id, &self.cancellation)?,
                });
            }
        }
        let mut rejection_indexes = GenericUnitMap::with_capacity(rejections.len());
        for (index, rejection) in rejections.iter().enumerate() {
            self.ensure_not_cancelled()?;
            if serde_json::from_str::<Box<serde_json::value::RawValue>>(&rejection.candidate_json)
                .is_err()
            {
                return Err(GenericProjectError::InvalidTranslation {
                    group_id: Some(rejection.group_id.clone()),
                    unit_id: Some(rejection.unit_id.clone()),
                    problem: GenericProjectTranslationProblem::Blank,
                    source: None,
                });
            }
            let key = GenericUnitKey::new(
                clone_text_with_cancellation(&rejection.group_id, &self.cancellation)?,
                clone_text_with_cancellation(&rejection.unit_id, &self.cancellation)?,
            );
            if write_indexes.contains_with_cancellation(&key, || self.ensure_not_cancelled())?
                || rejection_indexes
                    .insert_with_cancellation(key, index, || self.ensure_not_cancelled())?
                    .is_some()
            {
                return Err(GenericProjectError::DuplicateTranslationWrite {
                    group_id: clone_text_with_cancellation(
                        &rejection.group_id,
                        &self.cancellation,
                    )?,
                    unit_id: clone_text_with_cancellation(&rejection.unit_id, &self.cancellation)?,
                });
            }
        }

        let mut connection = self.open_connection(false)?;
        run_cancellable_transaction(
            &mut connection,
            &self.cancellation,
            self.performance.as_ref(),
            SqliteTransactionScope::WritePlan,
            "开始 Generic 译文提交事务",
            "完成 Generic 译文提交",
            "回滚 Generic 译文提交",
            |transaction| {
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
                        self.ensure_not_cancelled()?;
                        let (expected_translation, expected_state) = write
                            .expected_translation
                            .as_ref()
                            .map_or((None, None), |translation| {
                                (
                                    Some(translation.translation.as_str()),
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
                                write.state_fingerprint.as_bytes().as_slice(),
                                expected_translation,
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
                    let mut updated =
                        update
                            .query([])
                            .map_err(|source| GenericProjectError::Sqlite {
                                operation: "批量提交 Generic 译文",
                                source,
                            })?;
                    while let Some(row) =
                        updated
                            .next()
                            .map_err(|source| GenericProjectError::Sqlite {
                                operation: "读取 Generic 译文提交结果",
                                source,
                            })?
                    {
                        self.ensure_not_cancelled()?;
                        let group_id = clone_sqlite_text_column_with_cancellation(
                            row,
                            0,
                            "读取 Generic 译文提交结果",
                            &self.cancellation,
                        )?;
                        let unit_id = clone_sqlite_text_column_with_cancellation(
                            row,
                            1,
                            "读取 Generic 译文提交结果",
                            &self.cancellation,
                        )?;
                        let Some(&index) = write_indexes.get_parts_with_cancellation(
                            &group_id,
                            &unit_id,
                            || self.ensure_not_cancelled(),
                        )?
                        else {
                            return Err(invalid_database(
                                GenericProjectDatabaseProblem::UnexpectedCommittedUnit {
                                    group_id: project_optional_safe_identifier(&group_id),
                                    unit_id: project_optional_safe_identifier(&unit_id),
                                },
                            ));
                        };
                        if std::mem::replace(&mut applied[index], true) {
                            return Err(invalid_database(
                                GenericProjectDatabaseProblem::DuplicateCommittedUnit {
                                    group_id: project_optional_safe_identifier(&group_id),
                                    unit_id: project_optional_safe_identifier(&unit_id),
                                },
                            ));
                        }
                    }
                }
                let mut committed = 0;
                let mut resolved_rejected = 0;
                let mut conflicts = Vec::new();
                for (write, applied) in writes.iter().zip(applied) {
                    self.ensure_not_cancelled()?;
                    if applied {
                        committed += 1;
                        resolved_rejected += usize::from(write.was_current_rejected);
                        transaction
                            .execute(
                                "DELETE FROM generic_rejected_translation
                                 WHERE group_id = ?1 AND unit_id = ?2",
                                params![write.group_id, write.unit_id],
                            )
                            .map_err(|source| GenericProjectError::Sqlite {
                                operation: "清除 Generic 已修复的被拒候选",
                                source,
                            })?;
                    } else {
                        conflicts.push((
                            clone_text_with_cancellation(&write.group_id, &self.cancellation)?,
                            clone_text_with_cancellation(&write.unit_id, &self.cancellation)?,
                        ));
                    }
                }
                let mut rejected = 0_usize;
                let mut newly_rejected = 0_usize;
                for rejection in rejections {
                    self.ensure_not_cancelled()?;
                    let (expected_translation, expected_state) = rejection
                        .expected_translation
                        .as_ref()
                        .map_or((None, None), |translation| {
                            (
                                Some(translation.translation.as_str()),
                                Some(translation.state_fingerprint.as_bytes().as_slice()),
                            )
                        });
                    let current: i64 = transaction
                        .query_row(
                            "SELECT count(*)
                             FROM generic_unit AS unit
                             JOIN generic_group AS group_record
                               ON group_record.group_id = unit.group_id
                             WHERE unit.group_id = ?1
                               AND unit.unit_id = ?2
                               AND unit.source_text = ?3
                               AND group_record.context_fingerprint = ?4
                               AND (
                                   (
                                       ?6 IS NULL
                                       AND unit.translation IS NULL
                                       AND unit.translation_state IS NULL
                                   )
                                   OR
                                   (
                                       unit.translation = ?6
                                       AND unit.translation_state = ?7
                                   )
                               )
                               AND NOT EXISTS (
                                   SELECT 1 FROM generic_manual_translation AS manual
                                   WHERE manual.group_id = unit.group_id
                                     AND manual.unit_id = unit.unit_id
                                     AND manual.applicability_fingerprint = ?5
                               )",
                            params![
                                rejection.group_id,
                                rejection.unit_id,
                                rejection.expected_source_text,
                                rejection.expected_group_context.as_bytes().as_slice(),
                                rejection
                                    .expected_manual_applicability
                                    .as_bytes()
                                    .as_slice(),
                                expected_translation,
                                expected_state,
                            ],
                            |row| row.get(0),
                        )
                        .map_err(|source| GenericProjectError::Sqlite {
                            operation: "确认 Generic 被拒候选仍属于当前 Unit",
                            source,
                        })?;
                    if current != 1 {
                        conflicts.push((
                            clone_text_with_cancellation(&rejection.group_id, &self.cancellation)?,
                            clone_text_with_cancellation(&rejection.unit_id, &self.cancellation)?,
                        ));
                        continue;
                    }
                    transaction
                        .execute(
                            "INSERT INTO generic_rejected_translation (
                                 group_id, unit_id, readable_id, origin, source_json,
                                 candidate_json, translation_shape, group_context,
                                 violation_json, planning_state
                             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'free', ?7, ?8, ?9)
                             ON CONFLICT (group_id, unit_id) DO UPDATE SET
                                 readable_id = excluded.readable_id,
                                 origin = excluded.origin,
                                 source_json = excluded.source_json,
                                 candidate_json = excluded.candidate_json,
                                 translation_shape = excluded.translation_shape,
                                 group_context = excluded.group_context,
                                 violation_json = excluded.violation_json,
                                 planning_state = excluded.planning_state",
                            params![
                                rejection.group_id,
                                rejection.unit_id,
                                rejection.readable_id,
                                rejection.origin.storage_name(),
                                serde_json::to_string(&rejection.source)
                                    .expect("Generic 被拒候选原文必须可以编码"),
                                rejection.candidate_json,
                                rejection.expected_group_context.as_bytes().as_slice(),
                                serde_json::to_string(&rejection.violation)
                                    .expect("Generic 被拒原因必须可以编码"),
                                rejection.planning_state.as_bytes().as_slice(),
                            ],
                        )
                        .map_err(|source| GenericProjectError::Sqlite {
                            operation: "保存 Generic 被拒候选",
                            source,
                        })?;
                    rejected += 1;
                    newly_rejected += usize::from(!rejection.was_current_rejected);
                }
                if (committed > 0 || rejected > 0)
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
                Ok(CommitTranslationResultsOutcome {
                    committed,
                    rejected,
                    resolved_rejected,
                    newly_rejected,
                    conflicts,
                })
            },
        )
    }

    pub(crate) fn remember_profile(&self, profile_id: &str) -> Result<(), GenericProjectError> {
        self.ensure_not_cancelled()?;
        if profile_id.is_empty() || profile_id.chars().all(char::is_whitespace) {
            return Err(GenericProjectError::BlankProfileId);
        }
        self.finish_cancellable(self.remember_profile_inner(profile_id))
    }

    fn remember_profile_inner(&self, profile_id: &str) -> Result<(), GenericProjectError> {
        let mut connection = self.open_connection(false)?;
        run_cancellable_transaction(
            &mut connection,
            &self.cancellation,
            self.performance.as_ref(),
            SqliteTransactionScope::WritePlan,
            "开始保存 Generic 最近 Profile",
            "提交 Generic 最近 Profile",
            "回滚 Generic 最近 Profile",
            |transaction| {
                transaction
                    .execute(
                        "UPDATE generic_project SET last_profile_id = ?1 WHERE singleton = 1",
                        [profile_id],
                    )
                    .map_err(|source| GenericProjectError::Sqlite {
                        operation: "保存 Generic 最近 Profile",
                        source,
                    })?;
                Ok(())
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn load_translation_resources(
        &self,
    ) -> Result<TranslationResources, GenericProjectError> {
        self.finish_cancellable(self.load_translation_resources_inner())
    }

    #[cfg(test)]
    fn load_translation_resources_inner(
        &self,
    ) -> Result<TranslationResources, GenericProjectError> {
        let connection = self.open_connection(false)?;
        validate_project_database_with_cancellation(&connection, &self.cancellation)?;
        load_translation_resources_rows_with_cancellation(&connection, &self.cancellation)
    }

    /// 读取项目保存的现行 WriteBack 排版规则；保存的是规范内容而不是外部路径。
    pub(crate) fn load_write_back_layout_rules(
        &self,
    ) -> Result<LayoutRuleSet, GenericProjectError> {
        self.ensure_not_cancelled()?;
        self.finish_cancellable(self.load_write_back_layout_rules_inner())
    }

    fn load_write_back_layout_rules_inner(&self) -> Result<LayoutRuleSet, GenericProjectError> {
        let connection = self.open_connection(false)?;
        validate_current_generic_schema_with_cancellation(&connection, &self.cancellation)?;
        let canonical_json = load_translation_resource_row_with_cancellation(
            &connection,
            LAYOUT_RULES_RESOURCE,
            &self.cancellation,
        )?;
        LayoutRuleSet::from_canonical_json(&canonical_json)
            .map_err(GenericProjectError::InvalidLayoutRules)
    }

    /// 在 Extract 快照仍一致时原子替换项目排版规则；失败会回滚并保留旧规则。
    pub(crate) fn replace_write_back_layout_rules(
        &self,
        expected_raw_fingerprint: Sha256Fingerprint,
        rules: &LayoutRuleSet,
    ) -> Result<(), GenericProjectError> {
        self.ensure_not_cancelled()?;
        self.finish_cancellable(
            self.replace_write_back_layout_rules_inner(expected_raw_fingerprint, rules),
        )
    }

    fn replace_write_back_layout_rules_inner(
        &self,
        expected_raw_fingerprint: Sha256Fingerprint,
        rules: &LayoutRuleSet,
    ) -> Result<(), GenericProjectError> {
        let mut connection = self.open_connection(false)?;
        run_cancellable_transaction(
            &mut connection,
            &self.cancellation,
            self.performance.as_ref(),
            SqliteTransactionScope::WritePlan,
            "开始保存 Generic 排版规则",
            "提交 Generic 排版规则",
            "回滚 Generic 排版规则",
            |transaction| {
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
                let changed = transaction
                    .execute(
                        "UPDATE translation_resource
                         SET canonical_json = ?1 WHERE resource_kind = ?2",
                        params![rules.canonical_json(), LAYOUT_RULES_RESOURCE],
                    )
                    .map_err(|source| GenericProjectError::Sqlite {
                        operation: "保存 Generic 排版规则",
                        source,
                    })?;
                if changed != 1 {
                    return Err(GenericProjectError::Sqlite {
                        operation: "保存 Generic 排版规则",
                        source: rusqlite::Error::QueryReturnedNoRows,
                    });
                }
                Ok(())
            },
        )
    }

    /// 原子保存本轮资源，并以 CAS 清除规划时已经确认失效的旧译文。
    pub(crate) fn apply_translation_resources(
        &self,
        expected_raw_fingerprint: Sha256Fingerprint,
        terminology_json: &str,
        placeholder_rules_json: &str,
        invalidations: &[TranslationClear],
    ) -> Result<CommitTranslationsOutcome, GenericProjectError> {
        self.ensure_not_cancelled()?;
        self.finish_cancellable(self.apply_translation_resources_inner(
            expected_raw_fingerprint,
            terminology_json,
            placeholder_rules_json,
            invalidations,
        ))
    }

    fn apply_translation_resources_inner(
        &self,
        expected_raw_fingerprint: Sha256Fingerprint,
        terminology_json: &str,
        placeholder_rules_json: &str,
        invalidations: &[TranslationClear],
    ) -> Result<CommitTranslationsOutcome, GenericProjectError> {
        validate_translation_resources_with_cancellation(
            terminology_json,
            placeholder_rules_json,
            &self.cancellation,
        )?;
        let mut seen = GenericUnitMap::with_capacity(invalidations.len());
        for invalidation in invalidations {
            self.ensure_not_cancelled()?;
            let key = GenericUnitKey::new(
                clone_text_with_cancellation(&invalidation.group_id, &self.cancellation)?,
                clone_text_with_cancellation(&invalidation.unit_id, &self.cancellation)?,
            );
            if seen
                .insert_with_cancellation(key, (), || self.ensure_not_cancelled())?
                .is_some()
            {
                return Err(GenericProjectError::DuplicateTranslationClear {
                    group_id: clone_text_with_cancellation(
                        &invalidation.group_id,
                        &self.cancellation,
                    )?,
                    unit_id: clone_text_with_cancellation(
                        &invalidation.unit_id,
                        &self.cancellation,
                    )?,
                });
            }
        }
        let mut connection = self.open_connection(false)?;
        run_cancellable_transaction(
            &mut connection,
            &self.cancellation,
            self.performance.as_ref(),
            SqliteTransactionScope::WritePlan,
            "开始保存 Generic 翻译资源",
            "提交 Generic 翻译资源",
            "回滚 Generic 翻译资源",
            |transaction| {
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
                    let mut clear_automatic = transaction
                        .prepare_cached(
                            "UPDATE generic_unit
                             SET translation = NULL,
                                 translation_state = NULL
                             WHERE group_id = ?1 AND unit_id = ?2
                               AND source_text = ?3
                               AND EXISTS (
                                   SELECT 1 FROM generic_group
                                   WHERE group_id = ?1 AND context_fingerprint = ?4
                               )
                               AND translation = ?5
                               AND translation_state = ?6",
                        )
                        .map_err(|source| GenericProjectError::Sqlite {
                            operation: "准备清除失效 Generic 译文",
                            source,
                        })?;
                    let mut clear_manual = transaction
                        .prepare_cached(
                            "DELETE FROM generic_manual_translation
                             WHERE group_id = ?1 AND unit_id = ?2
                               AND translation_json = ?3
                               AND applicability_fingerprint = ?4
                               AND EXISTS (
                                   SELECT 1
                                   FROM generic_unit AS unit
                                   JOIN generic_group AS group_record
                                     ON group_record.group_id = unit.group_id
                                   WHERE unit.group_id = ?1 AND unit.unit_id = ?2
                                     AND unit.source_text = ?5
                                     AND group_record.context_fingerprint = ?6
                               )",
                        )
                        .map_err(|source| GenericProjectError::Sqlite {
                            operation: "准备清除失效 Generic 人工译文",
                            source,
                        })?;
                    let mut save_rejected = transaction
                        .prepare_cached(
                            "INSERT INTO generic_rejected_translation (
                                 group_id, unit_id, readable_id, origin, source_json,
                                 candidate_json, translation_shape, group_context,
                                 violation_json, planning_state
                             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'free', ?7, ?8, ?9)
                             ON CONFLICT (group_id, unit_id) DO UPDATE SET
                                 readable_id = excluded.readable_id,
                                 origin = excluded.origin,
                                 source_json = excluded.source_json,
                                 candidate_json = excluded.candidate_json,
                                 translation_shape = excluded.translation_shape,
                                 group_context = excluded.group_context,
                                 violation_json = excluded.violation_json,
                                 planning_state = excluded.planning_state",
                        )
                        .map_err(|source| GenericProjectError::Sqlite {
                            operation: "准备保存失效 Generic 候选",
                            source,
                        })?;
                    for invalidation in invalidations {
                        self.ensure_not_cancelled()?;
                        let expected_lines = invalidation
                            .expected_translation
                            .translation
                            .split('\n')
                            .map(str::to_owned)
                            .collect::<Vec<_>>();
                        let expected_translation_json = serde_json::to_string(&expected_lines)
                            .expect("Generic 当前译文字符串数组必须可以编码");
                        let changed = match invalidation.expected_translation.origin {
                            TranslationOrigin::Automatic => clear_automatic
                                .execute(params![
                                    invalidation.group_id,
                                    invalidation.unit_id,
                                    invalidation.expected_source_text,
                                    invalidation.expected_group_context.as_bytes().as_slice(),
                                    invalidation.expected_translation.translation,
                                    invalidation
                                        .expected_translation
                                        .state_fingerprint
                                        .as_bytes()
                                        .as_slice(),
                                ])
                                .map_err(|source| GenericProjectError::Sqlite {
                                    operation: "清除失效 Generic 自动译文",
                                    source,
                                })?,
                            TranslationOrigin::Manual => clear_manual
                                .execute(params![
                                    invalidation.group_id,
                                    invalidation.unit_id,
                                    expected_translation_json,
                                    invalidation
                                        .expected_translation
                                        .state_fingerprint
                                        .as_bytes()
                                        .as_slice(),
                                    invalidation.expected_source_text,
                                    invalidation.expected_group_context.as_bytes().as_slice(),
                                ])
                                .map_err(|source| GenericProjectError::Sqlite {
                                    operation: "清除失效 Generic 人工译文",
                                    source,
                                })?,
                        };
                        if changed == 1 {
                            committed += 1;
                            let source = invalidation
                                .expected_source_text
                                .split('\n')
                                .map(str::to_owned)
                                .collect::<Vec<_>>();
                            save_rejected
                                .execute(params![
                                    invalidation.group_id,
                                    invalidation.unit_id,
                                    invalidation.readable_id,
                                    invalidation.expected_translation.origin.storage_name(),
                                    serde_json::to_string(&source)
                                        .expect("Generic Rejected 原文必须可以编码"),
                                    expected_translation_json,
                                    invalidation.expected_group_context.as_bytes().as_slice(),
                                    serde_json::to_string(&invalidation.violation)
                                        .expect("Generic Rejected 原因必须可以编码"),
                                    invalidation.rejection_planning_state.as_bytes().as_slice(),
                                ])
                                .map_err(|source| GenericProjectError::Sqlite {
                                    operation: "保存失效 Generic 候选",
                                    source,
                                })?;
                        } else {
                            conflicts.push((
                                clone_text_with_cancellation(
                                    &invalidation.group_id,
                                    &self.cancellation,
                                )?,
                                clone_text_with_cancellation(
                                    &invalidation.unit_id,
                                    &self.cancellation,
                                )?,
                            ));
                        }
                    }
                }
                Ok(CommitTranslationsOutcome {
                    committed,
                    resolved_rejected: 0,
                    conflicts,
                })
            },
        )
    }

    fn open_connection(
        &self,
        create: bool,
    ) -> Result<AttSqliteCancellableConnection, GenericProjectError> {
        self.ensure_not_cancelled()?;
        if !create && !self.database_path.is_file() {
            return Err(GenericProjectError::ProjectNotFound {
                path: self.database_path.clone(),
            });
        }
        open_sqlite_connection(&self.database_path, create, self.cancellation.clone())
    }

    fn ensure_not_cancelled(&self) -> Result<(), GenericProjectError> {
        if self.cancellation.is_requested() {
            Err(GenericProjectError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn finish_cancellable<T>(
        &self,
        result: Result<T, GenericProjectError>,
    ) -> Result<T, GenericProjectError> {
        match result {
            Err(source)
                if self.cancellation.is_requested()
                    && source.is_sqlite_cancellation_without_cleanup_failure() =>
            {
                Err(GenericProjectError::Cancelled)
            }
            result => result,
        }
    }

    fn read_project_with_connection(
        &self,
        connection: &Connection,
    ) -> Result<GenericProject, GenericProjectError> {
        let row = load_generic_project_row_with_cancellation(connection, &self.cancellation)?;
        self.ensure_not_cancelled()?;
        let project_name = row
            .project_name
            .parse::<ProjectName>()
            .map_err(|_| invalid_database(GenericProjectDatabaseProblem::InvalidProjectName))?;
        self.ensure_not_cancelled()?;
        let source_root = decode_path_with_cancellation(&row.source_root, &self.cancellation)?;
        self.ensure_not_cancelled()?;
        let source = LanguageId::parse(&row.source_language)?;
        self.ensure_not_cancelled()?;
        let target = LanguageId::parse(&row.target_language)?;
        self.ensure_not_cancelled()?;
        validate_distinct_languages(&source, &target)?;
        self.ensure_not_cancelled()?;
        Ok(GenericProject {
            project_name,
            workspace_root: self.workspace_root.clone(),
            database_path: self.database_path.clone(),
            source_root,
            language_pair: LanguagePair::new(source, target),
            extracted_raw_fingerprint: read_optional_fingerprint(
                row.extracted_raw_fingerprint,
                "extracted_raw_fingerprint",
            )?,
            extracted_asset_fingerprint: read_optional_fingerprint(
                row.extracted_asset_fingerprint,
                "extracted_asset_fingerprint",
            )?,
            last_profile_id: row.last_profile_id,
        })
    }
}

struct GenericProjectRow {
    project_name: String,
    source_root: Vec<u8>,
    source_language: String,
    target_language: String,
    extracted_raw_fingerprint: Option<Vec<u8>>,
    extracted_asset_fingerprint: Option<Vec<u8>>,
    last_profile_id: Option<String>,
}

fn load_generic_project_row_with_cancellation(
    connection: &Connection,
    cancellation: &CooperativeCancellation,
) -> Result<GenericProjectRow, GenericProjectError> {
    const OPERATION: &str = "读取 Generic 项目记录";

    ensure_generic_operation_not_cancelled(cancellation)?;
    let mut statement = connection
        .prepare(
            "SELECT project_name, source_root, source_language, target_language,
                    extracted_raw_fingerprint, extracted_asset_fingerprint, last_profile_id
             FROM main.generic_project WHERE singleton = 1",
        )
        .map_err(|source| sqlite_operation_error(OPERATION, source))?;
    let mut rows = statement
        .query([])
        .map_err(|source| sqlite_operation_error(OPERATION, source))?;
    let Some(row) = rows
        .next()
        .map_err(|source| sqlite_operation_error(OPERATION, source))?
    else {
        return Err(invalid_database(
            GenericProjectDatabaseProblem::MissingProjectRow,
        ));
    };
    let project = GenericProjectRow {
        project_name: clone_sqlite_text_column_with_cancellation(row, 0, OPERATION, cancellation)?,
        source_root: clone_sqlite_blob_column_with_cancellation(row, 1, OPERATION, cancellation)?,
        source_language: clone_sqlite_text_column_with_cancellation(
            row,
            2,
            OPERATION,
            cancellation,
        )?,
        target_language: clone_sqlite_text_column_with_cancellation(
            row,
            3,
            OPERATION,
            cancellation,
        )?,
        extracted_raw_fingerprint: clone_optional_sqlite_blob_column_with_cancellation(
            row,
            4,
            OPERATION,
            cancellation,
        )?,
        extracted_asset_fingerprint: clone_optional_sqlite_blob_column_with_cancellation(
            row,
            5,
            OPERATION,
            cancellation,
        )?,
        last_profile_id: clone_optional_sqlite_text_column_with_cancellation(
            row,
            6,
            OPERATION,
            cancellation,
        )?,
    };
    drop(rows);
    drop(statement);
    ensure_generic_operation_not_cancelled(cancellation)?;
    Ok(project)
}

struct ReconciledSnapshot {
    files: Vec<GenericStoredFile>,
    preserved_translations: usize,
    cleared_translations: usize,
}

fn clone_sqlite_text_column_with_cancellation(
    row: &Row<'_>,
    index: usize,
    operation: &'static str,
    cancellation: &CooperativeCancellation,
) -> Result<String, GenericProjectError> {
    let value = row
        .get_ref(index)
        .map_err(|source| GenericProjectError::Sqlite { operation, source })?;
    let bytes = match value {
        ValueRef::Text(bytes) => bytes,
        value => {
            return Err(GenericProjectError::Sqlite {
                operation,
                source: invalid_sqlite_column_type(row, index, value),
            });
        }
    };
    clone_sqlite_text_value_with_cancellation(bytes, index, operation, cancellation)
}

fn clone_optional_sqlite_text_column_with_cancellation(
    row: &Row<'_>,
    index: usize,
    operation: &'static str,
    cancellation: &CooperativeCancellation,
) -> Result<Option<String>, GenericProjectError> {
    let value = row
        .get_ref(index)
        .map_err(|source| GenericProjectError::Sqlite { operation, source })?;
    match value {
        ValueRef::Null => Ok(None),
        ValueRef::Text(bytes) => {
            clone_sqlite_text_value_with_cancellation(bytes, index, operation, cancellation)
                .map(Some)
        }
        value => Err(GenericProjectError::Sqlite {
            operation,
            source: invalid_sqlite_column_type(row, index, value),
        }),
    }
}

fn clone_sqlite_text_value_with_cancellation(
    bytes: &[u8],
    index: usize,
    operation: &'static str,
    cancellation: &CooperativeCancellation,
) -> Result<String, GenericProjectError> {
    let bytes = clone_sqlite_bytes_with_cancellation(bytes, cancellation)?;
    validate_sqlite_utf8_with_cancellation(&bytes, index, operation, cancellation)?;
    // SAFETY: `validate_sqlite_utf8_with_cancellation` 已经分块覆盖整份字节串；跨块
    // 不完整码点会从码点起始位置在下一块重新校验。
    Ok(unsafe { String::from_utf8_unchecked(bytes) })
}

fn clone_sqlite_blob_column_with_cancellation(
    row: &Row<'_>,
    index: usize,
    operation: &'static str,
    cancellation: &CooperativeCancellation,
) -> Result<Vec<u8>, GenericProjectError> {
    let value = row
        .get_ref(index)
        .map_err(|source| GenericProjectError::Sqlite { operation, source })?;
    match value {
        ValueRef::Blob(bytes) => clone_sqlite_bytes_with_cancellation(bytes, cancellation),
        value => Err(GenericProjectError::Sqlite {
            operation,
            source: invalid_sqlite_column_type(row, index, value),
        }),
    }
}

fn clone_optional_sqlite_blob_column_with_cancellation(
    row: &Row<'_>,
    index: usize,
    operation: &'static str,
    cancellation: &CooperativeCancellation,
) -> Result<Option<Vec<u8>>, GenericProjectError> {
    let value = row
        .get_ref(index)
        .map_err(|source| GenericProjectError::Sqlite { operation, source })?;
    match value {
        ValueRef::Null => Ok(None),
        ValueRef::Blob(bytes) => {
            clone_sqlite_bytes_with_cancellation(bytes, cancellation).map(Some)
        }
        value => Err(GenericProjectError::Sqlite {
            operation,
            source: invalid_sqlite_column_type(row, index, value),
        }),
    }
}

fn invalid_sqlite_column_type(row: &Row<'_>, index: usize, value: ValueRef<'_>) -> rusqlite::Error {
    let column_name = row
        .as_ref()
        .column_name(index)
        .expect("已由 get_ref 确认 SQLite 列序号有效")
        .to_owned();
    rusqlite::Error::InvalidColumnType(index, column_name, value.data_type())
}

fn clone_sqlite_bytes_with_cancellation(
    bytes: &[u8],
    cancellation: &CooperativeCancellation,
) -> Result<Vec<u8>, GenericProjectError> {
    ensure_generic_operation_not_cancelled(cancellation)?;
    let mut cloned = Vec::with_capacity(bytes.len());
    for chunk in bytes.chunks(RESOURCE_CANCELLATION_CHECK_BYTES) {
        ensure_generic_operation_not_cancelled(cancellation)?;
        cloned.extend_from_slice(chunk);
    }
    ensure_generic_operation_not_cancelled(cancellation)?;
    Ok(cloned)
}

fn append_text_with_cancellation(
    output: &mut String,
    text: &str,
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericProjectError> {
    ensure_generic_operation_not_cancelled(cancellation)?;
    let mut start = 0_usize;
    while start < text.len() {
        ensure_generic_operation_not_cancelled(cancellation)?;
        let mut end = start
            .saturating_add(RESOURCE_CANCELLATION_CHECK_BYTES)
            .min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&text[start..end]);
        start = end;
    }
    ensure_generic_operation_not_cancelled(cancellation)
}

fn validate_sqlite_utf8_with_cancellation(
    bytes: &[u8],
    index: usize,
    operation: &'static str,
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericProjectError> {
    match validate_utf8_bytes_with_cancellation(bytes, cancellation)? {
        Ok(()) => Ok(()),
        Err(source) => Err(invalid_sqlite_utf8_error(
            index,
            operation,
            source.valid_up_to,
            source.error_len,
        )),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InvalidUtf8Facts {
    valid_up_to: usize,
    error_len: Option<usize>,
}

fn validate_utf8_bytes_with_cancellation(
    bytes: &[u8],
    cancellation: &CooperativeCancellation,
) -> Result<Result<(), InvalidUtf8Facts>, GenericProjectError> {
    let mut start = 0_usize;
    while start < bytes.len() {
        ensure_generic_operation_not_cancelled(cancellation)?;
        let end = start
            .saturating_add(RESOURCE_CANCELLATION_CHECK_BYTES)
            .min(bytes.len());
        match std::str::from_utf8(&bytes[start..end]) {
            Ok(_) => start = end,
            Err(source) => {
                let valid_end = start.saturating_add(source.valid_up_to());
                match source.error_len() {
                    Some(error_len) => {
                        ensure_generic_operation_not_cancelled(cancellation)?;
                        return Ok(Err(InvalidUtf8Facts {
                            valid_up_to: valid_end,
                            error_len: Some(error_len),
                        }));
                    }
                    None if end == bytes.len() => {
                        ensure_generic_operation_not_cancelled(cancellation)?;
                        return Ok(Err(InvalidUtf8Facts {
                            valid_up_to: valid_end,
                            error_len: None,
                        }));
                    }
                    None => {
                        debug_assert!(valid_end > start, "不完整 UTF-8 序列只能位于非空分块的末尾");
                        start = valid_end;
                    }
                }
            }
        }
    }
    ensure_generic_operation_not_cancelled(cancellation)?;
    Ok(Ok(()))
}

fn invalid_sqlite_utf8_error(
    index: usize,
    operation: &'static str,
    valid_up_to: usize,
    error_len: Option<usize>,
) -> GenericProjectError {
    invalid_database(GenericProjectDatabaseProblem::InvalidTextColumnUtf8 {
        operation: project_safe_identifier(operation, "sqlite_text"),
        column: index,
        valid_up_to,
        error_len,
    })
}

fn ensure_generic_operation_not_cancelled(
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericProjectError> {
    if cancellation.is_requested() {
        Err(GenericProjectError::Cancelled)
    } else {
        Ok(())
    }
}

fn validate_translation(translation: &str) -> Result<(), GenericProjectTranslationProblem> {
    if translation.chars().all(char::is_whitespace) {
        return Err(GenericProjectTranslationProblem::Blank);
    }
    if translation.contains('\r') {
        return Err(GenericProjectTranslationProblem::CarriageReturn);
    }
    if translation.contains('\0') {
        return Err(GenericProjectTranslationProblem::Nul);
    }
    Ok(())
}

fn clone_text_with_cancellation(
    value: &str,
    cancellation: &CooperativeCancellation,
) -> Result<String, GenericProjectError> {
    let mut cloned = String::with_capacity(value.len());
    append_text_with_cancellation(&mut cloned, value, cancellation)?;
    ensure_generic_operation_not_cancelled(cancellation)?;
    Ok(cloned)
}

fn bytes_equal_with_cancellation(
    left: &[u8],
    right: &[u8],
    cancellation: &CooperativeCancellation,
) -> Result<bool, GenericProjectError> {
    ensure_generic_operation_not_cancelled(cancellation)?;
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left
        .chunks(RESOURCE_CANCELLATION_CHECK_BYTES)
        .zip(right.chunks(RESOURCE_CANCELLATION_CHECK_BYTES))
    {
        ensure_generic_operation_not_cancelled(cancellation)?;
        if left != right {
            return Ok(false);
        }
    }
    ensure_generic_operation_not_cancelled(cancellation)?;
    Ok(true)
}

fn to_i64(value: usize) -> Result<i64, GenericProjectError> {
    i64::try_from(value)
        .map_err(|_| invalid_database(GenericProjectDatabaseProblem::OrdinalTooLarge { value }))
}

fn from_i64(value: i64, field: &'static str) -> Result<usize, GenericProjectError> {
    usize::try_from(value).map_err(|_| {
        invalid_database(GenericProjectDatabaseProblem::InvalidOrdinal {
            field: project_safe_identifier(field, "ordinal"),
            value,
        })
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
        invalid_database(GenericProjectDatabaseProblem::InvalidFingerprintLength {
            field: project_safe_identifier(field, "fingerprint"),
            expected: SHA256_FINGERPRINT_BYTES,
            actual: bytes.len(),
        })
    })?;
    Ok(Sha256Fingerprint::from_bytes(array))
}

fn encode_path(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn decode_path_with_cancellation(
    bytes: &[u8],
    cancellation: &CooperativeCancellation,
) -> Result<PathBuf, GenericProjectError> {
    use std::os::windows::ffi::OsStringExt;

    ensure_generic_operation_not_cancelled(cancellation)?;
    if bytes.is_empty() || !bytes.len().is_multiple_of(2) {
        return Err(invalid_database(
            GenericProjectDatabaseProblem::InvalidUtf16Path {
                actual_bytes: bytes.len(),
            },
        ));
    }
    let mut units = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks(RESOURCE_CANCELLATION_CHECK_BYTES) {
        ensure_generic_operation_not_cancelled(cancellation)?;
        for pair in chunk.chunks_exact(2) {
            units.push(u16::from_le_bytes([pair[0], pair[1]]));
        }
    }
    ensure_generic_operation_not_cancelled(cancellation)?;
    Ok(PathBuf::from(OsString::from_wide(&units)))
}

#[cfg(test)]
mod tests;
