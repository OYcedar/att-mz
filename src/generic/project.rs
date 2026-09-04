//! Generic 项目的专用 SQLite 状态与重复 Extract。

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use rusqlite::types::ValueRef;
use rusqlite::{Connection, DropBehavior, OpenFlags, Row, Transaction, params};

use crate::diagnostic::{
    Diagnostic, DiagnosticReport, FileSystemDiagnosticContext, FileSystemDiagnosticStage,
    FileSystemIssue, FileSystemOperation, FileSystemProblem, GenericDiagnosticStage, GenericIssue,
    GenericLanguageViolation, GenericProblem, GenericProjectDatabaseProblem,
    GenericProjectTranslationProblem, GenericResourceKind, IoFailure, RelatedFailureRelation,
    SafeIdentifier, SafePath, SqliteDiagnosticContext, SqliteDiagnosticStage, SqliteDriverFailure,
    SqliteIssue, SqliteOperation, SqliteProblem, SqliteTransactionState, StateEffect,
    TranslationIssue, TranslationPlanningResourceKind, TranslationPlanningResourceOrigin,
    TranslationPlanningResourceProblem,
};
use crate::execution::CooperativeCancellation;
use crate::fingerprint::{SHA256_FINGERPRINT_BYTES, Sha256Fingerprint, Sha256FramedHasher};
use crate::language::{LanguageId, LanguageIdError, LanguagePair};
use crate::project_name::ProjectName;
use crate::runtime::performance::{
    RunPerformanceCounters, SqliteTransactionControl, SqliteTransactionScope,
};
use crate::runtime::sqlite::{
    AttSqliteCancellableConnection, AttSqliteCancellationHandle,
    apply_att_sqlite_cancellable_read_write_policy, apply_att_sqlite_new_database_page_policy,
    begin_cancellable_transaction, execute_transaction_control, suspend_att_sqlite_cancellation,
};
use crate::runtime::windows::{
    FileIdentity, WindowsFsError, create_new_pinned_database_file, delete_regular_file_if_identity,
    pin_directory_without_reparse, pin_path_without_reparse, rename_without_replace_if_identity,
};
use crate::translation::TranslationOrigin;
use crate::translation::candidate_validation::ProvenInvariantViolation;
use crate::translation::layout_rules::{LayoutRuleSet, LayoutRulesError};
#[cfg(test)]
use crate::translation::placeholder::{
    PlaceholderRuleCompilationError, PlaceholderWorkerOperation,
};
use crate::translation::planning_resource::{
    CompiledTerminology, TerminologyDefinitionError, TerminologyEntry,
    compile_terminology_with_cancellation, terminology_problem, translation_json_failure,
};

use super::identity::CancellableTextMap;
#[cfg(test)]
use super::jsonl::scan_input_tree;
use super::jsonl::{GenericInputSnapshot, GenericJsonlError, scan_input_tree_with_cancellation};
use super::placeholder::{
    GenericCompiledPlaceholderRules, GenericPlaceholderError, GenericPlaceholderService,
};
use super::translate::{
    GenericPlanningError, GenericUnitKey, GenericUnitMap, readable_generic_unit_id,
};

const DATABASE_FILE_NAME: &str = "project.db";
const FINGERPRINT_CANCELLATION_CHECK_BYTES: NonZeroUsize =
    NonZeroUsize::new(64 * 1024).expect("Generic 指纹取消检查块大小必须非零");
const RESOURCE_CANCELLATION_CHECK_BYTES: usize = 64 * 1024;
const SQLITE_SIDECAR_SUFFIXES: [&str; 3] = ["-journal", "-wal", "-shm"];
const TERMINOLOGY_RESOURCE: &str = "terminology";
const PLACEHOLDER_RULES_RESOURCE: &str = "placeholder_rules";
const LAYOUT_RULES_RESOURCE: &str = "write_back_layout_rules";
const CREATE_INITIAL_SCHEMA_SQL: &str = "CREATE TABLE generic_project (
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
                 translation_state BLOB CHECK (
                     translation_state IS NULL OR length(translation_state) = 32
                 ),
                 PRIMARY KEY (group_id, unit_id),
                 UNIQUE (group_id, ordinal),
                 CHECK (
                     (translation IS NULL AND translation_state IS NULL)
                     OR
                     (translation IS NOT NULL AND length(trim(translation)) > 0
                      AND instr(translation, char(13)) = 0
                      AND instr(translation, char(0)) = 0
                      AND translation_state IS NOT NULL)
                 )
             ) STRICT;
             CREATE TABLE generic_manual_translation (
                 group_id TEXT NOT NULL CHECK (length(CAST(group_id AS BLOB)) > 0),
                 unit_id TEXT NOT NULL CHECK (length(CAST(unit_id AS BLOB)) > 0),
                 readable_id TEXT NOT NULL CHECK (length(readable_id) > 0),
                 source_json TEXT NOT NULL CHECK (
                     json_valid(source_json) AND json_type(source_json) = 'array'
                 ),
                 translation_json TEXT NOT NULL CHECK (
                     json_valid(translation_json)
                     AND json_type(translation_json) = 'array'
                     AND json_array_length(translation_json) > 0
                 ),
                 applicability_fingerprint BLOB NOT NULL CHECK (
                     length(applicability_fingerprint) = 32
                 ),
                 PRIMARY KEY (group_id, unit_id)
             ) STRICT;
             CREATE TABLE generic_rejected_translation (
                 group_id TEXT NOT NULL,
                 unit_id TEXT NOT NULL,
                 readable_id TEXT NOT NULL CHECK (length(readable_id) > 0),
                 origin TEXT NOT NULL CHECK (origin IN ('automatic', 'manual')),
                 source_json TEXT NOT NULL CHECK (
                     json_valid(source_json)
                     AND json_type(source_json) = 'array'
                     AND json_array_length(source_json) > 0
                 ),
                 candidate_json TEXT NOT NULL CHECK (json_valid(candidate_json)),
                 translation_shape TEXT NOT NULL CHECK (translation_shape = 'free'),
                 group_context BLOB NOT NULL CHECK (length(group_context) = 32),
                 violation_json TEXT NOT NULL CHECK (
                     json_valid(violation_json) AND json_type(violation_json) = 'object'
                 ),
                 planning_state BLOB NOT NULL CHECK (length(planning_state) = 32),
                 PRIMARY KEY (group_id, unit_id),
                 FOREIGN KEY (group_id, unit_id)
                     REFERENCES generic_unit(group_id, unit_id) ON DELETE CASCADE
             ) STRICT;
             CREATE TABLE translation_resource (
                 resource_kind TEXT PRIMARY KEY CHECK (
                     resource_kind IN ('terminology', 'placeholder_rules', 'write_back_layout_rules')
                 ),
                 canonical_json TEXT NOT NULL CHECK (length(canonical_json) > 0)
              ) STRICT;
              INSERT INTO translation_resource (resource_kind, canonical_json)
              VALUES ('terminology', '[]'), ('placeholder_rules', '[]'),
                     ('write_back_layout_rules', '[]');";

#[cfg(test)]
pub(crate) fn create_current_generic_schema_for_test(
    connection: &Connection,
) -> Result<(), rusqlite::Error> {
    connection.execute_batch(CREATE_INITIAL_SCHEMA_SQL)
}
const SELECT_GENERIC_ATT_SCHEMA: &str = "SELECT type, name, tbl_name, sql
    FROM main.sqlite_schema
    WHERE sql IS NOT NULL
      AND tbl_name IN (
          'generic_project',
          'generic_file',
          'generic_group',
          'generic_unit',
          'generic_manual_translation',
          'generic_rejected_translation',
          'translation_resource'
      )
    ORDER BY type, name";
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
const LOAD_UNITS_NATURAL_SQL: &str = "
    SELECT u.group_id, u.unit_id, u.ordinal, u.source_text,
           u.translation, u.translation_state,
           manual.translation_json, manual.applicability_fingerprint,
           rejected.readable_id, rejected.origin, rejected.source_json,
           rejected.candidate_json, rejected.translation_shape,
           rejected.group_context, rejected.violation_json, rejected.planning_state
    FROM main.generic_file AS f
    CROSS JOIN main.generic_group AS g
    CROSS JOIN main.generic_unit AS u
    LEFT JOIN main.generic_manual_translation AS manual
      ON manual.group_id = u.group_id
     AND manual.unit_id = u.unit_id
    LEFT JOIN main.generic_rejected_translation AS rejected
      ON rejected.group_id = u.group_id
     AND rejected.unit_id = u.unit_id
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

    pub(crate) fn into_parts(self) -> (String, String) {
        (self.terminology_json, self.placeholder_rules_json)
    }
}

/// 已按当前 Generic 契约解析、规范化并完成语义编译的 Placeholder 资源。
///
/// 字段保持私有，使项目完整校验只能凭这份编译结果跳过重复 PCRE2 编译，不能用一段裸
/// JSON 冒充已经验证的资源。
#[derive(Clone)]
pub(crate) struct GenericCompiledPlaceholderResource {
    canonical_json: Arc<String>,
    compiled: GenericCompiledPlaceholderRules,
}

impl GenericCompiledPlaceholderResource {
    pub(crate) fn canonical_json(&self) -> &str {
        self.canonical_json.as_str()
    }
}

impl fmt::Debug for GenericCompiledPlaceholderResource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GenericCompiledPlaceholderResource")
            .field("canonical_json_bytes", &self.canonical_json.len())
            .field("compiled", &self.compiled)
            .finish()
    }
}

/// 已按当前 Generic 契约解析、规范化并完成语义编译的术语资源。
#[derive(Clone)]
pub(crate) struct GenericCompiledTerminologyResource {
    canonical_json: Arc<String>,
    compiled: Arc<CompiledTerminology>,
}

impl GenericCompiledTerminologyResource {
    pub(crate) fn canonical_json(&self) -> &str {
        self.canonical_json.as_str()
    }
}

impl fmt::Debug for GenericCompiledTerminologyResource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GenericCompiledTerminologyResource")
            .field("canonical_json_bytes", &self.canonical_json.len())
            .field("compiled", &self.compiled)
            .finish()
    }
}

/// 当前 Generic 项目中两份已经完成规范解析与语义编译的翻译资源。
///
/// `load_current_translation_state` 在完整数据库校验期间建立这份值，并把同一份编译结果
/// 交给 Translate 与 WriteBack。调用方只能取得已编译对象和对应规范 JSON，不能把裸
/// JSON 标记成已验证。
#[derive(Clone, Debug)]
pub(crate) struct GenericCompiledTranslationResources {
    terminology: GenericCompiledTerminologyResource,
    placeholder: GenericCompiledPlaceholderResource,
}

impl GenericCompiledTranslationResources {
    pub(crate) fn terminology_json(&self) -> &str {
        self.terminology.canonical_json()
    }

    pub(crate) fn placeholder_rules_json(&self) -> &str {
        self.placeholder.canonical_json()
    }

    pub(crate) fn terminology(&self) -> Arc<CompiledTerminology> {
        Arc::clone(&self.terminology.compiled)
    }

    pub(crate) fn placeholder_rules(&self) -> GenericCompiledPlaceholderRules {
        self.placeholder.compiled.clone()
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
            validate_schema_with_cancellation(&connection, &self.cancellation)?;
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
            .and_then(|()| validate_schema_with_cancellation(&connection, &self.cancellation));
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
        validate_schema_with_cancellation(&connection, &self.cancellation)?;
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
        validate_schema_with_cancellation(&connection, &self.cancellation)?;
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
        let resources = validate_schema_with_cancellation(&connection, &self.cancellation)?;
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
        validate_schema_with_cancellation(&connection, &self.cancellation)?;
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

fn open_sqlite_connection(
    database_path: &Path,
    create: bool,
    cancellation: CooperativeCancellation,
) -> Result<AttSqliteCancellableConnection, GenericProjectError> {
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
    let wait_cancellation = cancellation.clone();
    apply_att_sqlite_cancellable_read_write_policy(connection, move || {
        wait_cancellation.is_requested()
    })
    .map_err(|source| {
        if cancellation.is_requested() && sqlite_error_is_busy(&source) {
            GenericProjectError::Cancelled
        } else {
            GenericProjectError::Sqlite {
                operation: "应用 Generic SQLite 读写策略",
                source,
            }
        }
    })
}

#[derive(Debug)]
pub(crate) enum GenericTransactionFinalizationFailure {
    Sqlite {
        operation: &'static str,
        source: rusqlite::Error,
    },
    InvalidState {
        violation: GenericTransactionFinalizationViolation,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GenericTransactionFinalizationViolation {
    CommitSucceededButTransactionActive,
    CommitFailedButTransactionClosed,
    RollbackSucceededButTransactionActive,
}

impl fmt::Display for GenericTransactionFinalizationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite { operation, source } => {
                write!(formatter, "{operation}失败：{source}")
            }
            Self::InvalidState { violation } => formatter.write_str(match violation {
                GenericTransactionFinalizationViolation::CommitSucceededButTransactionActive => {
                    "COMMIT 返回成功后 Generic SQLite 连接仍处于事务中"
                }
                GenericTransactionFinalizationViolation::CommitFailedButTransactionClosed => {
                    "COMMIT 返回错误后 Generic SQLite 连接已离开事务，结果无法确认"
                }
                GenericTransactionFinalizationViolation::RollbackSucceededButTransactionActive => {
                    "ROLLBACK 返回成功后 Generic SQLite 连接仍处于事务中"
                }
            }),
        }
    }
}

impl Error for GenericTransactionFinalizationFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite { source, .. } => Some(source),
            Self::InvalidState { .. } => None,
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "事务连接、取消、计数范围、三条终态诊断和业务体都是本边界的直接输入"
)]
fn run_cancellable_transaction<T>(
    connection: &mut AttSqliteCancellableConnection,
    cancellation: &CooperativeCancellation,
    performance: &RunPerformanceCounters,
    scope: SqliteTransactionScope,
    begin_operation: &'static str,
    commit_operation: &'static str,
    rollback_operation: &'static str,
    body: impl FnOnce(&Transaction<'_>) -> Result<T, GenericProjectError>,
) -> Result<T, GenericProjectError> {
    let cancellation_handle = connection.cancellation_handle();
    let mut transaction =
        begin_cancellable_transaction(connection, performance, scope).map_err(|source| {
            GenericProjectError::Sqlite {
                operation: begin_operation,
                source,
            }
        })?;
    let body_result = body(&transaction);

    // 从这里开始只允许本函数显式确定终态，不能再让 Transaction::drop 吞掉回滚错误。
    transaction.set_drop_behavior(DropBehavior::Ignore);
    let result = match body_result {
        Err(primary) => Err(rollback_generic_transaction(
            &transaction,
            &cancellation_handle,
            performance,
            scope,
            primary,
            rollback_operation,
        )),
        Ok(_) if cancellation.is_requested() => Err(rollback_generic_transaction(
            &transaction,
            &cancellation_handle,
            performance,
            scope,
            GenericProjectError::Cancelled,
            rollback_operation,
        )),
        Ok(value) => commit_generic_transaction(
            &transaction,
            &cancellation_handle,
            performance,
            scope,
            commit_operation,
            rollback_operation,
        )
        .map(|()| value),
    };
    drop(transaction);
    result
}

fn rollback_generic_transaction(
    transaction: &Transaction<'_>,
    cancellation: &AttSqliteCancellationHandle,
    performance: &RunPerformanceCounters,
    scope: SqliteTransactionScope,
    primary: GenericProjectError,
    rollback_operation: &'static str,
) -> GenericProjectError {
    if transaction.is_autocommit() {
        return primary;
    }

    let suspension = suspend_att_sqlite_cancellation(cancellation);
    let rollback = execute_transaction_control(
        transaction,
        performance,
        scope,
        SqliteTransactionControl::Rollback,
        "ROLLBACK",
    );
    let is_autocommit = transaction.is_autocommit();
    drop(suspension);

    match rollback {
        Ok(()) if is_autocommit => primary,
        Ok(()) => GenericProjectError::TransactionOutcomeUnknown {
            operation: rollback_operation,
            primary: Some(Box::new(primary)),
            finalization: GenericTransactionFinalizationFailure::InvalidState {
                violation:
                    GenericTransactionFinalizationViolation::RollbackSucceededButTransactionActive,
            },
        },
        Err(source) => GenericProjectError::TransactionOutcomeUnknown {
            operation: rollback_operation,
            primary: Some(Box::new(primary)),
            finalization: GenericTransactionFinalizationFailure::Sqlite {
                operation: rollback_operation,
                source,
            },
        },
    }
}

fn commit_generic_transaction(
    transaction: &Transaction<'_>,
    cancellation: &AttSqliteCancellationHandle,
    performance: &RunPerformanceCounters,
    scope: SqliteTransactionScope,
    commit_operation: &'static str,
    rollback_operation: &'static str,
) -> Result<(), GenericProjectError> {
    let suspension = suspend_att_sqlite_cancellation(cancellation);

    let commit = execute_transaction_control(
        transaction,
        performance,
        scope,
        SqliteTransactionControl::Commit,
        "COMMIT",
    );
    let is_autocommit = transaction.is_autocommit();
    let result = match commit {
        Ok(()) if is_autocommit => Ok(()),
        Ok(()) => Err(GenericProjectError::TransactionOutcomeUnknown {
            operation: commit_operation,
            primary: None,
            finalization: GenericTransactionFinalizationFailure::InvalidState {
                violation:
                    GenericTransactionFinalizationViolation::CommitSucceededButTransactionActive,
            },
        }),
        Err(source) if is_autocommit => Err(GenericProjectError::TransactionOutcomeUnknown {
            operation: commit_operation,
            primary: Some(Box::new(GenericProjectError::Sqlite {
                operation: commit_operation,
                source,
            })),
            finalization: GenericTransactionFinalizationFailure::InvalidState {
                violation:
                    GenericTransactionFinalizationViolation::CommitFailedButTransactionClosed,
            },
        }),
        Err(source) => {
            let rollback = execute_transaction_control(
                transaction,
                performance,
                scope,
                SqliteTransactionControl::Rollback,
                "ROLLBACK",
            );
            let rollback_autocommit = transaction.is_autocommit();
            match rollback {
                Ok(()) if rollback_autocommit => {
                    Err(GenericProjectError::TransactionNotCommitted {
                        operation: commit_operation,
                        source,
                    })
                }
                Ok(()) => Err(GenericProjectError::TransactionOutcomeUnknown {
                    operation: commit_operation,
                    primary: Some(Box::new(GenericProjectError::Sqlite {
                        operation: commit_operation,
                        source,
                    })),
                    finalization: GenericTransactionFinalizationFailure::InvalidState {
                        violation:
                            GenericTransactionFinalizationViolation::RollbackSucceededButTransactionActive,
                    },
                }),
                Err(rollback) => Err(GenericProjectError::TransactionOutcomeUnknown {
                    operation: commit_operation,
                    primary: Some(Box::new(GenericProjectError::Sqlite {
                        operation: commit_operation,
                        source,
                    })),
                    finalization: GenericTransactionFinalizationFailure::Sqlite {
                        operation: rollback_operation,
                        source: rollback,
                    },
                }),
            }
        }
    };
    drop(suspension);
    result
}

#[derive(Debug)]
pub(crate) enum GenericProjectResourceError {
    InvalidSnapshot {
        resource: GenericResourceKind,
        source: serde_json::Error,
    },
    SnapshotEncoding {
        resource: GenericResourceKind,
        source: serde_json::Error,
    },
    TerminologyDefinition(TerminologyDefinitionError),
    Placeholder(GenericPlaceholderError),
    NonCanonicalSnapshot {
        resource: GenericResourceKind,
    },
}

impl fmt::Display for GenericProjectResourceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSnapshot { resource, source } => {
                write!(
                    formatter,
                    "{resource:?} 资源快照不是现行规范 JSON：{source}"
                )
            }
            Self::SnapshotEncoding { resource, source } => {
                write!(formatter, "{resource:?} 资源快照无法编码：{source}")
            }
            Self::TerminologyDefinition(source) => source.fmt(formatter),
            Self::Placeholder(source) => source.fmt(formatter),
            Self::NonCanonicalSnapshot { resource } => {
                write!(formatter, "{resource:?} 资源快照不是规范紧凑 JSON")
            }
        }
    }
}

impl Error for GenericProjectResourceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidSnapshot { source, .. } | Self::SnapshotEncoding { source, .. } => {
                Some(source)
            }
            Self::TerminologyDefinition(source) => Some(source),
            Self::Placeholder(source) => Some(source),
            Self::NonCanonicalSnapshot { .. } => None,
        }
    }
}

/// Generic 项目行为失败。
#[derive(Debug)]
pub(crate) enum GenericProjectError {
    Cancelled,
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
        operation: FileSystemOperation,
        path: PathBuf,
        source: io::Error,
    },
    InitialDatabaseFileSystem {
        operation: FileSystemOperation,
        source: WindowsFsError,
    },
    InitialDatabaseOutcomeUnknown(Box<GenericProjectError>),
    Sqlite {
        operation: &'static str,
        source: rusqlite::Error,
    },
    TransactionNotCommitted {
        operation: &'static str,
        source: rusqlite::Error,
    },
    TransactionOutcomeUnknown {
        operation: &'static str,
        primary: Option<Box<GenericProjectError>>,
        finalization: GenericTransactionFinalizationFailure,
    },
    InitialCandidateCleanup {
        original: Box<GenericProjectError>,
        cleanup: Vec<GenericProjectError>,
    },
    InvalidDatabase {
        problem: GenericProjectDatabaseProblem,
        source: Option<Box<GenericPlanningError>>,
    },
    InvalidLanguage(LanguageIdError),
    SameSourceAndTargetLanguage {
        language: String,
    },
    Jsonl(GenericJsonlError),
    InputChangedDuringExtract,
    ExtractRequired,
    TranslationSnapshotChanged,
    InvalidTranslation {
        group_id: Option<String>,
        unit_id: Option<String>,
        problem: GenericProjectTranslationProblem,
        source: Option<Box<GenericPlaceholderError>>,
    },
    DuplicateTranslationWrite {
        group_id: String,
        unit_id: String,
    },
    DuplicateTranslationClear {
        group_id: String,
        unit_id: String,
    },
    BlankProfileId,
    InvalidResource(GenericProjectResourceError),
    InvalidLayoutRules(LayoutRulesError),
}

impl fmt::Display for GenericProjectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("Generic 项目操作已取消"),
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
            } => write!(
                formatter,
                "{}失败：{}（{source}）",
                operation.as_str(),
                path.display()
            ),
            Self::Sqlite { operation, source } => write!(formatter, "{operation}失败：{source}"),
            Self::InitialDatabaseFileSystem { source, .. } => source.fmt(formatter),
            Self::InitialDatabaseOutcomeUnknown(source) => {
                write!(formatter, "初始数据库结果未知：{source}")
            }
            Self::TransactionNotCommitted { operation, source } => write!(
                formatter,
                "{operation}失败，事务已确认回滚且未提交：{source}"
            ),
            Self::TransactionOutcomeUnknown {
                operation,
                primary,
                finalization,
            } => {
                write!(formatter, "{operation}后事务结果未知")?;
                if let Some(primary) = primary {
                    write!(formatter, "；主失败：{primary}")?;
                }
                write!(formatter, "；终态确认失败：{finalization}")
            }
            Self::InitialCandidateCleanup { original, cleanup } => {
                write!(formatter, "{original}")?;
                for source in cleanup {
                    write!(formatter, "；清理初始数据库候选失败：{source}")?;
                }
                Ok(())
            }
            Self::InvalidDatabase { problem, .. } => {
                write!(formatter, "Generic 项目数据库无效：{problem:?}")
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
            Self::InvalidTranslation {
                problem, source, ..
            } => {
                write!(formatter, "Generic 译文无效：{problem:?}")?;
                if let Some(source) = source {
                    write!(formatter, "（{source}）")?;
                }
                Ok(())
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
            Self::InvalidResource(source) => write!(formatter, "Generic 翻译资源无效：{source}"),
            Self::InvalidLayoutRules(source) => {
                write!(formatter, "Generic WriteBack 排版规则无效：{source}")
            }
        }
    }
}

impl Error for GenericProjectError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::InitialDatabaseFileSystem { source, .. } => Some(source),
            Self::InitialDatabaseOutcomeUnknown(source) => Some(source.as_ref()),
            Self::Sqlite { source, .. } | Self::TransactionNotCommitted { source, .. } => {
                Some(source)
            }
            Self::TransactionOutcomeUnknown {
                primary: Some(primary),
                ..
            } => Some(primary.as_ref()),
            Self::TransactionOutcomeUnknown {
                primary: None,
                finalization,
                ..
            } => finalization.source(),
            Self::InitialCandidateCleanup { original, .. } => Some(original.as_ref()),
            Self::InvalidLanguage(source) => Some(source),
            Self::Jsonl(source) => Some(source),
            Self::InvalidTranslation {
                source: Some(source),
                ..
            } => Some(source.as_ref()),
            Self::InvalidResource(source) => Some(source),
            Self::InvalidLayoutRules(source) => Some(source),
            Self::InvalidDatabase {
                source: Some(source),
                ..
            } => Some(source.as_ref()),
            Self::Cancelled
            | Self::MissingInitialField(_)
            | Self::WorkspaceNotDirectory { .. }
            | Self::SourceNotDirectory { .. }
            | Self::SourceWriteBackOverlap { .. }
            | Self::ProjectNotFound { .. }
            | Self::ProjectIdentityMismatch { .. }
            | Self::InvalidDatabase { source: None, .. }
            | Self::SameSourceAndTargetLanguage { .. }
            | Self::InputChangedDuringExtract
            | Self::ExtractRequired
            | Self::TranslationSnapshotChanged
            | Self::InvalidTranslation { source: None, .. }
            | Self::DuplicateTranslationWrite { .. }
            | Self::DuplicateTranslationClear { .. }
            | Self::BlankProfileId => None,
        }
    }
}

impl GenericProjectError {
    /// 在 Generic 项目边界仍掌握数据库路径、查询 ID、事务终态和后端类别时建立公开报告。
    /// 原始错误正文只保留在 `Error::source`，不会进入 CLI 或 JSONL。
    pub(crate) fn diagnostic_report(
        &self,
        stage: GenericDiagnosticStage,
        database: &Path,
        effect: StateEffect,
    ) -> DiagnosticReport {
        let generic = |problem| {
            DiagnosticReport::new(
                effect,
                Diagnostic::generic(GenericIssue::project(stage, problem)),
            )
        };
        match self {
            Self::Cancelled => generic(GenericProblem::ProjectCancelled),
            Self::MissingInitialField(field) => generic(GenericProblem::MissingInitialField {
                field: project_safe_identifier(field, "initial_field"),
            }),
            Self::WorkspaceNotDirectory { path } | Self::SourceNotDirectory { path } => {
                file_system_project_report(
                    stage,
                    FileSystemOperation::Open,
                    FileSystemProblem::NotDirectory {
                        path: SafePath::new(path),
                    },
                    effect,
                )
            }
            Self::SourceWriteBackOverlap {
                source_root,
                write_back_root,
            } => generic(GenericProblem::SourceWriteBackOverlap {
                source_root: SafePath::new(source_root),
                write_back_root: SafePath::new(write_back_root),
            }),
            Self::ProjectNotFound { path } => file_system_project_report(
                stage,
                FileSystemOperation::Open,
                FileSystemProblem::NotFound {
                    path: SafePath::new(path),
                },
                effect,
            ),
            Self::ProjectIdentityMismatch { expected, observed } => {
                generic(GenericProblem::ProjectIdentityMismatch {
                    expected: project_safe_identifier(expected, "expected_project"),
                    observed: project_safe_identifier(observed, "observed_project"),
                })
            }
            Self::Io {
                operation,
                path,
                source,
            } => file_system_project_report(
                stage,
                *operation,
                FileSystemProblem::Io {
                    path: SafePath::new(path),
                    failure: IoFailure::from_error(source),
                },
                effect,
            ),
            Self::Sqlite { operation, source } => sqlite_project_report(
                stage,
                database,
                operation,
                source,
                SqliteOperation::Execute,
                SqliteTransactionState::Active,
                effect,
            ),
            Self::TransactionNotCommitted { operation, source } => sqlite_project_report(
                stage,
                database,
                operation,
                source,
                SqliteOperation::Transaction,
                SqliteTransactionState::RolledBack,
                StateEffect::Unchanged,
            ),
            Self::InitialDatabaseFileSystem { operation, source } => DiagnosticReport::new(
                effect,
                source.diagnostic(FileSystemDiagnosticContext::new(
                    file_system_project_stage(stage),
                    *operation,
                )),
            ),
            Self::InitialDatabaseOutcomeUnknown(source) => {
                source.diagnostic_report(stage, database, StateEffect::OutcomeUnknown)
            }
            Self::TransactionOutcomeUnknown {
                operation: _,
                primary,
                finalization,
            } => {
                let finalization = match finalization {
                    GenericTransactionFinalizationFailure::Sqlite { operation, source } => {
                        sqlite_project_report(
                            stage,
                            database,
                            operation,
                            source,
                            SqliteOperation::Transaction,
                            SqliteTransactionState::OutcomeUnknown,
                            StateEffect::OutcomeUnknown,
                        )
                    }
                    GenericTransactionFinalizationFailure::InvalidState { .. } => {
                        DiagnosticReport::new(
                            StateEffect::OutcomeUnknown,
                            Diagnostic::sqlite(SqliteIssue::new(
                                SqliteDiagnosticContext::new(
                                    sqlite_project_stage(stage),
                                    SqliteOperation::Transaction,
                                    SqliteTransactionState::OutcomeUnknown,
                                ),
                                SqliteProblem::InternalInvariant {
                                    database: SafePath::new(database),
                                },
                            )),
                        )
                    }
                };
                primary.as_deref().map_or(finalization.clone(), |primary| {
                    primary
                        .diagnostic_report(stage, database, StateEffect::Unchanged)
                        .with_related(RelatedFailureRelation::Finalization, finalization)
                })
            }
            Self::InitialCandidateCleanup { original, cleanup } => {
                let mut report = original.diagnostic_report(stage, database, effect);
                for source in cleanup {
                    report = report.with_related(
                        RelatedFailureRelation::Cleanup,
                        source.diagnostic_report(stage, database, StateEffect::RecoveryRequired),
                    );
                }
                report
            }
            Self::InvalidDatabase { problem, .. } => {
                generic(GenericProblem::InvalidProjectDatabase {
                    problem: problem.clone(),
                })
            }
            Self::InvalidLanguage(source) => generic(GenericProblem::InvalidLanguage {
                violation: generic_language_violation(source),
            }),
            Self::SameSourceAndTargetLanguage { language } => {
                generic(GenericProblem::SameSourceAndTargetLanguage {
                    language: project_safe_identifier(language, "language"),
                })
            }
            Self::Jsonl(source) => DiagnosticReport::new(effect, source.diagnostic(stage)),
            Self::InputChangedDuringExtract => generic(GenericProblem::InputChangedDuringExtract),
            Self::ExtractRequired => generic(GenericProblem::ExtractRequired),
            Self::TranslationSnapshotChanged => generic(GenericProblem::TranslationSnapshotChanged),
            Self::InvalidTranslation {
                group_id,
                unit_id,
                problem,
                ..
            } => generic(GenericProblem::InvalidTranslation {
                group_id: group_id
                    .as_deref()
                    .and_then(|value| SafeIdentifier::new(value).ok()),
                unit_id: unit_id
                    .as_deref()
                    .and_then(|value| SafeIdentifier::new(value).ok()),
                problem: problem.clone(),
            }),
            Self::DuplicateTranslationWrite { group_id, unit_id } => {
                generic(GenericProblem::DuplicateTranslationWrite {
                    group_id: project_safe_identifier(group_id, "group_id"),
                    unit_id: project_safe_identifier(unit_id, "unit_id"),
                })
            }
            Self::DuplicateTranslationClear { group_id, unit_id } => {
                generic(GenericProblem::DuplicateTranslationClear {
                    group_id: project_safe_identifier(group_id, "group_id"),
                    unit_id: project_safe_identifier(unit_id, "unit_id"),
                })
            }
            Self::BlankProfileId => generic(GenericProblem::BlankProfileId),
            Self::InvalidResource(source) => generic_project_resource_report(source, stage, effect),
            Self::InvalidLayoutRules(source) => generic(GenericProblem::WriteBackLayoutRules {
                path: None,
                rule_number: source.rule_number(),
                project_snapshot: true,
            }),
        }
    }
}

fn project_safe_identifier(value: impl AsRef<str>, fallback: &'static str) -> SafeIdentifier {
    SafeIdentifier::new(value).unwrap_or_else(|_| SafeIdentifier::from_validated(fallback))
}

fn project_optional_safe_identifier(value: impl AsRef<str>) -> Option<SafeIdentifier> {
    SafeIdentifier::new(value).ok()
}

fn invalid_database(problem: GenericProjectDatabaseProblem) -> GenericProjectError {
    GenericProjectError::InvalidDatabase {
        problem,
        source: None,
    }
}

fn generic_project_resource_report(
    source: &GenericProjectResourceError,
    stage: GenericDiagnosticStage,
    effect: StateEffect,
) -> DiagnosticReport {
    let planning_resource = |resource, problem| {
        DiagnosticReport::new(
            effect,
            Diagnostic::translation(TranslationIssue::PlanningResource {
                resource,
                origin: TranslationPlanningResourceOrigin::ProjectSnapshot,
                problem,
            }),
        )
    };
    match source {
        GenericProjectResourceError::InvalidSnapshot { resource, source } => planning_resource(
            translation_planning_resource_kind(*resource),
            TranslationPlanningResourceProblem::InvalidSnapshotJson {
                category: translation_json_failure(source),
                line: source.line(),
                column: source.column(),
            },
        ),
        GenericProjectResourceError::SnapshotEncoding { resource, source } => planning_resource(
            translation_planning_resource_kind(*resource),
            TranslationPlanningResourceProblem::SnapshotEncodingJson {
                category: translation_json_failure(source),
                line: source.line(),
                column: source.column(),
            },
        ),
        GenericProjectResourceError::TerminologyDefinition(source) => planning_resource(
            TranslationPlanningResourceKind::Terminology,
            terminology_problem(source),
        ),
        GenericProjectResourceError::Placeholder(
            GenericPlaceholderError::InvalidResourceSnapshot(source),
        ) => planning_resource(
            TranslationPlanningResourceKind::PlaceholderRules,
            TranslationPlanningResourceProblem::InvalidSnapshotJson {
                category: translation_json_failure(source),
                line: source.line(),
                column: source.column(),
            },
        ),
        GenericProjectResourceError::Placeholder(GenericPlaceholderError::Compilation(source)) => {
            DiagnosticReport::new(
                effect,
                Diagnostic::translation(TranslationIssue::PlaceholderCompilation {
                    origin: TranslationPlanningResourceOrigin::ProjectSnapshot,
                    problem: source.diagnostic_problem(),
                }),
            )
        }
        GenericProjectResourceError::Placeholder(_) => DiagnosticReport::new(
            effect,
            Diagnostic::generic(GenericIssue::project(
                stage,
                GenericProblem::UnexpectedResourceState {
                    resource: GenericResourceKind::PlaceholderRules,
                },
            )),
        ),
        GenericProjectResourceError::NonCanonicalSnapshot { resource } => DiagnosticReport::new(
            effect,
            Diagnostic::generic(GenericIssue::project(
                stage,
                GenericProblem::NonCanonicalResourceSnapshot {
                    resource: *resource,
                },
            )),
        ),
    }
}

const fn translation_planning_resource_kind(
    resource: GenericResourceKind,
) -> TranslationPlanningResourceKind {
    match resource {
        GenericResourceKind::Terminology => TranslationPlanningResourceKind::Terminology,
        GenericResourceKind::PlaceholderRules => TranslationPlanningResourceKind::PlaceholderRules,
    }
}

fn file_system_project_report(
    stage: GenericDiagnosticStage,
    operation: FileSystemOperation,
    problem: FileSystemProblem,
    effect: StateEffect,
) -> DiagnosticReport {
    DiagnosticReport::new(
        effect,
        Diagnostic::file_system(FileSystemIssue::new(
            FileSystemDiagnosticContext::new(file_system_project_stage(stage), operation),
            problem,
        )),
    )
}

fn sqlite_project_report(
    stage: GenericDiagnosticStage,
    database: &Path,
    query_id: &'static str,
    source: &rusqlite::Error,
    operation: SqliteOperation,
    transaction: SqliteTransactionState,
    effect: StateEffect,
) -> DiagnosticReport {
    DiagnosticReport::new(
        effect,
        Diagnostic::sqlite(SqliteIssue::new(
            SqliteDiagnosticContext::new(sqlite_project_stage(stage), operation, transaction),
            SqliteProblem::Driver {
                database: SafePath::new(database),
                query_id: SafeIdentifier::new(query_id).ok(),
                query_ordinal: None,
                failure: SqliteDriverFailure::from_error(source),
            },
        )),
    )
}

const fn file_system_project_stage(stage: GenericDiagnosticStage) -> FileSystemDiagnosticStage {
    match stage {
        GenericDiagnosticStage::ProjectOpening => FileSystemDiagnosticStage::Project,
        GenericDiagnosticStage::Init => FileSystemDiagnosticStage::Project,
        GenericDiagnosticStage::Extract => FileSystemDiagnosticStage::Extract,
        GenericDiagnosticStage::Translate | GenericDiagnosticStage::TaskRecord => {
            FileSystemDiagnosticStage::Translate
        }
        GenericDiagnosticStage::WriteBack => FileSystemDiagnosticStage::WriteBack,
    }
}

const fn sqlite_project_stage(stage: GenericDiagnosticStage) -> SqliteDiagnosticStage {
    match stage {
        GenericDiagnosticStage::ProjectOpening => SqliteDiagnosticStage::Project,
        GenericDiagnosticStage::Init => SqliteDiagnosticStage::Init,
        GenericDiagnosticStage::Extract => SqliteDiagnosticStage::Extract,
        GenericDiagnosticStage::Translate | GenericDiagnosticStage::TaskRecord => {
            SqliteDiagnosticStage::Translate
        }
        GenericDiagnosticStage::WriteBack => SqliteDiagnosticStage::WriteBack,
    }
}

const fn generic_language_violation(source: &LanguageIdError) -> GenericLanguageViolation {
    match source {
        LanguageIdError::Blank => GenericLanguageViolation::Blank,
        LanguageIdError::SurroundingWhitespace { .. } => {
            GenericLanguageViolation::SurroundingWhitespace
        }
        LanguageIdError::Underscore { .. } => GenericLanguageViolation::Underscore,
        LanguageIdError::InvalidSyntax { .. } => GenericLanguageViolation::InvalidSyntax,
        LanguageIdError::InvalidRegistryTag { .. } => GenericLanguageViolation::InvalidRegistryTag,
        LanguageIdError::CanonicalizationFailed { .. } => {
            GenericLanguageViolation::CanonicalizationFailed
        }
        LanguageIdError::UndefinedPrimaryLanguage { .. } => {
            GenericLanguageViolation::UndefinedPrimaryLanguage
        }
    }
}

impl GenericProjectError {
    pub(crate) fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
            || matches!(self, Self::Jsonl(source) if source.is_cancelled())
    }

    fn is_sqlite_cancellation_without_cleanup_failure(&self) -> bool {
        match self {
            Self::Sqlite { source, .. } => {
                sqlite_error_is_busy(source) || sqlite_error_is_interrupted(source)
            }
            Self::Jsonl(source) => source.is_cancelled(),
            Self::Cancelled => true,
            Self::InitialCandidateCleanup { .. }
            | Self::MissingInitialField(_)
            | Self::WorkspaceNotDirectory { .. }
            | Self::SourceNotDirectory { .. }
            | Self::SourceWriteBackOverlap { .. }
            | Self::ProjectNotFound { .. }
            | Self::ProjectIdentityMismatch { .. }
            | Self::Io { .. }
            | Self::InitialDatabaseFileSystem { .. }
            | Self::InitialDatabaseOutcomeUnknown(_)
            | Self::TransactionNotCommitted { .. }
            | Self::TransactionOutcomeUnknown { .. }
            | Self::InvalidDatabase { .. }
            | Self::InvalidLanguage(_)
            | Self::SameSourceAndTargetLanguage { .. }
            | Self::InputChangedDuringExtract
            | Self::ExtractRequired
            | Self::TranslationSnapshotChanged
            | Self::InvalidTranslation { .. }
            | Self::DuplicateTranslationWrite { .. }
            | Self::DuplicateTranslationClear { .. }
            | Self::BlankProfileId
            | Self::InvalidResource(_)
            | Self::InvalidLayoutRules(_) => false,
        }
    }
}

fn sqlite_error_is_busy(source: &rusqlite::Error) -> bool {
    matches!(
        source.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

fn sqlite_error_is_interrupted(source: &rusqlite::Error) -> bool {
    matches!(
        source.sqlite_error_code(),
        Some(rusqlite::ErrorCode::OperationInterrupted)
    )
}

fn sqlite_operation_error(operation: &'static str, source: rusqlite::Error) -> GenericProjectError {
    if sqlite_error_is_interrupted(&source) {
        GenericProjectError::Cancelled
    } else {
        GenericProjectError::Sqlite { operation, source }
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

fn reconcile_snapshot(
    previous: &GenericStoredSnapshot,
    scanned: &GenericInputSnapshot,
    cancellation: &CooperativeCancellation,
) -> Result<ReconciledSnapshot, GenericProjectError> {
    let mut previous_groups = HashMap::<Sha256Fingerprint, Vec<&GenericStoredGroup>>::new();
    let mut previous_translation_count = 0;
    for file in &previous.files {
        ensure_generic_operation_not_cancelled(cancellation)?;
        for group in &file.groups {
            ensure_generic_operation_not_cancelled(cancellation)?;
            let fingerprint =
                lookup_text_fingerprint_with_cancellation(group.id.as_str(), cancellation)?;
            previous_groups.entry(fingerprint).or_default().push(group);
            for unit in &group.units {
                ensure_generic_operation_not_cancelled(cancellation)?;
                previous_translation_count += usize::from(unit.translation.is_some());
            }
        }
    }

    let mut preserved_translations = 0;
    let mut files = Vec::with_capacity(scanned.files().len());
    for (file_ordinal, file) in scanned.files().iter().enumerate() {
        ensure_generic_operation_not_cancelled(cancellation)?;
        let mut groups = Vec::with_capacity(file.groups().len());
        for (group_ordinal, group) in file.groups().iter().enumerate() {
            ensure_generic_operation_not_cancelled(cancellation)?;
            let context_fingerprint = group_context_fingerprint(
                group.kind(),
                group.units().iter().map(|unit| unit.text()),
                Some(cancellation),
            )?;
            let previous_group =
                find_previous_group_with_cancellation(&previous_groups, group.id(), cancellation)?;
            let mut previous_units = HashMap::<Sha256Fingerprint, Vec<&GenericStoredUnit>>::new();
            if let Some(previous_group) = previous_group {
                for previous_unit in &previous_group.units {
                    ensure_generic_operation_not_cancelled(cancellation)?;
                    let fingerprint = lookup_text_fingerprint_with_cancellation(
                        previous_unit.id.as_str(),
                        cancellation,
                    )?;
                    previous_units
                        .entry(fingerprint)
                        .or_default()
                        .push(previous_unit);
                }
            }
            let mut units = Vec::with_capacity(group.units().len());
            for (unit_ordinal, unit) in group.units().iter().enumerate() {
                ensure_generic_operation_not_cancelled(cancellation)?;
                let previous_unit =
                    find_previous_unit_with_cancellation(&previous_units, unit.id(), cancellation)?;
                let translation = match previous_unit {
                    Some(old)
                        if bytes_equal_with_cancellation(
                            old.source_text.as_bytes(),
                            unit.text().as_bytes(),
                            cancellation,
                        )? =>
                    {
                        old.translation
                            .as_ref()
                            .map(|translation| {
                                clone_stored_translation_with_cancellation(
                                    translation,
                                    cancellation,
                                )
                            })
                            .transpose()?
                    }
                    _ => None,
                };
                let rejected = match previous_unit {
                    Some(old)
                        if bytes_equal_with_cancellation(
                            old.source_text.as_bytes(),
                            unit.text().as_bytes(),
                            cancellation,
                        )? =>
                    {
                        old.rejected
                            .as_ref()
                            .map(|rejected| {
                                clone_stored_rejected_translation_with_cancellation(
                                    rejected,
                                    cancellation,
                                )
                            })
                            .transpose()?
                    }
                    _ => None,
                };
                if translation.is_some() {
                    preserved_translations += 1;
                }
                units.push(GenericStoredUnit {
                    id: clone_text_with_cancellation(unit.id(), cancellation)?,
                    ordinal: unit_ordinal,
                    source_text: clone_text_with_cancellation(unit.text(), cancellation)?,
                    translation,
                    rejected,
                });
            }
            groups.push(GenericStoredGroup {
                id: clone_text_with_cancellation(group.id(), cancellation)?,
                ordinal: group_ordinal,
                kind: clone_text_with_cancellation(group.kind(), cancellation)?,
                context_fingerprint,
                units,
            });
        }
        files.push(GenericStoredFile {
            relative_path: file.relative_path().to_path_buf(),
            ordinal: file_ordinal,
            groups,
        });
    }

    Ok(ReconciledSnapshot {
        files,
        preserved_translations,
        cleared_translations: previous_translation_count.saturating_sub(preserved_translations),
    })
}

fn clone_stored_translation_with_cancellation(
    translation: &GenericStoredTranslation,
    cancellation: &CooperativeCancellation,
) -> Result<GenericStoredTranslation, GenericProjectError> {
    Ok(GenericStoredTranslation {
        translation: clone_text_with_cancellation(&translation.translation, cancellation)?,
        origin: translation.origin,
        state_fingerprint: translation.state_fingerprint,
    })
}

fn clone_stored_rejected_translation_with_cancellation(
    rejected: &GenericStoredRejectedTranslation,
    cancellation: &CooperativeCancellation,
) -> Result<GenericStoredRejectedTranslation, GenericProjectError> {
    let mut source = Vec::with_capacity(rejected.source.len());
    for line in &rejected.source {
        source.push(clone_text_with_cancellation(line, cancellation)?);
    }
    let translation = rejected
        .translation
        .as_ref()
        .map(|lines| {
            let mut cloned = Vec::with_capacity(lines.len());
            for line in lines {
                cloned.push(clone_text_with_cancellation(line, cancellation)?);
            }
            Ok::<_, GenericProjectError>(cloned)
        })
        .transpose()?;
    Ok(GenericStoredRejectedTranslation {
        readable_id: clone_text_with_cancellation(&rejected.readable_id, cancellation)?,
        origin: rejected.origin,
        source,
        candidate_json: clone_text_with_cancellation(&rejected.candidate_json, cancellation)?,
        translation,
        group_context: rejected.group_context,
        violation: rejected.violation.clone(),
        planning_state: rejected.planning_state,
    })
}

fn find_previous_unit_with_cancellation<'a>(
    previous_units: &HashMap<Sha256Fingerprint, Vec<&'a GenericStoredUnit>>,
    unit_id: &str,
    cancellation: &CooperativeCancellation,
) -> Result<Option<&'a GenericStoredUnit>, GenericProjectError> {
    let fingerprint = lookup_text_fingerprint_with_cancellation(unit_id, cancellation)?;
    let Some(candidates) = previous_units.get(&fingerprint) else {
        return Ok(None);
    };
    for candidate in candidates {
        if bytes_equal_with_cancellation(candidate.id.as_bytes(), unit_id.as_bytes(), cancellation)?
        {
            return Ok(Some(*candidate));
        }
    }
    Ok(None)
}

fn find_previous_group_with_cancellation<'a>(
    previous_groups: &HashMap<Sha256Fingerprint, Vec<&'a GenericStoredGroup>>,
    group_id: &str,
    cancellation: &CooperativeCancellation,
) -> Result<Option<&'a GenericStoredGroup>, GenericProjectError> {
    let fingerprint = lookup_text_fingerprint_with_cancellation(group_id, cancellation)?;
    let Some(candidates) = previous_groups.get(&fingerprint) else {
        return Ok(None);
    };
    for candidate in candidates {
        if bytes_equal_with_cancellation(
            candidate.id.as_bytes(),
            group_id.as_bytes(),
            cancellation,
        )? {
            return Ok(Some(*candidate));
        }
    }
    Ok(None)
}

fn lookup_text_fingerprint_with_cancellation(
    value: &str,
    cancellation: &CooperativeCancellation,
) -> Result<Sha256Fingerprint, GenericProjectError> {
    let mut hasher = Sha256FramedHasher::new(b"att.generic.lookup-text");
    hasher.try_frame_chunks(
        1,
        value.as_bytes(),
        FINGERPRINT_CANCELLATION_CHECK_BYTES,
        || ensure_generic_operation_not_cancelled(cancellation),
    )?;
    Ok(hasher.finish())
}

fn group_context_fingerprint<'a>(
    kind: &str,
    texts: impl IntoIterator<Item = &'a str>,
    cancellation: Option<&CooperativeCancellation>,
) -> Result<Sha256Fingerprint, GenericProjectError> {
    let mut hasher = Sha256FramedHasher::new(b"att.generic.group-context");
    frame_group_context_bytes(&mut hasher, 1, kind.as_bytes(), cancellation)?;
    for text in texts {
        frame_group_context_bytes(&mut hasher, 2, text.as_bytes(), cancellation)?;
    }
    Ok(hasher.finish())
}

fn frame_group_context_bytes(
    hasher: &mut Sha256FramedHasher,
    tag: u8,
    bytes: &[u8],
    cancellation: Option<&CooperativeCancellation>,
) -> Result<(), GenericProjectError> {
    match cancellation {
        Some(cancellation) => {
            hasher.try_frame_chunks(tag, bytes, FINGERPRINT_CANCELLATION_CHECK_BYTES, || {
                ensure_generic_operation_not_cancelled(cancellation)
            })?;
        }
        None => {
            hasher.frame(tag, bytes);
        }
    }
    Ok(())
}

fn scan_current_input(
    stored: &GenericStoredSnapshot,
    cancellation: &CooperativeCancellation,
) -> Result<GenericInputSnapshot, GenericProjectError> {
    let live = scan_input_tree_with_cancellation(stored.project.source_root(), cancellation)?;
    if Some(live.raw_fingerprint()) != stored.project.extracted_raw_fingerprint
        || Some(live.asset_fingerprint()) != stored.project.extracted_asset_fingerprint
    {
        return Err(GenericProjectError::ExtractRequired);
    }
    validate_stored_assets_match_live(stored, &live, Some(cancellation))?;
    Ok(live)
}

/// 候选已经建立后再次完整扫描外部输入，并同时比较原始与资产指纹。
///
/// 调用方在同一项目排他租约内持有首次完整验证得到的项目事实，因此这里不再打开
/// 64 MB 级项目数据库或重复执行 SQLite 完整性检查。
#[cfg(test)]
pub(crate) fn ensure_input_fingerprints_current(
    project: &GenericProject,
) -> Result<(), GenericProjectError> {
    ensure_input_fingerprints_current_with_cancellation(
        project,
        &CooperativeCancellation::default(),
    )
}

pub(crate) fn ensure_input_fingerprints_current_with_cancellation(
    project: &GenericProject,
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericProjectError> {
    if cancellation.is_requested() {
        return Err(GenericProjectError::Cancelled);
    }
    let expected_raw_fingerprint = project
        .extracted_raw_fingerprint
        .ok_or(GenericProjectError::ExtractRequired)?;
    let expected_asset_fingerprint = project
        .extracted_asset_fingerprint
        .ok_or(GenericProjectError::ExtractRequired)?;
    let live = scan_input_tree_with_cancellation(project.source_root(), cancellation)?;
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
    cancellation: Option<&CooperativeCancellation>,
) -> Result<(), GenericProjectError> {
    let no_cancellation = CooperativeCancellation::default();
    let comparison_cancellation = cancellation.unwrap_or(&no_cancellation);
    if stored.files.len() != live.files().len() {
        return Err(invalid_database(
            GenericProjectDatabaseProblem::SnapshotFileCount {
                stored: stored.files.len(),
                extracted: live.files().len(),
            },
        ));
    }
    for (file_ordinal, (stored_file, live_file)) in
        stored.files.iter().zip(live.files()).enumerate()
    {
        if let Some(cancellation) = cancellation {
            ensure_generic_operation_not_cancelled(cancellation)?;
        }
        if stored_file.ordinal != file_ordinal
            || stored_file.relative_path != live_file.relative_path()
            || stored_file.groups.len() != live_file.groups().len()
        {
            return Err(invalid_database(
                GenericProjectDatabaseProblem::SnapshotFileMismatch {
                    relative_path: SafePath::new(live_file.relative_path()),
                },
            ));
        }
        for (group_ordinal, (stored_group, live_group)) in stored_file
            .groups
            .iter()
            .zip(live_file.groups())
            .enumerate()
        {
            if let Some(cancellation) = cancellation {
                ensure_generic_operation_not_cancelled(cancellation)?;
            }
            let expected_context = group_context_fingerprint(
                live_group.kind(),
                live_group.units().iter().map(|unit| unit.text()),
                cancellation,
            )?;
            let group_id_matches = bytes_equal_with_cancellation(
                stored_group.id.as_bytes(),
                live_group.id().as_bytes(),
                comparison_cancellation,
            )?;
            let group_kind_matches = bytes_equal_with_cancellation(
                stored_group.kind.as_bytes(),
                live_group.kind().as_bytes(),
                comparison_cancellation,
            )?;
            if stored_group.ordinal != group_ordinal
                || !group_id_matches
                || !group_kind_matches
                || stored_group.context_fingerprint != expected_context
                || stored_group.units.len() != live_group.units().len()
            {
                return Err(invalid_database(
                    GenericProjectDatabaseProblem::SnapshotGroupMismatch {
                        relative_path: SafePath::new(live_file.relative_path()),
                        group_id: project_optional_safe_identifier(live_group.id()),
                    },
                ));
            }
            for (unit_ordinal, (stored_unit, live_unit)) in stored_group
                .units
                .iter()
                .zip(live_group.units())
                .enumerate()
            {
                if let Some(cancellation) = cancellation {
                    ensure_generic_operation_not_cancelled(cancellation)?;
                }
                let unit_id_matches = bytes_equal_with_cancellation(
                    stored_unit.id.as_bytes(),
                    live_unit.id().as_bytes(),
                    comparison_cancellation,
                )?;
                let source_text_matches = bytes_equal_with_cancellation(
                    stored_unit.source_text.as_bytes(),
                    live_unit.text().as_bytes(),
                    comparison_cancellation,
                )?;
                if stored_unit.ordinal != unit_ordinal || !unit_id_matches || !source_text_matches {
                    return Err(invalid_database(
                        GenericProjectDatabaseProblem::SnapshotUnitMismatch {
                            relative_path: SafePath::new(live_file.relative_path()),
                            group_id: project_optional_safe_identifier(live_group.id()),
                            unit_id: project_optional_safe_identifier(live_unit.id()),
                        },
                    ));
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
    cancellation: &CooperativeCancellation,
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
                             translation, translation_state
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "写入 Generic Unit",
                source,
            })?;
        let mut rejected_statement = transaction
            .prepare_cached(
                "INSERT INTO generic_rejected_translation (
                     group_id, unit_id, readable_id, origin, source_json, candidate_json,
                     translation_shape, group_context, violation_json, planning_state
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'free', ?7, ?8, ?9)",
            )
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "写入 Generic 被拒候选",
                source,
            })?;

        for file in files {
            ensure_generic_operation_not_cancelled(cancellation)?;
            let relative_path = encode_path(&file.relative_path);
            file_statement
                .execute(params![&relative_path, to_i64(file.ordinal)?])
                .map_err(|source| GenericProjectError::Sqlite {
                    operation: "写入 Generic 文件",
                    source,
                })?;
            for group in &file.groups {
                ensure_generic_operation_not_cancelled(cancellation)?;
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
                    ensure_generic_operation_not_cancelled(cancellation)?;
                    // 人工译文由独立表持有，并按当前位置重新计算适用性。把加载时覆盖在
                    // Unit 上的人工正文写入自动列，会在文件移动等身份变化后把本应过期的
                    // 人工译文伪装成 Current 自动译文。
                    let (translation, state) = unit
                        .translation
                        .as_ref()
                        .filter(|translation| translation.origin == TranslationOrigin::Automatic)
                        .map_or((None, None), |translation| {
                            (
                                Some(translation.translation.as_str()),
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
                            state
                        ])
                        .map_err(|source| GenericProjectError::Sqlite {
                            operation: "写入 Generic Unit",
                            source,
                        })?;
                    if let Some(rejected) = &unit.rejected {
                        rejected_statement
                            .execute(params![
                                group.id,
                                unit.id,
                                rejected.readable_id,
                                rejected.origin.storage_name(),
                                serde_json::to_string(&rejected.source)
                                    .expect("Generic 被拒候选原文必须可以编码"),
                                rejected.candidate_json,
                                rejected.group_context.as_bytes().as_slice(),
                                serde_json::to_string(&rejected.violation)
                                    .expect("Generic 被拒原因必须可以编码"),
                                rejected.planning_state.as_bytes().as_slice(),
                            ])
                            .map_err(|source| GenericProjectError::Sqlite {
                                operation: "写入 Generic 被拒候选",
                                source,
                            })?;
                    }
                }
            }
        }
    }
    ensure_generic_operation_not_cancelled(cancellation)?;
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

fn ensure_generic_operation_not_cancelled(
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericProjectError> {
    if cancellation.is_requested() {
        Err(GenericProjectError::Cancelled)
    } else {
        Ok(())
    }
}

fn load_translation_resources_rows_with_cancellation(
    connection: &Connection,
    cancellation: &CooperativeCancellation,
) -> Result<TranslationResources, GenericProjectError> {
    Ok(TranslationResources {
        terminology_json: load_translation_resource_row_with_cancellation(
            connection,
            TERMINOLOGY_RESOURCE,
            cancellation,
        )?,
        placeholder_rules_json: load_translation_resource_row_with_cancellation(
            connection,
            PLACEHOLDER_RULES_RESOURCE,
            cancellation,
        )?,
    })
}

fn load_translation_resource_row_with_cancellation(
    connection: &Connection,
    kind: &'static str,
    cancellation: &CooperativeCancellation,
) -> Result<String, GenericProjectError> {
    const OPERATION: &str = "读取 Generic 翻译资源";

    ensure_generic_operation_not_cancelled(cancellation)?;
    let mut statement = connection
        .prepare(
            "SELECT canonical_json FROM main.translation_resource
             WHERE resource_kind = ?1",
        )
        .map_err(|source| sqlite_operation_error(OPERATION, source))?;
    let mut rows = statement
        .query([kind])
        .map_err(|source| sqlite_operation_error(OPERATION, source))?;
    let row = rows
        .next()
        .map_err(|source| sqlite_operation_error(OPERATION, source))?
        .ok_or(GenericProjectError::Sqlite {
            operation: OPERATION,
            source: rusqlite::Error::QueryReturnedNoRows,
        })?;
    let canonical_json =
        clone_sqlite_text_column_with_cancellation(row, 0, OPERATION, cancellation)?;
    drop(rows);
    drop(statement);
    ensure_generic_operation_not_cancelled(cancellation)?;
    Ok(canonical_json)
}

fn query_optional_first_text_with_cancellation(
    connection: &Connection,
    query: &'static str,
    operation: &'static str,
    cancellation: &CooperativeCancellation,
) -> Result<Option<String>, GenericProjectError> {
    ensure_generic_operation_not_cancelled(cancellation)?;
    let mut statement = connection
        .prepare(query)
        .map_err(|source| sqlite_operation_error(operation, source))?;
    let mut rows = statement
        .query([])
        .map_err(|source| sqlite_operation_error(operation, source))?;
    let value = match rows
        .next()
        .map_err(|source| sqlite_operation_error(operation, source))?
    {
        Some(row) => Some(clone_sqlite_text_column_with_cancellation(
            row,
            0,
            operation,
            cancellation,
        )?),
        None => None,
    };
    drop(rows);
    drop(statement);
    ensure_generic_operation_not_cancelled(cancellation)?;
    Ok(value)
}

fn load_snapshot_rows(
    connection: &Connection,
    project: &GenericProject,
    cancellation: &CooperativeCancellation,
) -> Result<GenericStoredSnapshot, GenericProjectError> {
    let mut files = Vec::new();
    let mut file_indexes = HashMap::new();
    let mut file_statement = connection
        .prepare(
            "SELECT relative_path, ordinal
             FROM main.generic_file ORDER BY ordinal",
        )
        .map_err(|source| sqlite_operation_error("准备读取 Generic 文件", source))?;
    let mut file_rows = file_statement
        .query([])
        .map_err(|source| sqlite_operation_error("读取 Generic 文件", source))?;
    while let Some(row) = file_rows
        .next()
        .map_err(|source| sqlite_operation_error("解码 Generic 文件记录", source))?
    {
        ensure_generic_operation_not_cancelled(cancellation)?;
        let path_bytes = clone_sqlite_blob_column_with_cancellation(
            row,
            0,
            "解码 Generic 文件记录",
            cancellation,
        )?;
        let ordinal = row
            .get::<_, i64>(1)
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "解码 Generic 文件记录",
                source,
            })?;
        let relative_path = decode_path_with_cancellation(&path_bytes, cancellation)?;
        ensure_generic_operation_not_cancelled(cancellation)?;
        let file_index = files.len();
        file_indexes.insert(path_bytes, file_index);
        files.push(GenericStoredFile {
            relative_path,
            ordinal: from_i64(ordinal, "file.ordinal")?,
            groups: Vec::new(),
        });
    }
    drop(file_rows);
    drop(file_statement);

    let mut group_indexes = CancellableTextMap::with_capacity(files.len());
    let mut group_statement = connection
        .prepare(
            "SELECT g.relative_path, g.group_id, g.ordinal,
                    g.kind, g.context_fingerprint
             FROM main.generic_group AS g
             JOIN main.generic_file AS f
               ON f.relative_path = g.relative_path
             ORDER BY f.ordinal, g.ordinal",
        )
        .map_err(|source| sqlite_operation_error("准备读取 Generic Group", source))?;
    let mut group_rows = group_statement
        .query([])
        .map_err(|source| sqlite_operation_error("读取 Generic Group", source))?;
    while let Some(row) = group_rows
        .next()
        .map_err(|source| sqlite_operation_error("解码 Generic Group", source))?
    {
        ensure_generic_operation_not_cancelled(cancellation)?;
        let path_bytes =
            clone_sqlite_blob_column_with_cancellation(row, 0, "解码 Generic Group", cancellation)?;
        let group_id =
            clone_sqlite_text_column_with_cancellation(row, 1, "解码 Generic Group", cancellation)?;
        let group_ordinal = row
            .get::<_, i64>(2)
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "解码 Generic Group",
                source,
            })?;
        let kind =
            clone_sqlite_text_column_with_cancellation(row, 3, "解码 Generic Group", cancellation)?;
        let context =
            clone_sqlite_blob_column_with_cancellation(row, 4, "解码 Generic Group", cancellation)?;
        ensure_generic_operation_not_cancelled(cancellation)?;
        let Some(&file_index) = file_indexes.get(&path_bytes) else {
            return Err(invalid_database(
                GenericProjectDatabaseProblem::GroupReferencesMissingFile {
                    group_id: project_optional_safe_identifier(&group_id),
                },
            ));
        };
        let group_index = files[file_index].groups.len();
        let group_index_key = clone_text_with_cancellation(&group_id, cancellation)?;
        let previous = group_indexes.insert_with_cancellation(
            group_index_key,
            (file_index, group_index),
            || ensure_generic_operation_not_cancelled(cancellation),
        )?;
        debug_assert!(previous.is_none());
        files[file_index].groups.push(GenericStoredGroup {
            id: group_id,
            ordinal: from_i64(group_ordinal, "group.ordinal")?,
            kind,
            context_fingerprint: read_fingerprint(context, "context_fingerprint")?,
            units: Vec::new(),
        });
    }
    drop(group_rows);
    drop(group_statement);

    let mut unit_statement = connection
        .prepare(LOAD_UNITS_NATURAL_SQL)
        .map_err(|source| sqlite_operation_error("准备读取 Generic Unit", source))?;
    let mut unit_rows = unit_statement
        .query([])
        .map_err(|source| sqlite_operation_error("读取 Generic Unit", source))?;
    while let Some(row) = unit_rows
        .next()
        .map_err(|source| sqlite_operation_error("解码 Generic Unit", source))?
    {
        ensure_generic_operation_not_cancelled(cancellation)?;
        let group_id =
            clone_sqlite_text_column_with_cancellation(row, 0, "解码 Generic Unit", cancellation)?;
        let unit_id =
            clone_sqlite_text_column_with_cancellation(row, 1, "解码 Generic Unit", cancellation)?;
        let unit_ordinal = row
            .get::<_, i64>(2)
            .map_err(|source| GenericProjectError::Sqlite {
                operation: "解码 Generic Unit",
                source,
            })?;
        let source_text =
            clone_sqlite_text_column_with_cancellation(row, 3, "解码 Generic Unit", cancellation)?;
        let translation = clone_optional_sqlite_text_column_with_cancellation(
            row,
            4,
            "解码 Generic Unit",
            cancellation,
        )?;
        let state = clone_optional_sqlite_blob_column_with_cancellation(
            row,
            5,
            "解码 Generic Unit",
            cancellation,
        )?;
        let automatic_translation = match (translation, state) {
            (None, None) => None,
            (Some(translation), Some(state)) => Some(GenericStoredTranslation {
                translation,
                origin: TranslationOrigin::Automatic,
                state_fingerprint: read_fingerprint(state, "translation_state")?,
            }),
            _ => {
                return Err(invalid_database(
                    GenericProjectDatabaseProblem::IncompleteTranslationState {
                        group_id: project_optional_safe_identifier(&group_id),
                        unit_id: project_optional_safe_identifier(&unit_id),
                    },
                ));
            }
        };
        let manual_translation_json = clone_optional_sqlite_text_column_with_cancellation(
            row,
            6,
            "解码 Generic 人工译文",
            cancellation,
        )?;
        let manual_state = clone_optional_sqlite_blob_column_with_cancellation(
            row,
            7,
            "解码 Generic 人工译文",
            cancellation,
        )?;
        let rejected_readable_id = clone_optional_sqlite_text_column_with_cancellation(
            row,
            8,
            "解码 Generic 被拒候选",
            cancellation,
        )?;
        let rejected_origin = clone_optional_sqlite_text_column_with_cancellation(
            row,
            9,
            "解码 Generic 被拒候选",
            cancellation,
        )?;
        let rejected_source_json = clone_optional_sqlite_text_column_with_cancellation(
            row,
            10,
            "解码 Generic 被拒候选",
            cancellation,
        )?;
        let rejected_candidate_json = clone_optional_sqlite_text_column_with_cancellation(
            row,
            11,
            "解码 Generic 被拒候选",
            cancellation,
        )?;
        let rejected_shape = clone_optional_sqlite_text_column_with_cancellation(
            row,
            12,
            "解码 Generic 被拒候选",
            cancellation,
        )?;
        let rejected_group_context = clone_optional_sqlite_blob_column_with_cancellation(
            row,
            13,
            "解码 Generic 被拒候选",
            cancellation,
        )?;
        let rejected_violation_json = clone_optional_sqlite_text_column_with_cancellation(
            row,
            14,
            "解码 Generic 被拒候选",
            cancellation,
        )?;
        let rejected_planning_state = clone_optional_sqlite_blob_column_with_cancellation(
            row,
            15,
            "解码 Generic 被拒候选",
            cancellation,
        )?;
        ensure_generic_operation_not_cancelled(cancellation)?;
        let Some(&(file_index, group_index)) = group_indexes
            .get_with_cancellation(&group_id, || {
                ensure_generic_operation_not_cancelled(cancellation)
            })?
        else {
            return Err(invalid_database(
                GenericProjectDatabaseProblem::UnitReferencesMissingGroup {
                    group_id: project_optional_safe_identifier(&group_id),
                    unit_id: project_optional_safe_identifier(&unit_id),
                },
            ));
        };
        let group = &files[file_index].groups[group_index];
        let source_lines = source_text
            .split('\n')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let readable_path = files[file_index]
            .relative_path
            .to_string_lossy()
            .replace('\\', "/");
        let expected_manual_state = crate::manual::generic_manual_applicability(
            &group_id,
            &unit_id,
            &readable_path,
            group.kind(),
            project.language_pair().source().as_str(),
            project.language_pair().target().as_str(),
            &source_lines,
        );
        let manual_translation = match (manual_translation_json, manual_state) {
            (None, None) => None,
            (Some(translation_json), Some(state)) => {
                let state = read_fingerprint(state, "applicability_fingerprint")?;
                if state != expected_manual_state {
                    None
                } else {
                    let lines =
                        serde_json::from_str::<Vec<String>>(&translation_json).map_err(|_| {
                            invalid_database(
                                GenericProjectDatabaseProblem::ManualTranslationStateFailure,
                            )
                        })?;
                    if lines.is_empty()
                        || lines.iter().any(|line| {
                            line.chars()
                                .any(|character| matches!(character, '\r' | '\n' | '\0'))
                        })
                    {
                        return Err(invalid_database(
                            GenericProjectDatabaseProblem::ManualTranslationStateFailure,
                        ));
                    }
                    Some(GenericStoredTranslation {
                        translation: lines.join("\n"),
                        origin: TranslationOrigin::Manual,
                        state_fingerprint: expected_manual_state,
                    })
                }
            }
            _ => {
                return Err(invalid_database(
                    GenericProjectDatabaseProblem::ManualTranslationStateFailure,
                ));
            }
        };
        let translation = manual_translation.or(automatic_translation);
        let rejected = match (
            rejected_readable_id,
            rejected_origin,
            rejected_source_json,
            rejected_candidate_json,
            rejected_shape,
            rejected_group_context,
            rejected_violation_json,
            rejected_planning_state,
        ) {
            (None, None, None, None, None, None, None, None) => None,
            (
                Some(readable_id),
                Some(origin),
                Some(source_json),
                Some(candidate_json),
                Some(shape),
                Some(group_context),
                Some(violation_json),
                Some(planning_state),
            ) if shape == "free" => {
                let origin = TranslationOrigin::from_storage_name(&origin).ok_or_else(|| {
                    invalid_database(GenericProjectDatabaseProblem::ManualTranslationStateFailure)
                })?;
                let source = serde_json::from_str::<Vec<String>>(&source_json).map_err(|_| {
                    invalid_database(GenericProjectDatabaseProblem::ManualTranslationStateFailure)
                })?;
                let translation = serde_json::from_str::<Vec<String>>(&candidate_json)
                    .ok()
                    .filter(|translation| !translation.is_empty());
                let violation = serde_json::from_str::<ProvenInvariantViolation>(&violation_json)
                    .map_err(|_| {
                    invalid_database(GenericProjectDatabaseProblem::ManualTranslationStateFailure)
                })?;
                if source.is_empty() {
                    return Err(invalid_database(
                        GenericProjectDatabaseProblem::ManualTranslationStateFailure,
                    ));
                }
                Some(GenericStoredRejectedTranslation {
                    readable_id,
                    origin,
                    source,
                    candidate_json,
                    translation,
                    group_context: read_fingerprint(group_context, "group_context")?,
                    violation,
                    planning_state: read_fingerprint(planning_state, "planning_state")?,
                })
            }
            _ => {
                return Err(invalid_database(
                    GenericProjectDatabaseProblem::ManualTranslationStateFailure,
                ));
            }
        };
        files[file_index].groups[group_index]
            .units
            .push(GenericStoredUnit {
                id: unit_id,
                ordinal: from_i64(unit_ordinal, "unit.ordinal")?,
                source_text,
                translation,
                rejected,
            });
    }
    drop(unit_rows);
    ensure_generic_operation_not_cancelled(cancellation)?;
    Ok(GenericStoredSnapshot {
        project: project.clone(),
        files,
    })
}

fn create_initial_schema(
    connection: &mut AttSqliteCancellableConnection,
    project_name: &ProjectName,
    source_root: &Path,
    source_language: &LanguageId,
    target_language: &LanguageId,
    cancellation: &CooperativeCancellation,
    performance: &RunPerformanceCounters,
) -> Result<(), GenericProjectError> {
    run_cancellable_transaction(
        connection,
        cancellation,
        performance,
        SqliteTransactionScope::DatabaseInitialization,
        "开始建立 Generic schema",
        "提交 Generic schema",
        "回滚 Generic schema",
        |transaction| {
            transaction
                .execute_batch(CREATE_INITIAL_SCHEMA_SQL)
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
            Ok(())
        },
    )
}

#[derive(Debug, Eq, PartialEq)]
struct GenericSchemaObject {
    kind: String,
    name: String,
    table_name: String,
    sql: String,
}

fn read_generic_att_schema_with_cancellation(
    connection: &Connection,
    cancellation: &CooperativeCancellation,
) -> Result<Vec<GenericSchemaObject>, GenericProjectError> {
    const OPERATION: &str = "读取当前 Generic schema";

    ensure_generic_operation_not_cancelled(cancellation)?;
    let mut statement = connection
        .prepare(SELECT_GENERIC_ATT_SCHEMA)
        .map_err(|source| sqlite_operation_error(OPERATION, source))?;
    let mut rows = statement
        .query([])
        .map_err(|source| sqlite_operation_error(OPERATION, source))?;
    let mut objects = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|source| sqlite_operation_error(OPERATION, source))?
    {
        ensure_generic_operation_not_cancelled(cancellation)?;
        objects.push(GenericSchemaObject {
            kind: clone_sqlite_text_column_with_cancellation(row, 0, OPERATION, cancellation)?,
            name: clone_sqlite_text_column_with_cancellation(row, 1, OPERATION, cancellation)?,
            table_name: clone_sqlite_text_column_with_cancellation(
                row,
                2,
                OPERATION,
                cancellation,
            )?,
            sql: clone_sqlite_text_column_with_cancellation(row, 3, OPERATION, cancellation)?,
        });
    }
    drop(rows);
    drop(statement);
    ensure_generic_operation_not_cancelled(cancellation)?;
    Ok(objects)
}

fn expected_generic_att_schema() -> &'static [GenericSchemaObject] {
    static EXPECTED: OnceLock<Vec<GenericSchemaObject>> = OnceLock::new();
    EXPECTED.get_or_init(|| {
        let connection =
            Connection::open_in_memory().expect("当前 Generic schema 必须能在内存数据库中建立");
        connection
            .execute_batch(CREATE_INITIAL_SCHEMA_SQL)
            .expect("当前 Generic schema DDL 必须有效");
        read_generic_att_schema_with_cancellation(&connection, &CooperativeCancellation::default())
            .expect("必须能读取刚建立的当前 Generic schema")
    })
}

/// 检查调用方连接中的 ATT 受管对象是否与当前唯一 Generic schema 完全一致。
///
/// 查询只覆盖 ATT 管理的表及附属于这些表的显式 schema 对象；脚本自己的独立表、
/// 索引、视图和触发器不属于本契约。
#[cfg(test)]
pub(crate) fn validate_current_generic_schema(
    connection: &Connection,
) -> Result<(), GenericProjectError> {
    validate_current_generic_schema_with_cancellation(
        connection,
        &CooperativeCancellation::default(),
    )
}

pub(crate) fn validate_current_generic_schema_with_cancellation(
    connection: &Connection,
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericProjectError> {
    let actual = read_generic_att_schema_with_cancellation(connection, cancellation)?;
    let expected = expected_generic_att_schema();
    validate_generic_schema_objects_with_cancellation(&actual, expected, cancellation)
}

fn validate_generic_schema_objects_with_cancellation(
    actual: &[GenericSchemaObject],
    expected: &[GenericSchemaObject],
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericProjectError> {
    let mut missing = Vec::new();
    for expected_object in expected {
        ensure_generic_operation_not_cancelled(cancellation)?;
        let mut found = false;
        for actual_object in actual {
            ensure_generic_operation_not_cancelled(cancellation)?;
            if schema_object_identity_equal_with_cancellation(
                actual_object,
                expected_object,
                cancellation,
            )? {
                found = true;
                break;
            }
        }
        if !found {
            missing.push(schema_object_label_with_cancellation(
                expected_object,
                cancellation,
            )?);
        }
    }

    let mut definition_mismatches = Vec::new();
    for expected_object in expected {
        ensure_generic_operation_not_cancelled(cancellation)?;
        let mut matching = None;
        for actual_object in actual {
            ensure_generic_operation_not_cancelled(cancellation)?;
            if schema_object_identity_equal_with_cancellation(
                actual_object,
                expected_object,
                cancellation,
            )? {
                matching = Some(actual_object);
                break;
            }
        }
        if let Some(actual_object) = matching
            && !schema_object_equal_with_cancellation(actual_object, expected_object, cancellation)?
        {
            definition_mismatches.push(schema_object_label_with_cancellation(
                expected_object,
                cancellation,
            )?);
        }
    }

    let mut unexpected = Vec::new();
    for actual_object in actual {
        ensure_generic_operation_not_cancelled(cancellation)?;
        let mut found = false;
        for expected_object in expected {
            ensure_generic_operation_not_cancelled(cancellation)?;
            if schema_object_identity_equal_with_cancellation(
                actual_object,
                expected_object,
                cancellation,
            )? {
                found = true;
                break;
            }
        }
        if !found {
            unexpected.push(schema_object_label_with_cancellation(
                actual_object,
                cancellation,
            )?);
        }
    }

    if actual.len() == expected.len()
        && missing.is_empty()
        && definition_mismatches.is_empty()
        && unexpected.is_empty()
    {
        return Ok(());
    }
    ensure_generic_operation_not_cancelled(cancellation)?;
    Err(invalid_database(
        GenericProjectDatabaseProblem::SchemaMismatch {
            expected_count: expected.len(),
            actual_count: actual.len(),
            missing,
            definition_mismatches,
            unexpected,
        },
    ))
}

fn schema_object_identity_equal_with_cancellation(
    left: &GenericSchemaObject,
    right: &GenericSchemaObject,
    cancellation: &CooperativeCancellation,
) -> Result<bool, GenericProjectError> {
    Ok(
        bytes_equal_with_cancellation(left.kind.as_bytes(), right.kind.as_bytes(), cancellation)?
            && bytes_equal_with_cancellation(
                left.name.as_bytes(),
                right.name.as_bytes(),
                cancellation,
            )?,
    )
}

fn schema_object_equal_with_cancellation(
    left: &GenericSchemaObject,
    right: &GenericSchemaObject,
    cancellation: &CooperativeCancellation,
) -> Result<bool, GenericProjectError> {
    Ok(
        schema_object_identity_equal_with_cancellation(left, right, cancellation)?
            && bytes_equal_with_cancellation(
                left.table_name.as_bytes(),
                right.table_name.as_bytes(),
                cancellation,
            )?
            && bytes_equal_with_cancellation(
                left.sql.as_bytes(),
                right.sql.as_bytes(),
                cancellation,
            )?,
    )
}

fn schema_object_label_with_cancellation(
    object: &GenericSchemaObject,
    cancellation: &CooperativeCancellation,
) -> Result<SafeIdentifier, GenericProjectError> {
    let mut label = String::new();
    append_text_with_cancellation(&mut label, &object.kind, cancellation)?;
    append_text_with_cancellation(&mut label, "/", cancellation)?;
    append_text_with_cancellation(&mut label, &object.name, cancellation)?;
    ensure_generic_operation_not_cancelled(cancellation)?;
    Ok(project_safe_identifier(label, "schema_object"))
}

fn validate_schema_with_cancellation(
    connection: &Connection,
    cancellation: &CooperativeCancellation,
) -> Result<GenericCompiledTranslationResources, GenericProjectError> {
    validate_schema_with_compiled_resources(connection, None, cancellation)
}

fn validate_schema_with_compiled_resources(
    connection: &Connection,
    compiled_resources: Option<(
        &GenericCompiledTerminologyResource,
        &GenericCompiledPlaceholderResource,
    )>,
    cancellation: &CooperativeCancellation,
) -> Result<GenericCompiledTranslationResources, GenericProjectError> {
    ensure_generic_operation_not_cancelled(cancellation)?;
    validate_current_generic_schema_with_cancellation(connection, cancellation)?;
    ensure_generic_operation_not_cancelled(cancellation)?;
    let resource_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM main.translation_resource
             WHERE resource_kind IN (
                 'terminology', 'placeholder_rules', 'write_back_layout_rules'
             )
               AND length(canonical_json) > 0",
            [],
            |row| row.get(0),
        )
        .map_err(|source| sqlite_operation_error("检查 Generic 翻译资源", source))?;
    if resource_count != 3 {
        return Err(invalid_database(
            GenericProjectDatabaseProblem::TranslationResourceCount {
                expected: 3,
                actual: resource_count,
            },
        ));
    }
    ensure_generic_operation_not_cancelled(cancellation)?;
    let layout_rules_json = load_translation_resource_row_with_cancellation(
        connection,
        LAYOUT_RULES_RESOURCE,
        cancellation,
    )?;
    LayoutRuleSet::from_canonical_json(&layout_rules_json)
        .map_err(GenericProjectError::InvalidLayoutRules)?;
    ensure_generic_operation_not_cancelled(cancellation)?;
    let resources = load_translation_resources_rows_with_cancellation(connection, cancellation)?;
    let compiled_resources = match compiled_resources {
        Some((terminology, placeholder)) => {
            if !bytes_equal_with_cancellation(
                resources.terminology_json().as_bytes(),
                terminology.canonical_json().as_bytes(),
                cancellation,
            )? || !bytes_equal_with_cancellation(
                resources.placeholder_rules_json().as_bytes(),
                placeholder.canonical_json().as_bytes(),
                cancellation,
            )? {
                return Err(invalid_database(
                    GenericProjectDatabaseProblem::CompiledTranslationResourcesMismatch,
                ));
            }
            GenericCompiledTranslationResources {
                terminology: terminology.clone(),
                placeholder: placeholder.clone(),
            }
        }
        None => {
            let (terminology_json, placeholder_rules_json) = resources.into_parts();
            compile_translation_resources_with_cancellation(
                terminology_json,
                placeholder_rules_json,
                cancellation,
            )?
        }
    };
    ensure_generic_operation_not_cancelled(cancellation)?;
    let foreign_key_issue = query_optional_first_text_with_cancellation(
        connection,
        "PRAGMA main.foreign_key_check",
        "检查 Generic 外键",
        cancellation,
    )?;
    if let Some(table) = foreign_key_issue {
        return Err(invalid_database(
            GenericProjectDatabaseProblem::ForeignKeyViolation {
                table: project_safe_identifier(table, "unknown_table"),
            },
        ));
    }
    ensure_generic_operation_not_cancelled(cancellation)?;
    let quick_check = query_optional_first_text_with_cancellation(
        connection,
        "PRAGMA main.quick_check",
        "检查 Generic SQLite 完整性",
        cancellation,
    )?
    .ok_or(GenericProjectError::Sqlite {
        operation: "检查 Generic SQLite 完整性",
        source: rusqlite::Error::QueryReturnedNoRows,
    })?;
    if quick_check != "ok" {
        return Err(invalid_database(
            GenericProjectDatabaseProblem::QuickCheckFailed,
        ));
    }
    ensure_generic_operation_not_cancelled(cancellation)?;
    Ok(compiled_resources)
}

fn resolve_source_root(path: &Path) -> Result<PathBuf, GenericProjectError> {
    if !path.is_dir() {
        return Err(GenericProjectError::SourceNotDirectory {
            path: path.to_path_buf(),
        });
    }
    fs::canonicalize(path).map_err(|source| GenericProjectError::Io {
        operation: FileSystemOperation::ResolveDirectory,
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
        operation: FileSystemOperation::ResolveDirectory,
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
                    operation: FileSystemOperation::ResolveDirectory,
                    path: absolute.clone(),
                    source: io::Error::new(io::ErrorKind::NotFound, "找不到可规范化的现存祖先目录"),
                })?;
                missing.push(component.to_os_string());
                cursor = cursor.parent().ok_or_else(|| GenericProjectError::Io {
                    operation: FileSystemOperation::ResolveDirectory,
                    path: absolute.clone(),
                    source: io::Error::new(io::ErrorKind::NotFound, "找不到可规范化的现存祖先目录"),
                })?;
            }
            Err(source) => {
                return Err(GenericProjectError::Io {
                    operation: FileSystemOperation::ResolveDirectory,
                    path: cursor.to_path_buf(),
                    source,
                });
            }
        }
    }
}

fn publish_initial_database_candidate(
    connection: AttSqliteCancellableConnection,
    candidate_file: fs::File,
    identity: FileIdentity,
    candidate_path: &Path,
    database_path: &Path,
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericProjectError> {
    const CHECKPOINT_OPERATION: &str = "收束 Generic 初始数据库 WAL";
    const JOURNAL_MODE_OPERATION: &str = "切换 Generic 初始数据库日志模式";
    const CLOSE_OPERATION: &str = "关闭 Generic 初始数据库候选";

    ensure_generic_operation_not_cancelled(cancellation)?;
    let (busy, log_frames, checkpointed_frames) = connection
        .query_row("PRAGMA main.wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(|source| sqlite_operation_error(CHECKPOINT_OPERATION, source))?;
    if busy != 0 || log_frames != checkpointed_frames {
        if cancellation.is_requested() {
            return Err(GenericProjectError::Cancelled);
        }
        let code = if busy != 0 {
            rusqlite::ffi::SQLITE_BUSY
        } else {
            rusqlite::ffi::SQLITE_ERROR
        };
        return Err(GenericProjectError::Sqlite {
            operation: CHECKPOINT_OPERATION,
            source: rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                Some(format!(
                    "WAL checkpoint 未完成：busy={busy}, log_frames={log_frames}, \
                     checkpointed_frames={checkpointed_frames}"
                )),
            ),
        });
    }

    ensure_generic_operation_not_cancelled(cancellation)?;
    let journal_mode = connection
        .query_row("PRAGMA main.journal_mode = DELETE", [], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|source| sqlite_operation_error(JOURNAL_MODE_OPERATION, source))?;
    if !journal_mode.eq_ignore_ascii_case("delete") {
        return Err(GenericProjectError::Sqlite {
            operation: JOURNAL_MODE_OPERATION,
            source: rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
                Some(format!(
                    "期望 journal_mode=delete，SQLite 实际返回 {journal_mode:?}"
                )),
            ),
        });
    }

    ensure_generic_operation_not_cancelled(cancellation)?;
    connection
        .close()
        .map_err(|source| GenericProjectError::Sqlite {
            operation: CLOSE_OPERATION,
            source,
        })?;
    ensure_generic_operation_not_cancelled(cancellation)?;
    ensure_initial_database_path_has_no_sidecars(
        candidate_path,
        FileSystemOperation::Metadata,
        "候选数据库切换到 DELETE 后仍存在 SQLite sidecar",
        cancellation,
    )?;
    ensure_initial_database_path_has_no_sidecars(
        database_path,
        FileSystemOperation::Metadata,
        "首次 Init 的发布目标旁存在不属于当前项目的 SQLite sidecar",
        cancellation,
    )?;
    let verify_identity = (|| {
        let current = pin_path_without_reparse(candidate_path)?;
        if FileIdentity::of(&candidate_file, candidate_path)? == identity
            && FileIdentity::of(current.file(), candidate_path)? == identity
        {
            Ok(())
        } else {
            Err(WindowsFsError::FileIdentityChanged {
                path: candidate_path.to_path_buf(),
            })
        }
    })();
    verify_identity.map_err(|source| {
        GenericProjectError::InitialDatabaseOutcomeUnknown(Box::new(
            initial_database_file_system_error(FileSystemOperation::Metadata, source),
        ))
    })?;
    drop(candidate_file);
    rename_without_replace_if_identity(candidate_path, database_path, identity).map_err(|source| {
        let unknown = matches!(source, WindowsFsError::RenameTargetUnconfirmed { .. });
        let source = initial_database_file_system_error(FileSystemOperation::Rename, source);
        if unknown {
            GenericProjectError::InitialDatabaseOutcomeUnknown(Box::new(source))
        } else {
            source
        }
    })
}

fn ensure_initial_database_path_has_no_sidecars(
    database_path: &Path,
    operation: FileSystemOperation,
    present_message: &'static str,
    cancellation: &CooperativeCancellation,
) -> Result<(), GenericProjectError> {
    for suffix in SQLITE_SIDECAR_SUFFIXES {
        ensure_generic_operation_not_cancelled(cancellation)?;
        let sidecar = sqlite_sidecar_path(database_path, suffix);
        match fs::symlink_metadata(&sidecar) {
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(GenericProjectError::Io {
                    operation,
                    path: sidecar,
                    source,
                });
            }
            Ok(_) => {
                return Err(GenericProjectError::Io {
                    operation,
                    path: sidecar,
                    source: io::Error::new(io::ErrorKind::AlreadyExists, present_message),
                });
            }
        }
    }
    ensure_generic_operation_not_cancelled(cancellation)
}

fn sqlite_sidecar_path(database_path: &Path, suffix: &str) -> PathBuf {
    let mut sidecar = database_path.as_os_str().to_os_string();
    sidecar.push(suffix);
    PathBuf::from(sidecar)
}

fn initial_database_file_system_error(
    operation: FileSystemOperation,
    source: WindowsFsError,
) -> GenericProjectError {
    GenericProjectError::InitialDatabaseFileSystem { operation, source }
}

fn observe_initial_database_sidecars(
    candidate_path: &Path,
    targets: &mut Vec<(PathBuf, FileIdentity)>,
) -> Result<(), GenericProjectError> {
    for suffix in SQLITE_SIDECAR_SUFFIXES {
        let path = sqlite_sidecar_path(candidate_path, suffix);
        let observed =
            pin_path_without_reparse(&path).and_then(|file| FileIdentity::of(file.file(), &path));
        match observed {
            Ok(identity) => targets.push((path, identity)),
            Err(WindowsFsError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(GenericProjectError::InitialDatabaseOutcomeUnknown(
                    Box::new(initial_database_file_system_error(
                        FileSystemOperation::Metadata,
                        source,
                    )),
                ));
            }
        }
    }
    Ok(())
}

fn cleanup_initial_database_candidate(
    targets: &[(PathBuf, FileIdentity)],
) -> Result<(), Vec<GenericProjectError>> {
    let mut failures = Vec::new();
    for (path, identity) in targets {
        match delete_regular_file_if_identity(path, *identity) {
            Ok(()) => {}
            Err(WindowsFsError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => failures.push(initial_database_file_system_error(
                FileSystemOperation::Remove,
                source,
            )),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures)
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

struct CancellableResourceJsonReader<'a> {
    remaining: &'a [u8],
    cancellation: &'a CooperativeCancellation,
    bytes_until_check: usize,
    cancelled: bool,
}

impl<'a> CancellableResourceJsonReader<'a> {
    fn new(remaining: &'a [u8], cancellation: &'a CooperativeCancellation) -> Self {
        Self {
            remaining,
            cancellation,
            bytes_until_check: 0,
            cancelled: false,
        }
    }
}

impl Read for CancellableResourceJsonReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.cancelled {
            // serde_json 在一次底层读取失败后仍可能请求更多输入。取消事实必须锁存，
            // 后续读取只重复不可重试的 I/O 错误，不能再次调用取消检查。
            return Err(io::Error::other("Generic 翻译资源 JSON 解析已取消"));
        }
        if output.is_empty() || self.remaining.is_empty() {
            return Ok(0);
        }
        if self.bytes_until_check == 0 {
            if ensure_generic_operation_not_cancelled(self.cancellation).is_err() {
                self.cancelled = true;
                return Err(io::Error::other("Generic 翻译资源 JSON 解析已取消"));
            }
            self.bytes_until_check = RESOURCE_CANCELLATION_CHECK_BYTES;
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

struct CancellableResourceJsonWriter<'a> {
    output: &'a mut Vec<u8>,
    cancellation: &'a CooperativeCancellation,
    bytes_until_check: usize,
    cancelled: bool,
}

impl<'a> CancellableResourceJsonWriter<'a> {
    fn new(output: &'a mut Vec<u8>, cancellation: &'a CooperativeCancellation) -> Self {
        Self {
            output,
            cancellation,
            bytes_until_check: 0,
            cancelled: false,
        }
    }
}

impl Write for CancellableResourceJsonWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.cancelled {
            // 与 reader 保持同一锁存语义，避免序列化器在首次错误后重复轮询取消。
            return Err(io::Error::other("Generic 翻译资源 JSON 编码已取消"));
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.bytes_until_check == 0 {
            if ensure_generic_operation_not_cancelled(self.cancellation).is_err() {
                self.cancelled = true;
                return Err(io::Error::other("Generic 翻译资源 JSON 编码已取消"));
            }
            self.bytes_until_check = RESOURCE_CANCELLATION_CHECK_BYTES;
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

fn validate_translation_resources_with_cancellation(
    terminology_json: &str,
    placeholder_rules_json: &str,
    cancellation: &CooperativeCancellation,
) -> Result<GenericCompiledTranslationResources, GenericProjectError> {
    let terminology_json = clone_text_with_cancellation(terminology_json, cancellation)?;
    let placeholder_rules_json =
        clone_text_with_cancellation(placeholder_rules_json, cancellation)?;
    compile_translation_resources_with_cancellation(
        terminology_json,
        placeholder_rules_json,
        cancellation,
    )
}

fn compile_translation_resources_with_cancellation(
    terminology_json: String,
    placeholder_rules_json: String,
    cancellation: &CooperativeCancellation,
) -> Result<GenericCompiledTranslationResources, GenericProjectError> {
    let terminology =
        compile_terminology_resource_with_cancellation(terminology_json, cancellation)?;
    let placeholder =
        compile_placeholder_resource_with_cancellation(placeholder_rules_json, cancellation)?;
    ensure_generic_operation_not_cancelled(cancellation)?;
    Ok(GenericCompiledTranslationResources {
        terminology,
        placeholder,
    })
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

fn compile_terminology_resource_with_cancellation(
    canonical_json: String,
    cancellation: &CooperativeCancellation,
) -> Result<GenericCompiledTerminologyResource, GenericProjectError> {
    ensure_generic_operation_not_cancelled(cancellation)?;
    let slice_reader = CancellableResourceJsonReader::new(canonical_json.as_bytes(), cancellation);
    let mut reader = BufReader::with_capacity(RESOURCE_CANCELLATION_CHECK_BYTES, slice_reader);
    let entries_result = serde_json::from_reader::<_, Vec<TerminologyEntry>>(&mut reader);
    let cancelled = reader.get_ref().cancelled;
    drop(reader);
    if cancelled {
        return Err(GenericProjectError::Cancelled);
    }
    ensure_generic_operation_not_cancelled(cancellation)?;
    let entries = entries_result.map_err(|source| {
        GenericProjectError::InvalidResource(GenericProjectResourceError::InvalidSnapshot {
            resource: GenericResourceKind::Terminology,
            source,
        })
    })?;

    let mut encoded = Vec::with_capacity(canonical_json.len());
    let (encode_result, cancelled) = {
        let mut writer = CancellableResourceJsonWriter::new(&mut encoded, cancellation);
        let result = serde_json::to_writer(&mut writer, &entries);
        (result, writer.cancelled)
    };
    if cancelled {
        return Err(GenericProjectError::Cancelled);
    }
    ensure_generic_operation_not_cancelled(cancellation)?;
    encode_result.map_err(|source| {
        GenericProjectError::InvalidResource(GenericProjectResourceError::SnapshotEncoding {
            resource: GenericResourceKind::Terminology,
            source,
        })
    })?;
    if !bytes_equal_with_cancellation(&encoded, canonical_json.as_bytes(), cancellation)? {
        return Err(GenericProjectError::InvalidResource(
            GenericProjectResourceError::NonCanonicalSnapshot {
                resource: GenericResourceKind::Terminology,
            },
        ));
    }

    let compiled = compile_terminology_with_cancellation(entries, &|| {
        ensure_generic_operation_not_cancelled(cancellation).is_err()
    })
    .map_err(terminology_resource_error)?;
    ensure_generic_operation_not_cancelled(cancellation)?;
    Ok(GenericCompiledTerminologyResource {
        canonical_json: Arc::new(canonical_json),
        compiled: Arc::new(compiled),
    })
}

fn terminology_resource_error(source: TerminologyDefinitionError) -> GenericProjectError {
    match source {
        TerminologyDefinitionError::Cancelled => GenericProjectError::Cancelled,
        source => GenericProjectError::InvalidResource(
            GenericProjectResourceError::TerminologyDefinition(source),
        ),
    }
}

fn compile_placeholder_resource_with_cancellation(
    canonical_json: String,
    cancellation: &CooperativeCancellation,
) -> Result<GenericCompiledPlaceholderResource, GenericProjectError> {
    let service = GenericPlaceholderService::default();
    let definitions = service
        .parse_canonical_json_with_cancellation(&canonical_json, || {
            ensure_generic_operation_not_cancelled(cancellation)
        })?
        .map_err(placeholder_resource_error)?;
    let mut encoded = Vec::with_capacity(canonical_json.len());
    let (encode_result, cancelled) = {
        let mut writer = CancellableResourceJsonWriter::new(&mut encoded, cancellation);
        let result = serde_json::to_writer(&mut writer, &definitions);
        (result, writer.cancelled)
    };
    if cancelled {
        return Err(GenericProjectError::Cancelled);
    }
    ensure_generic_operation_not_cancelled(cancellation)?;
    encode_result.map_err(|source| {
        GenericProjectError::InvalidResource(GenericProjectResourceError::SnapshotEncoding {
            resource: GenericResourceKind::PlaceholderRules,
            source,
        })
    })?;
    if !bytes_equal_with_cancellation(&encoded, canonical_json.as_bytes(), cancellation)? {
        return Err(GenericProjectError::InvalidResource(
            GenericProjectResourceError::NonCanonicalSnapshot {
                resource: GenericResourceKind::PlaceholderRules,
            },
        ));
    }
    let compiled = service
        .compile_with_cancellation(definitions, || {
            ensure_generic_operation_not_cancelled(cancellation)
        })?
        .map_err(placeholder_resource_error)?;
    ensure_generic_operation_not_cancelled(cancellation)?;
    Ok(GenericCompiledPlaceholderResource {
        canonical_json: Arc::new(canonical_json),
        compiled,
    })
}

fn placeholder_resource_error(source: GenericPlaceholderError) -> GenericProjectError {
    GenericProjectError::InvalidResource(GenericProjectResourceError::Placeholder(source))
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

#[cfg(windows)]
fn encode_path(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(windows)]
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

#[cfg(not(windows))]
fn encode_path(path: &Path) -> Vec<u8> {
    path.as_os_str().as_encoded_bytes().to_vec()
}

#[cfg(not(windows))]
fn decode_path_with_cancellation(
    bytes: &[u8],
    cancellation: &CooperativeCancellation,
) -> Result<PathBuf, GenericProjectError> {
    if let Err(source) = validate_utf8_bytes_with_cancellation(bytes, cancellation)? {
        return Err(invalid_database(
            GenericProjectDatabaseProblem::InvalidUtf8Path {
                valid_up_to: source.valid_up_to,
                error_len: source.error_len,
            },
        ));
    }
    // SAFETY: 上面的分块校验已经确认整份字节串是 UTF-8。
    let value = unsafe { std::str::from_utf8_unchecked(bytes) };
    let mut cloned = String::with_capacity(value.len());
    append_text_with_cancellation(&mut cloned, value, cancellation)?;
    Ok(PathBuf::from(cloned))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn sqlite_operation_errors_use_the_actual_sqlite_failure() {
        let interrupted = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
            None,
        );
        assert!(matches!(
            sqlite_operation_error("测试查询", interrupted),
            GenericProjectError::Cancelled
        ));

        assert!(matches!(
            sqlite_operation_error("测试查询", rusqlite::Error::QueryReturnedNoRows),
            GenericProjectError::Sqlite {
                operation: "测试查询",
                source: rusqlite::Error::QueryReturnedNoRows,
            }
        ));
    }

    #[test]
    fn initial_candidate_cleanup_preserves_every_failed_path() {
        let directory = tempdir().unwrap();
        let candidate = directory.path().join(".project.db.init.tmp");
        let sidecar = sqlite_sidecar_path(&candidate, SQLITE_SIDECAR_SUFFIXES[0]);
        fs::write(&candidate, "candidate").unwrap();
        fs::write(&sidecar, "sidecar").unwrap();
        let targets = [&candidate, &sidecar]
            .into_iter()
            .map(|path| {
                let file = pin_path_without_reparse(path).unwrap();
                (path.clone(), FileIdentity::of(file.file(), path).unwrap())
            })
            .collect::<Vec<_>>();
        fs::rename(&candidate, directory.path().join("original.db")).unwrap();
        fs::rename(&sidecar, directory.path().join("original-journal")).unwrap();
        fs::write(&candidate, "foreign database").unwrap();
        fs::write(&sidecar, "foreign sidecar").unwrap();

        let cleanup = cleanup_initial_database_candidate(&targets)
            .expect_err("清理必须保留被替换后的外来文件");
        assert_eq!(cleanup.len(), 2);
        assert_eq!(fs::read_to_string(&candidate).unwrap(), "foreign database");
        assert_eq!(fs::read_to_string(&sidecar).unwrap(), "foreign sidecar");

        let error = GenericProjectError::InitialCandidateCleanup {
            original: Box::new(GenericProjectError::Io {
                operation: FileSystemOperation::Create,
                path: directory.path().join("project.db"),
                source: io::Error::other("建立初始数据库失败"),
            }),
            cleanup,
        };
        let displayed = error.to_string();
        assert!(displayed.contains(".project.db.init.tmp"));
        assert!(displayed.contains(".project.db.init.tmp-journal"));

        let diagnostic = error.diagnostic_report(
            GenericDiagnosticStage::Init,
            Path::new("project.db"),
            StateEffect::Unchanged,
        );
        assert_eq!(diagnostic.related().len(), 2);
        for related in diagnostic.related() {
            assert_eq!(related.relation(), RelatedFailureRelation::Cleanup);
            assert_eq!(related.report().effect(), StateEffect::RecoveryRequired);
        }
    }

    fn language(value: &str) -> LanguageId {
        LanguageId::parse(value).expect("测试语言应合法")
    }

    #[test]
    fn resource_worker_start_failures_preserve_typed_backend_facts() {
        assert!(matches!(
            terminology_resource_error(TerminologyDefinitionError::Cancelled),
            GenericProjectError::Cancelled
        ));

        let errors = [
            (
                terminology_resource_error(TerminologyDefinitionError::StartWorker {
                    operation: "启动术语测试 worker",
                    source: io::Error::from_raw_os_error(8),
                }),
                "translation.terminology.worker_start",
            ),
            (
                placeholder_resource_error(GenericPlaceholderError::Compilation(
                    PlaceholderRuleCompilationError::StartWorker {
                        operation: PlaceholderWorkerOperation::CompileCustomRules,
                        source: io::Error::from_raw_os_error(8),
                    },
                )),
                "translation.placeholder.compilation.worker_start",
            ),
        ];

        for (error, expected_code) in errors {
            assert!(std::error::Error::source(&error).is_some());

            let diagnostic = error.diagnostic_report(
                GenericDiagnosticStage::Translate,
                Path::new("project.db"),
                StateEffect::Unchanged,
            );
            assert_eq!(diagnostic.effect(), StateEffect::Unchanged);
            assert_eq!(diagnostic.primary().code(), expected_code);
            assert_eq!(
                diagnostic.primary().resolution(),
                crate::diagnostic::DiagnosticResolution::Retry
            );
            let wire = serde_json::to_string(&diagnostic).expect("worker 诊断必须可序列化");
            assert!(wire.contains("\"raw_os_code\":8"));
        }
    }

    #[test]
    fn current_schema_validation_uses_the_command_cancellation() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(CREATE_INITIAL_SCHEMA_SQL).unwrap();
        let cancellation = CooperativeCancellation::default();
        cancellation.request();

        assert!(matches!(
            validate_current_generic_schema_with_cancellation(&connection, &cancellation),
            Err(GenericProjectError::Cancelled)
        ));
    }

    #[test]
    fn sqlite_row_text_clone_preserves_conversion_errors() {
        let connection = Connection::open_in_memory().unwrap();
        let mut statement = connection.prepare("SELECT 7 AS value").unwrap();
        let mut rows = statement.query([]).unwrap();
        let row = rows.next().unwrap().unwrap();
        let cancellation = CooperativeCancellation::default();
        assert!(matches!(
            clone_sqlite_text_column_with_cancellation(
                row,
                0,
                "读取测试 TEXT",
                &cancellation,
            ),
            Err(GenericProjectError::Sqlite {
                source: rusqlite::Error::InvalidColumnType(
                    0,
                    ref column,
                    rusqlite::types::Type::Integer,
                ),
                ..
            }) if column == "value"
        ));
        drop(rows);
        drop(statement);

        let mut statement = connection
            .prepare("SELECT CAST(x'80' AS TEXT) AS value")
            .unwrap();
        let mut rows = statement.query([]).unwrap();
        let row = rows.next().unwrap().unwrap();
        let error =
            clone_sqlite_text_column_with_cancellation(row, 0, "读取测试 TEXT", &cancellation)
                .expect_err("无效 UTF-8 TEXT 必须拒绝");
        assert!(matches!(
            &error,
            GenericProjectError::InvalidDatabase {
                problem: GenericProjectDatabaseProblem::InvalidTextColumnUtf8 {
                    column: 0,
                    valid_up_to: 0,
                    error_len: Some(1),
                    ..
                },
                source: None,
            }
        ));
        let wire = serde_json::to_value(error.diagnostic_report(
            GenericDiagnosticStage::ProjectOpening,
            Path::new("project.db"),
            StateEffect::Unchanged,
        ))
        .expect("数据库诊断必须可序列化");
        assert_eq!(
            wire["primary"]["code"],
            "generic.project.database.text_column_invalid_utf8"
        );
        assert_eq!(
            wire["primary"]["issue"]["details"]["problem"]["problem"]["column"],
            0
        );
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
    fn write_back_layout_rules_are_persisted_reused_cleared_and_rollback_safe() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).expect("应建立测试来源目录");
        write_source(
            &source,
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"u\",\"text\":\"原文\"}]}\n",
        );
        let store = init(&temp.path().join("project"), &source);
        store.extract().expect("首次 Extract 应成功");
        let fingerprint = store
            .open()
            .unwrap()
            .extracted_raw_fingerprint()
            .expect("已提取项目必须有来源指纹");

        assert!(
            store
                .load_write_back_layout_rules()
                .expect("尚未设置规则时必须读取空规则")
                .is_empty()
        );
        let selected = LayoutRuleSet::parse_toml(
            b"[[rule]]\nmax_fullwidth_chars = 20\nscopes = ['dialogue']\n",
        )
        .expect("测试规则必须有效");
        store
            .replace_write_back_layout_rules(fingerprint, &selected)
            .expect("有效规则必须原子保存");
        assert_eq!(
            store
                .load_write_back_layout_rules()
                .expect("省略外部文件时必须可复用保存内容")
                .canonical_json(),
            selected.canonical_json()
        );

        assert!(LayoutRuleSet::parse_toml(b"[[rule]]\nmax_fullwidth_chars = 0\n").is_err());
        assert_eq!(
            store
                .load_write_back_layout_rules()
                .expect("无效新文件不得改变旧规则")
                .canonical_json(),
            selected.canonical_json()
        );

        let empty = LayoutRuleSet::parse_toml(b"rule = []").expect("显式空规则必须有效");
        store
            .replace_write_back_layout_rules(fingerprint, &empty)
            .expect("空规则必须清除已保存规则");
        assert!(store.load_write_back_layout_rules().unwrap().is_empty());

        let stale = Sha256Fingerprint::from_bytes([0x7f; 32]);
        assert!(matches!(
            store.replace_write_back_layout_rules(stale, &selected),
            Err(GenericProjectError::TranslationSnapshotChanged)
        ));
        assert!(
            store
                .load_write_back_layout_rules()
                .expect("CAS 失败必须回滚并保留空规则")
                .is_empty()
        );
    }

    #[test]
    fn project_diagnostics_preserve_io_sqlite_and_state_facts() {
        let io_error = GenericProjectError::Io {
            operation: FileSystemOperation::Read,
            path: PathBuf::from("nested/input.jsonl"),
            source: io::Error::from_raw_os_error(5),
        };
        let io_diagnostic = io_error.diagnostic_report(
            GenericDiagnosticStage::Extract,
            Path::new("project.db"),
            StateEffect::Unchanged,
        );
        assert_eq!(io_diagnostic.primary().code(), "filesystem.io");
        let wire = serde_json::to_string(&io_diagnostic).expect("I/O 诊断必须可序列化");
        assert!(wire.contains("nested/input.jsonl"));
        assert!(wire.contains("\"raw_os_code\":5"));

        let connection = Connection::open_in_memory().unwrap();
        let source = connection
            .execute("INSERT INTO missing_table VALUES (1)", [])
            .expect_err("不存在的表必须产生 SQLite driver 错误");
        let sqlite_error = GenericProjectError::Sqlite {
            operation: "写入 Generic 测试数据库",
            source,
        };
        let sqlite_diagnostic = sqlite_error.diagnostic_report(
            GenericDiagnosticStage::Translate,
            Path::new("project.db"),
            StateEffect::Unchanged,
        );
        assert_eq!(sqlite_diagnostic.primary().code(), "sqlite.driver");
        let wire = serde_json::to_string(&sqlite_diagnostic).expect("SQLite 诊断必须可序列化");
        assert!(wire.contains("\"primary_code\":1"));
        assert!(wire.contains("\"extended_code\":1"));

        let state_diagnostic = GenericProjectError::ExtractRequired.diagnostic_report(
            GenericDiagnosticStage::Translate,
            Path::new("project.db"),
            StateEffect::Unchanged,
        );
        assert_eq!(
            state_diagnostic.primary().code(),
            "generic.project.extract_required"
        );
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
    fn generic_init_records_its_sqlite_transaction() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let performance = Arc::new(RunPerformanceCounters::default());

        GenericProjectStore::initialize_with_cancellation(
            GenericInitRequest {
                project_name: "game".parse().unwrap(),
                workspace_root: temp.path().join("project"),
                source_root: Some(source),
                source_language: Some(language("ja")),
                target_language: Some(language("zh-Hans")),
            },
            CooperativeCancellation::default(),
            Arc::clone(&performance),
        )
        .expect("首次 Init 应成功");

        let transactions = performance
            .snapshot()
            .sqlite_transactions
            .database_initialization;
        assert_eq!(transactions.begin.attempted, 1);
        assert_eq!(transactions.begin.succeeded, 1);
        assert_eq!(transactions.commit.attempted, 1);
        assert_eq!(transactions.commit.succeeded, 1);
        assert_eq!(transactions.rollback.attempted, 0);
    }

    #[test]
    fn initial_database_and_connections_use_the_common_sqlite_policy() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");
        let store = init(&workspace, &source);
        let project = store.open().expect("Generic 项目应可打开");
        let connection = open_sqlite_connection(
            project.database_path(),
            false,
            CooperativeCancellation::default(),
        )
        .expect("项目数据库应可重开");

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
    fn initial_database_matches_the_current_exact_generic_schema() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");
        let _store = init(&workspace, &source);
        let connection =
            Connection::open(workspace.join(DATABASE_FILE_NAME)).expect("应打开 Generic 数据库");

        validate_current_generic_schema(&connection)
            .expect("新建数据库必须与当前唯一 Generic schema 完全一致");
    }

    #[test]
    fn current_generic_schema_rejects_definition_drift_and_attached_objects() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(CREATE_INITIAL_SCHEMA_SQL).unwrap();
        connection
            .execute_batch(
                "ALTER TABLE translation_resource RENAME TO translation_resource_old;
                 CREATE TABLE translation_resource (
                     resource_kind TEXT PRIMARY KEY,
                     canonical_json TEXT NOT NULL
                 ) STRICT;
                 INSERT INTO translation_resource
                 SELECT * FROM translation_resource_old;
                 DROP TABLE translation_resource_old;",
            )
            .unwrap();

        let definition_error =
            validate_current_generic_schema(&connection).expect_err("约束变化必须拒绝");
        assert!(matches!(
            definition_error,
            GenericProjectError::InvalidDatabase {
                problem: GenericProjectDatabaseProblem::SchemaMismatch {
                    ref definition_mismatches,
                    ..
                },
                ..
            } if definition_mismatches
                .iter()
                .any(|object| object.as_str() == "table/translation_resource")
        ));

        connection
            .execute(
                "CREATE INDEX unexpected_generic_unit_index
                 ON generic_unit(source_text)",
                [],
            )
            .unwrap();
        let attached_object_error =
            validate_current_generic_schema(&connection).expect_err("附加受管索引必须拒绝");
        assert!(matches!(
            attached_object_error,
            GenericProjectError::InvalidDatabase {
                problem: GenericProjectDatabaseProblem::SchemaMismatch {
                    ref unexpected,
                    ..
                },
                ..
            } if unexpected
                .iter()
                .any(|object| object.as_str() == "index/unexpected_generic_unit_index")
        ));
    }

    #[test]
    fn normal_project_open_rejects_generic_schema_drift() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");
        let store = init(&workspace, &source);
        Connection::open(workspace.join(DATABASE_FILE_NAME))
            .unwrap()
            .execute_batch(
                "ALTER TABLE translation_resource RENAME TO translation_resource_old;
                 CREATE TABLE translation_resource (
                     resource_kind TEXT PRIMARY KEY,
                     canonical_json TEXT NOT NULL
                 ) STRICT;
                 INSERT INTO translation_resource
                 SELECT * FROM translation_resource_old;
                 DROP TABLE translation_resource_old;",
            )
            .unwrap();

        assert!(matches!(
            store.open(),
            Err(GenericProjectError::InvalidDatabase {
                problem: GenericProjectDatabaseProblem::SchemaMismatch {
                    ref definition_mismatches,
                    ..
                },
                ..
            }) if definition_mismatches
                .iter()
                .any(|object| object.as_str() == "table/translation_resource")
        ));
    }

    #[test]
    fn normal_project_open_rejects_typed_invalid_terminology_resource() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");
        let store = init(&workspace, &source);
        Connection::open(workspace.join(DATABASE_FILE_NAME))
            .unwrap()
            .execute(
                "UPDATE translation_resource
                 SET canonical_json = '[1]'
                 WHERE resource_kind = 'terminology'",
                [],
            )
            .unwrap();

        assert!(matches!(
            store.open(),
            Err(GenericProjectError::InvalidResource(
                GenericProjectResourceError::InvalidSnapshot {
                    resource: GenericResourceKind::Terminology,
                    ..
                }
            ))
        ));
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
            entry.unwrap().file_name().to_string_lossy() != ".project.db.init.tmp"
        }));
    }

    #[test]
    fn first_init_publishes_a_self_contained_database_file() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");
        let (_, initialized) = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "game".parse().unwrap(),
            workspace_root: workspace.clone(),
            source_root: Some(source.canonicalize().unwrap()),
            source_language: Some(language("ja")),
            target_language: Some(language("zh-Hans")),
        })
        .expect("首次 Init 应发布数据库");
        let published = workspace.join(DATABASE_FILE_NAME);
        for suffix in SQLITE_SIDECAR_SUFFIXES {
            assert!(
                !sqlite_sidecar_path(&published, suffix).exists(),
                "首次 Init 成功后不得依赖 SQLite sidecar"
            );
        }

        let isolated_workspace = temp.path().join("isolated-project");
        fs::create_dir(&isolated_workspace).unwrap();
        let isolated_database = isolated_workspace.join(DATABASE_FILE_NAME);
        fs::copy(&published, &isolated_database).expect("应只复制已发布的主数据库文件");
        let isolated_connection =
            Connection::open(&isolated_database).expect("独立主数据库文件应可直接重开");
        validate_current_generic_schema(&isolated_connection)
            .expect("独立主数据库文件应包含完整的当前 schema");
        let singleton_count: i64 = isolated_connection
            .query_row(
                "SELECT count(*) FROM generic_project WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .expect("应可读取独立主数据库中的项目单例");
        assert_eq!(singleton_count, 1);
        let journal_mode: String = isolated_connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("应可读取独立主数据库的日志模式");
        assert_eq!(journal_mode, "delete");
        isolated_connection
            .close()
            .expect("独立主数据库验证连接应可显式关闭");

        let reopened = GenericProjectStore::for_workspace(isolated_workspace)
            .open()
            .expect("只复制主数据库文件后仍应能通过生产读取路径打开");
        assert_eq!(reopened.project_name(), initialized.project_name());
        assert_eq!(reopened.source_root(), initialized.source_root());
        assert_eq!(reopened.language_pair(), initialized.language_pair());
    }

    #[test]
    fn first_init_rejects_and_preserves_every_stale_target_sidecar() {
        for suffix in SQLITE_SIDECAR_SUFFIXES {
            let temp = tempdir().unwrap();
            let source = temp.path().join("source");
            fs::create_dir(&source).unwrap();
            let workspace = temp.path().join("project");
            fs::create_dir(&workspace).unwrap();
            let published = workspace.join(DATABASE_FILE_NAME);
            let stale_sidecar = sqlite_sidecar_path(&published, suffix);
            fs::write(&stale_sidecar, b"stale-sidecar").expect("应建立遗留 sidecar");

            let error = GenericProjectStore::initialize(GenericInitRequest {
                project_name: "game".parse().unwrap(),
                workspace_root: workspace.clone(),
                source_root: Some(source.canonicalize().unwrap()),
                source_language: Some(language("ja")),
                target_language: Some(language("zh-Hans")),
            })
            .expect_err("发布目标旁存在遗留 sidecar 时不得建立新项目");

            assert!(matches!(
                error,
                GenericProjectError::Io {
                    operation: FileSystemOperation::Metadata,
                    ref path,
                    ref source,
                } if path == &stale_sidecar && source.kind() == io::ErrorKind::AlreadyExists
            ));
            assert!(!published.exists(), "拒绝遗留 sidecar 时不得发布主数据库");
            assert_eq!(
                fs::read(&stale_sidecar).expect("遗留 sidecar 必须保留"),
                b"stale-sidecar"
            );
            assert!(
                fs::read_dir(&workspace).unwrap().all(|entry| {
                    entry.unwrap().file_name().to_string_lossy() != ".project.db.init.tmp"
                }),
                "拒绝遗留 sidecar 后不得留下候选数据库"
            );
        }
    }

    #[test]
    fn target_sidecar_appearing_during_init_still_blocks_publish() {
        for suffix in SQLITE_SIDECAR_SUFFIXES {
            let temp = tempdir().unwrap();
            let source = temp.path().join("source");
            fs::create_dir(&source).unwrap();
            let candidate = temp.path().join("candidate.db");
            let published = temp.path().join(DATABASE_FILE_NAME);
            let stale_sidecar = sqlite_sidecar_path(&published, suffix);
            let cancellation = CooperativeCancellation::default();
            let candidate_file = create_new_pinned_database_file(&candidate).unwrap();
            let identity = FileIdentity::of(&candidate_file, &candidate).unwrap();
            let mut connection = open_sqlite_connection(&candidate, true, cancellation.clone())
                .expect("应打开候选库");
            create_initial_schema(
                &mut connection,
                &"game".parse().unwrap(),
                &source.canonicalize().unwrap(),
                &language("ja"),
                &language("zh-Hans"),
                &cancellation,
                &RunPerformanceCounters::default(),
            )
            .expect("应建立候选 schema");
            fs::write(&stale_sidecar, b"appeared-during-init")
                .expect("应模拟候选建立后出现的目标 sidecar");

            let error = publish_initial_database_candidate(
                connection,
                candidate_file,
                identity,
                &candidate,
                &published,
                &cancellation,
            )
            .expect_err("最终 rename 前发现目标 sidecar 时不得发布");

            assert!(matches!(
                error,
                GenericProjectError::Io {
                    operation: FileSystemOperation::Metadata,
                    ref path,
                    ref source,
                } if path == &stale_sidecar && source.kind() == io::ErrorKind::AlreadyExists
            ));
            assert!(!published.exists(), "目标 sidecar 竞争时不得发布主数据库");
            assert_eq!(
                fs::read(&stale_sidecar).expect("竞争产生的 sidecar 必须保留"),
                b"appeared-during-init"
            );
            cleanup_initial_database_candidate(&[(candidate, identity)]).expect("应清理未发布候选");
        }
    }

    #[test]
    fn occupied_wal_checkpoint_does_not_publish_initial_database() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let candidate = temp.path().join("candidate.db");
        let published = temp.path().join(DATABASE_FILE_NAME);
        let cancellation = CooperativeCancellation::default();
        let candidate_file = create_new_pinned_database_file(&candidate).unwrap();
        let identity = FileIdentity::of(&candidate_file, &candidate).unwrap();
        let mut writer =
            open_sqlite_connection(&candidate, true, cancellation.clone()).expect("应打开候选库");
        create_initial_schema(
            &mut writer,
            &"game".parse().unwrap(),
            &source.canonicalize().unwrap(),
            &language("ja"),
            &language("zh-Hans"),
            &cancellation,
            &RunPerformanceCounters::default(),
        )
        .expect("应建立候选 schema");

        let blocker = Connection::open(&candidate).expect("应打开 checkpoint 阻塞连接");
        blocker.execute_batch("BEGIN").expect("应开始读取事务");
        let initial_profile: Option<String> = blocker
            .query_row(
                "SELECT last_profile_id FROM generic_project WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .expect("读取事务应固定 WAL 快照");
        assert_eq!(initial_profile, None);
        writer
            .execute(
                "UPDATE generic_project SET last_profile_id = 'primary' WHERE singleton = 1",
                [],
            )
            .expect("应在读取快照之后追加 WAL frame");
        drop(writer);
        assert!(
            sqlite_sidecar_path(&candidate, "-wal").exists(),
            "读取快照必须让候选 WAL 保持存在"
        );

        let publisher = apply_att_sqlite_cancellable_read_write_policy(
            Connection::open(&candidate).expect("应重开候选库"),
            || true,
        )
        .expect("应安装立即停止 busy wait 的测试策略");
        publisher
            .progress_handler(0, None::<fn() -> bool>)
            .expect("测试只应由 busy handler 停止 checkpoint");
        let error = publish_initial_database_candidate(
            publisher,
            candidate_file,
            identity,
            &candidate,
            &published,
            &cancellation,
        )
        .expect_err("checkpoint 被读取快照占用时不得发布主数据库");
        assert!(matches!(
            error,
            GenericProjectError::Sqlite {
                operation: "收束 Generic 初始数据库 WAL",
                ref source,
            } if sqlite_error_is_busy(source)
        ));
        assert!(!published.exists(), "checkpoint 未完成不得出现已发布数据库");

        blocker.execute_batch("ROLLBACK").expect("应释放读取快照");
        drop(blocker);
        let mut cleanup_targets = vec![(candidate.clone(), identity)];
        observe_initial_database_sidecars(&candidate, &mut cleanup_targets).unwrap();
        cleanup_initial_database_candidate(&cleanup_targets).expect("应清理测试候选及 sidecar");
    }

    #[test]
    fn initial_schema_and_project_singleton_roll_back_together() {
        use rusqlite::hooks::{AuthAction, Authorization};

        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let cancellation = CooperativeCancellation::default();
        let mut connection = apply_att_sqlite_cancellable_read_write_policy(
            Connection::open_in_memory().unwrap(),
            || false,
        )
        .unwrap();
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
            &cancellation,
            &RunPerformanceCounters::default(),
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
    fn extract_preserves_stable_units_and_retains_bodies_across_context_changes() {
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
                state_fingerprint: Sha256Fingerprint::from_bytes([7; 32]),
                expected_translation: None,
                was_current_rejected: false,
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
        let changed = store.load_snapshot().unwrap();
        let changed_group = &changed.files()[0].groups()[0];
        assert!(changed_group.units()[0].translation().is_none());
        assert!(changed_group.units()[1].translation().is_none());
        let retained = changed_group.units()[2]
            .translation()
            .expect("稳定 Unit 的正文不应因 kind 变化而删除");
        assert_eq!(retained.translation(), "译丙");
        assert_eq!(
            retained.state_fingerprint(),
            Sha256Fingerprint::from_bytes([7; 32]),
            "Extract 只能保留正文和原状态，不能把失配状态改写成当前适用性"
        );
        assert_eq!(
            crate::generic::current_translation_for_stored_with_cancellation(
                changed.project(),
                changed_group,
                &changed_group.units()[2],
                &CooperativeCancellation::default(),
            )
            .unwrap(),
            None,
            "旧 kind 语境的正文只保留为可逆旧值，不得继续作为 Current"
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
            state_fingerprint: Sha256Fingerprint::from_bytes([42; 32]),
            expected_translation: None,
            was_current_rejected: false,
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
    fn rejected_candidate_round_trips_and_valid_translation_clears_it_atomically() {
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
        let state = Sha256Fingerprint::from_bytes([42; 32]);
        let source_lines = vec![unit.source_text().to_owned()];
        let rejected = RejectedTranslationWrite {
            group_id: group.id().to_owned(),
            unit_id: unit.id().to_owned(),
            readable_id: "input.jsonl:line1:unit1:text".to_owned(),
            origin: TranslationOrigin::Automatic,
            expected_source_text: unit.source_text().to_owned(),
            source: source_lines.clone(),
            expected_group_context: group.context_fingerprint(),
            expected_manual_applicability: crate::manual::generic_manual_applicability(
                group.id(),
                unit.id(),
                "text.jsonl",
                group.kind(),
                "ja",
                "zh-Hans",
                &source_lines,
            ),
            candidate_json: "{\"wrong\":true}".to_owned(),
            translation: None,
            violation: ProvenInvariantViolation::InvalidCandidateShape,
            planning_state: state,
            expected_translation: None,
            was_current_rejected: false,
        };

        let outcome = store
            .commit_translation_results_for_profile(
                snapshot.project().extracted_raw_fingerprint().unwrap(),
                &[],
                std::slice::from_ref(&rejected),
                "primary",
            )
            .unwrap();
        assert_eq!(outcome.rejected, 1);
        assert_eq!(outcome.newly_rejected, 1);
        assert_eq!(outcome.resolved_rejected, 0);
        let snapshot = store.load_snapshot().unwrap();
        let stored = snapshot.files()[0].groups()[0].units()[0]
            .rejected()
            .expect("当前硬拒绝必须可以重读");
        assert_eq!(stored.readable_id(), rejected.readable_id);
        assert_eq!(stored.translation(), None);
        assert_eq!(
            stored.violation(),
            &ProvenInvariantViolation::InvalidCandidateShape
        );

        let no_result = store
            .commit_translation_results_for_profile(
                snapshot.project().extracted_raw_fingerprint().unwrap(),
                &[],
                &[],
                "primary",
            )
            .unwrap();
        assert_eq!(no_result.committed, 0);
        assert_eq!(no_result.rejected, 0);
        assert_eq!(no_result.newly_rejected, 0);
        assert_eq!(no_result.resolved_rejected, 0);
        let unchanged = store.load_snapshot().unwrap();
        assert_eq!(
            unchanged.files()[0].groups()[0].units()[0]
                .rejected()
                .expect("取消、Unavailable 或无法映射时不得请求前清除旧 Rejected")
                .candidate_json,
            rejected.candidate_json
        );

        let repeated = store
            .commit_translation_results_for_profile(
                snapshot.project().extracted_raw_fingerprint().unwrap(),
                &[],
                &[RejectedTranslationWrite {
                    was_current_rejected: true,
                    ..rejected.clone()
                }],
                "primary",
            )
            .unwrap();
        assert_eq!(repeated.rejected, 1);
        assert_eq!(repeated.newly_rejected, 0);
        assert_eq!(repeated.resolved_rejected, 0);

        let write = TranslationWrite {
            group_id: rejected.group_id,
            unit_id: rejected.unit_id,
            expected_source_text: rejected.expected_source_text,
            expected_group_context: rejected.expected_group_context,
            translation: "译文".to_owned(),
            state_fingerprint: state,
            expected_translation: None,
            was_current_rejected: true,
        };
        let outcome = store
            .commit_translation_results_for_profile(
                snapshot.project().extracted_raw_fingerprint().unwrap(),
                std::slice::from_ref(&write),
                &[],
                "primary",
            )
            .unwrap();
        assert_eq!(outcome.committed, 1);
        assert_eq!(outcome.resolved_rejected, 1);
        assert_eq!(outcome.newly_rejected, 0);
        let snapshot = store.load_snapshot().unwrap();
        let unit = &snapshot.files()[0].groups()[0].units()[0];
        assert_eq!(unit.translation().unwrap().translation(), "译文");
        assert!(unit.rejected().is_none());
    }

    #[test]
    fn rejected_candidate_cas_accepts_exact_stale_body_and_rejects_a_changed_body() {
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
        let old_state = crate::translation::generic_automatic_applicability(
            "ja",
            "zh-Hans",
            group.id(),
            unit.id(),
            unit.source_text(),
            Sha256Fingerprint::from_bytes([91; 32]),
        );
        store
            .commit_translations(
                snapshot.project().extracted_raw_fingerprint().unwrap(),
                &[TranslationWrite {
                    group_id: group.id().to_owned(),
                    unit_id: unit.id().to_owned(),
                    expected_source_text: unit.source_text().to_owned(),
                    expected_group_context: group.context_fingerprint(),
                    translation: "旧语境译文".to_owned(),
                    state_fingerprint: old_state,
                    expected_translation: None,
                    was_current_rejected: false,
                }],
            )
            .unwrap();
        let stale = store.load_snapshot().unwrap();
        let group = &stale.files()[0].groups()[0];
        let unit = &group.units()[0];
        let previous = unit.translation().expect("旧正文必须保留").clone();
        let source_lines = vec![unit.source_text().to_owned()];
        let current_state = crate::translation::generic_automatic_applicability(
            "ja",
            "zh-Hans",
            group.id(),
            unit.id(),
            unit.source_text(),
            group.context_fingerprint(),
        );
        let rejection = RejectedTranslationWrite {
            group_id: group.id().to_owned(),
            unit_id: unit.id().to_owned(),
            readable_id: "text.jsonl:line1:unit1:text".to_owned(),
            origin: TranslationOrigin::Automatic,
            expected_source_text: unit.source_text().to_owned(),
            source: source_lines.clone(),
            expected_group_context: group.context_fingerprint(),
            expected_manual_applicability: crate::manual::generic_manual_applicability(
                group.id(),
                unit.id(),
                "text.jsonl",
                group.kind(),
                "ja",
                "zh-Hans",
                &source_lines,
            ),
            candidate_json: "true".to_owned(),
            translation: None,
            violation: ProvenInvariantViolation::InvalidCandidateShape,
            planning_state: current_state,
            expected_translation: Some(previous.clone()),
            was_current_rejected: false,
        };

        let saved = store
            .commit_translation_results_for_profile(
                stale.project().extracted_raw_fingerprint().unwrap(),
                &[],
                std::slice::from_ref(&rejection),
                "primary",
            )
            .unwrap();
        assert_eq!(saved.rejected, 1);
        assert!(saved.conflicts.is_empty());
        let retained = store.load_snapshot().unwrap();
        let retained_unit = &retained.files()[0].groups()[0].units()[0];
        assert_eq!(retained_unit.translation(), Some(&previous));
        assert!(retained_unit.rejected().is_some());

        let replacement = TranslationWrite {
            group_id: rejection.group_id.clone(),
            unit_id: rejection.unit_id.clone(),
            expected_source_text: rejection.expected_source_text.clone(),
            expected_group_context: rejection.expected_group_context,
            translation: "新语境译文".to_owned(),
            state_fingerprint: current_state,
            expected_translation: Some(previous),
            was_current_rejected: false,
        };
        assert_eq!(
            store
                .commit_translations(
                    retained.project().extracted_raw_fingerprint().unwrap(),
                    &[replacement],
                )
                .unwrap()
                .committed,
            1
        );
        let conflict = store
            .commit_translation_results_for_profile(
                retained.project().extracted_raw_fingerprint().unwrap(),
                &[],
                &[rejection],
                "primary",
            )
            .unwrap();
        assert_eq!(conflict.rejected, 0);
        assert_eq!(conflict.conflicts, [("g".to_owned(), "u".to_owned())]);
        let final_snapshot = store.load_snapshot().unwrap();
        let final_unit = &final_snapshot.files()[0].groups()[0].units()[0];
        assert_eq!(
            final_unit
                .translation()
                .map(GenericStoredTranslation::translation),
            Some("新语境译文")
        );
        assert!(final_unit.rejected().is_none());
    }

    #[test]
    fn stale_manual_translation_does_not_block_current_rejected_candidate() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");
        write_source(
            &source,
            "{\"id\":\"g\",\"kind\":\"k\",\"units\":[{\"id\":\"u\",\"text\":\"旧原文\"}]}\n",
        );
        let store = init(&workspace, &source);
        store.extract().unwrap();
        let snapshot = store.load_snapshot().unwrap();
        let group = &snapshot.files()[0].groups()[0];
        let unit = &group.units()[0];
        let old_source = vec![unit.source_text().to_owned()];
        let old_applicability = crate::manual::generic_manual_applicability(
            group.id(),
            unit.id(),
            "text.jsonl",
            group.kind(),
            "ja",
            "zh-Hans",
            &old_source,
        );
        let connection = Connection::open(&store.database_path).unwrap();
        crate::manual::apply_generic_manual_translations(
            &connection,
            &[crate::manual::ValidatedManualTranslation {
                id: "text.jsonl:line1:unit1:text".to_owned(),
                kind: crate::manual::ManualTranslationType::Free,
                source: old_source,
                translation: vec!["旧译文".to_owned()],
                locator: crate::manual::ManualTranslationLocator::Generic {
                    group_id: group.id().to_owned(),
                    unit_id: unit.id().to_owned(),
                },
                applicability: old_applicability,
            }],
        )
        .unwrap();
        drop(connection);

        let current_rejection = RejectedTranslationWrite {
            group_id: group.id().to_owned(),
            unit_id: unit.id().to_owned(),
            readable_id: "text.jsonl:line1:unit1:text".to_owned(),
            origin: TranslationOrigin::Automatic,
            expected_source_text: unit.source_text().to_owned(),
            source: vec![unit.source_text().to_owned()],
            expected_group_context: group.context_fingerprint(),
            expected_manual_applicability: old_applicability,
            candidate_json: "{\"wrong\":true}".to_owned(),
            translation: None,
            violation: ProvenInvariantViolation::InvalidCandidateShape,
            planning_state: Sha256Fingerprint::from_bytes([41; 32]),
            expected_translation: None,
            was_current_rejected: false,
        };
        let current_outcome = store
            .commit_translation_results_for_profile(
                snapshot.project().extracted_raw_fingerprint().unwrap(),
                &[],
                &[current_rejection],
                "primary",
            )
            .unwrap();
        assert_eq!(current_outcome.rejected, 0);
        assert_eq!(
            current_outcome.conflicts,
            [(group.id().to_owned(), unit.id().to_owned())],
            "当前人工译文仍必须阻止模型候选覆盖该 Unit"
        );

        write_source(
            &source,
            "{\"id\":\"g\",\"kind\":\"k\",\"units\":[{\"id\":\"u\",\"text\":\"新原文\"}]}\n",
        );
        store.extract().unwrap();
        let snapshot = store.load_snapshot().unwrap();
        let group = &snapshot.files()[0].groups()[0];
        let unit = &group.units()[0];
        assert!(unit.translation().is_none(), "旧人工译文必须已经过期");
        let source_lines = vec![unit.source_text().to_owned()];
        let rejected = RejectedTranslationWrite {
            group_id: group.id().to_owned(),
            unit_id: unit.id().to_owned(),
            readable_id: "text.jsonl:line1:unit1:text".to_owned(),
            origin: TranslationOrigin::Automatic,
            expected_source_text: unit.source_text().to_owned(),
            source: source_lines.clone(),
            expected_group_context: group.context_fingerprint(),
            expected_manual_applicability: crate::manual::generic_manual_applicability(
                group.id(),
                unit.id(),
                "text.jsonl",
                group.kind(),
                "ja",
                "zh-Hans",
                &source_lines,
            ),
            candidate_json: "{\"wrong\":true}".to_owned(),
            translation: None,
            violation: ProvenInvariantViolation::InvalidCandidateShape,
            planning_state: Sha256Fingerprint::from_bytes([42; 32]),
            expected_translation: None,
            was_current_rejected: false,
        };

        let outcome = store
            .commit_translation_results_for_profile(
                snapshot.project().extracted_raw_fingerprint().unwrap(),
                &[],
                &[rejected],
                "primary",
            )
            .unwrap();
        assert_eq!(outcome.rejected, 1);
        assert!(outcome.conflicts.is_empty());
        let snapshot = store.load_snapshot().unwrap();
        assert!(
            snapshot.files()[0].groups()[0].units()[0]
                .rejected()
                .is_some(),
            "当前 Rejected 候选必须在过期人工记录存在时仍可保存"
        );
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
                state_fingerprint: state,
                expected_translation: None,
                was_current_rejected: false,
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
            state_fingerprint: Sha256Fingerprint::from_bytes([43; 32]),
            expected_translation: Some(previous),
            was_current_rejected: false,
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
        store.extract().expect("空输入也应建立 Extract 快照");
        let extracted_before_move = store.open().unwrap().extracted_raw_fingerprint();

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
        assert_eq!(
            after_source_change.extracted_raw_fingerprint(),
            extracted_before_move,
            "只改变绑定路径不应删除最近一次成功 Extract 的事实"
        );

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
    fn changing_either_language_preserves_extract_and_translation_bodies() {
        for (source_language, target_language) in [(Some("en"), None), (None, Some("zh-Hant"))] {
            let temp = tempdir().unwrap();
            let source = temp.path().join("source");
            fs::create_dir(&source).unwrap();
            write_source(
                &source,
                "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"u\",\"text\":\"原文\"}]}\n",
            );
            let workspace = temp.path().join("project");
            let store = init(&workspace, &source);
            store.extract().expect("首次 Extract 应成功");
            let snapshot = store.load_snapshot().expect("应该可读取 Extract 快照");
            let group = &snapshot.files()[0].groups()[0];
            let unit = &group.units()[0];
            let current_state = crate::translation::generic_automatic_applicability(
                snapshot.project().language_pair().source().as_str(),
                snapshot.project().language_pair().target().as_str(),
                group.id(),
                unit.id(),
                unit.source_text(),
                group.context_fingerprint(),
            );
            store
                .commit_translations(
                    snapshot.project().extracted_raw_fingerprint().unwrap(),
                    &[TranslationWrite {
                        group_id: group.id().to_owned(),
                        unit_id: unit.id().to_owned(),
                        expected_source_text: unit.source_text().to_owned(),
                        expected_group_context: group.context_fingerprint(),
                        translation: "译文".to_owned(),
                        state_fingerprint: current_state,
                        expected_translation: None,
                        was_current_rejected: false,
                    }],
                )
                .expect("测试译文应该可提交");

            GenericProjectStore::initialize(GenericInitRequest {
                project_name: "game".parse().unwrap(),
                workspace_root: workspace,
                source_root: None,
                source_language: source_language.map(language),
                target_language: target_language.map(language),
            })
            .expect("改变语言应成功");

            let project = store.open().unwrap();
            assert_eq!(
                project.extracted_raw_fingerprint(),
                snapshot.project().extracted_raw_fingerprint()
            );
            assert_eq!(
                project.extracted_asset_fingerprint(),
                snapshot.project().extracted_asset_fingerprint()
            );
            let connection = store.open_connection(false).unwrap();
            let asset_rows: i64 = connection
                .query_row(
                    "SELECT
                         (SELECT count(*) FROM generic_file)
                       + (SELECT count(*) FROM generic_group)
                       + (SELECT count(*) FROM generic_unit)",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(asset_rows, 3, "语言变化不应销毁仍可核对的 Extract 事实");
            let retained_translation: Option<String> = connection
                .query_row(
                    "SELECT translation FROM generic_unit WHERE group_id = 'g' AND unit_id = 'u'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(retained_translation.as_deref(), Some("译文"));
            drop(connection);
            let changed = store.load_snapshot().unwrap();
            let changed_group = &changed.files()[0].groups()[0];
            let changed_unit = &changed_group.units()[0];
            assert_eq!(
                changed_unit.translation().unwrap().state_fingerprint(),
                current_state,
                "语言变化只能改变当前适用性，不能重写已有状态"
            );
            assert_eq!(
                crate::generic::current_translation_for_stored_with_cancellation(
                    changed.project(),
                    changed_group,
                    changed_unit,
                    &CooperativeCancellation::default(),
                )
                .unwrap(),
                None
            );
            let connection = store.open_connection(false).unwrap();
            let terminology_json: String = connection
                .query_row(
                    "SELECT canonical_json FROM translation_resource
                     WHERE resource_kind = 'terminology'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(terminology_json, "[]");
        }
    }

    #[test]
    fn moving_to_an_identical_source_root_preserves_the_current_snapshot_and_translation() {
        let temp = tempdir().unwrap();
        let first_source = temp.path().join("source-a");
        let second_source = temp.path().join("source-b");
        fs::create_dir(&first_source).unwrap();
        fs::create_dir(&second_source).unwrap();
        let input =
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"u\",\"text\":\"原文\"}]}\n";
        write_source(&first_source, input);
        write_source(&second_source, input);
        let workspace = temp.path().join("project");
        let store = init(&workspace, &first_source);
        store.extract().expect("首次 Extract 应成功");
        let snapshot = store.load_snapshot().expect("应该可读取 Extract 快照");
        let group = &snapshot.files()[0].groups()[0];
        let unit = &group.units()[0];
        store
            .commit_translations(
                snapshot.project().extracted_raw_fingerprint().unwrap(),
                &[TranslationWrite {
                    group_id: group.id().to_owned(),
                    unit_id: unit.id().to_owned(),
                    expected_source_text: unit.source_text().to_owned(),
                    expected_group_context: group.context_fingerprint(),
                    translation: "译文".to_owned(),
                    state_fingerprint: Sha256Fingerprint::from_bytes([31; 32]),
                    expected_translation: None,
                    was_current_rejected: false,
                }],
            )
            .expect("测试译文应该可提交");

        GenericProjectStore::initialize(GenericInitRequest {
            project_name: "game".parse().unwrap(),
            workspace_root: workspace,
            source_root: Some(second_source),
            source_language: None,
            target_language: None,
        })
        .expect("移动到相同内容的输入根应成功");

        let (moved, _) = store
            .ensure_input_current()
            .expect("相同内容的新根应继续匹配既有 Extract 快照");
        assert_eq!(
            moved.files()[0].groups()[0].units()[0]
                .translation()
                .map(GenericStoredTranslation::translation),
            Some("译文")
        );
    }

    #[test]
    fn sqlite_busy_wait_stops_promptly_when_the_command_is_cancelled() {
        use std::sync::{Arc, Barrier, mpsc};
        use std::time::Duration;

        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");
        init(&workspace, &source);

        let blocker = Connection::open(workspace.join(DATABASE_FILE_NAME)).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();

        let cancellation = CooperativeCancellation::default();
        let cancellable_store = GenericProjectStore::for_workspace_with_cancellation(
            workspace.clone(),
            cancellation.clone(),
            Arc::new(RunPerformanceCounters::default()),
        );
        let barrier = Arc::new(Barrier::new(2));
        let worker_barrier = Arc::clone(&barrier);
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            worker_barrier.wait();
            sender
                .send(cancellable_store.remember_profile("blocked"))
                .unwrap();
        });
        barrier.wait();
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("SQLite 等待线程不得在返回结果前断开")
            }
            Ok(result) => panic!("外部写锁存在时操作必须继续等待，而不是提前返回：{result:?}"),
        }

        cancellation.request();
        let result = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("取消后不应继续等待 rusqlite 默认的约五秒超时");
        assert!(matches!(result, Err(GenericProjectError::Cancelled)));
        assert!(result.unwrap_err().is_cancelled());

        blocker.execute_batch("ROLLBACK").unwrap();
        worker.join().unwrap();
        let verification = Connection::open(workspace.join(DATABASE_FILE_NAME)).unwrap();
        let remembered: Option<String> = verification
            .query_row(
                "SELECT last_profile_id FROM generic_project WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            remembered, None,
            "Busy 取消只能在显式回滚确认成功后返回 Cancelled"
        );
    }

    #[test]
    fn cancellation_rolls_back_a_partially_written_extract_batch() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write_source(
            &source,
            concat!(
                "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[",
                "{\"id\":\"u1\",\"text\":\"第一项\"},",
                "{\"id\":\"u2\",\"text\":\"第二项\"}",
                "]}\n"
            ),
        );
        let workspace = temp.path().join("project");
        init(&workspace, &source);

        let cancellation = CooperativeCancellation::default();
        let store = GenericProjectStore::for_workspace_with_cancellation(
            workspace.clone(),
            cancellation.clone(),
            Arc::new(RunPerformanceCounters::default()),
        );
        let project = store.open().expect("应打开 Generic 项目");
        let scanned = scan_input_tree(&source).expect("应扫描测试输入");
        let previous = GenericStoredSnapshot {
            project,
            files: Vec::new(),
        };
        let reconciled =
            reconcile_snapshot(&previous, &scanned, &cancellation).expect("应建立待写快照");

        let mut connection = store.open_connection(false).expect("应打开项目数据库");
        let hook_cancellation = cancellation.clone();
        connection
            .update_hook(Some(
                move |_action: rusqlite::hooks::Action,
                      _database: &str,
                      table: &str,
                      _row_id: i64| {
                    if table == "generic_unit" {
                        hook_cancellation.request();
                    }
                },
            ))
            .expect("应安装测试更新 hook");
        let result = store.finish_cancellable(run_cancellable_transaction(
            &mut connection,
            &cancellation,
            &RunPerformanceCounters::default(),
            SqliteTransactionScope::WritePlan,
            "开始测试 Extract 事务",
            "提交测试 Extract 事务",
            "回滚测试 Extract 事务",
            |transaction| replace_snapshot(transaction, &scanned, &reconciled.files, &cancellation),
        ));
        assert!(matches!(result, Err(GenericProjectError::Cancelled)));
        assert!(connection.is_autocommit(), "取消返回前必须确认事务已回滚");
        drop(connection);

        let verification =
            Connection::open(workspace.join(DATABASE_FILE_NAME)).expect("应重开项目数据库");
        let asset_rows: i64 = verification
            .query_row(
                "SELECT
                     (SELECT count(*) FROM generic_file)
                   + (SELECT count(*) FROM generic_group)
                   + (SELECT count(*) FROM generic_unit)",
                [],
                |row| row.get(0),
            )
            .expect("应检查回滚后的资产");
        let fingerprints: (Option<Vec<u8>>, Option<Vec<u8>>) = verification
            .query_row(
                "SELECT extracted_raw_fingerprint, extracted_asset_fingerprint
                 FROM generic_project WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("应检查回滚后的 Extract 指纹");
        assert_eq!(asset_rows, 0);
        assert_eq!(fingerprints, (None, None));
    }

    #[test]
    fn rollback_failure_is_outcome_unknown_and_preserves_both_failures() {
        use rusqlite::hooks::{AuthAction, Authorization, TransactionOperation};

        let cancellation = CooperativeCancellation::default();
        let wait_cancellation = cancellation.clone();
        let mut connection = apply_att_sqlite_cancellable_read_write_policy(
            Connection::open_in_memory().unwrap(),
            move || wait_cancellation.is_requested(),
        )
        .unwrap();
        connection
            .execute_batch("CREATE TABLE changed(value INTEGER NOT NULL)")
            .unwrap();
        connection
            .authorizer(Some(
                |context: rusqlite::hooks::AuthContext<'_>| match context.action {
                    AuthAction::Transaction {
                        operation: TransactionOperation::Rollback,
                    } => Authorization::Deny,
                    _ => Authorization::Allow,
                },
            ))
            .unwrap();

        let result: Result<(), GenericProjectError> = run_cancellable_transaction(
            &mut connection,
            &cancellation,
            &RunPerformanceCounters::default(),
            SqliteTransactionScope::WritePlan,
            "开始回滚失败测试事务",
            "提交回滚失败测试事务",
            "回滚失败测试事务",
            |transaction| {
                transaction
                    .execute("INSERT INTO changed VALUES (1)", [])
                    .map_err(|source| GenericProjectError::Sqlite {
                        operation: "写入回滚失败测试数据",
                        source,
                    })?;
                cancellation.request();
                Err(GenericProjectError::Cancelled)
            },
        );
        let classifier = GenericProjectStore::for_workspace_with_cancellation(
            PathBuf::new(),
            cancellation.clone(),
            Arc::new(RunPerformanceCounters::default()),
        );
        let result = classifier.finish_cancellable(result);
        let error = result.expect_err("ROLLBACK 被拒绝时不得报告干净取消");
        match &error {
            GenericProjectError::TransactionOutcomeUnknown {
                primary: Some(primary),
                finalization: GenericTransactionFinalizationFailure::Sqlite { operation, .. },
                ..
            } => {
                assert!(matches!(primary.as_ref(), GenericProjectError::Cancelled));
                assert_eq!(*operation, "回滚失败测试事务");
            }
            other => panic!("应保留主取消与回滚失败，实际为 {other:?}"),
        }
        assert!(!error.is_cancelled());
        let diagnostic = error.diagnostic_report(
            GenericDiagnosticStage::Extract,
            Path::new("project.db"),
            StateEffect::Unchanged,
        );
        assert_eq!(diagnostic.effect(), StateEffect::OutcomeUnknown);
        assert_eq!(diagnostic.related().len(), 1);
        assert_eq!(
            diagnostic.related()[0].relation(),
            RelatedFailureRelation::Finalization
        );
        assert_eq!(
            diagnostic.related()[0].report().effect(),
            StateEffect::OutcomeUnknown
        );
        assert_eq!(
            diagnostic.related()[0].report().primary().code(),
            "sqlite.driver"
        );
    }

    #[test]
    fn cancellation_requested_during_commit_does_not_interrupt_a_successful_commit() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        use rusqlite::hooks::{AuthAction, Authorization, TransactionOperation};

        let cancellation = CooperativeCancellation::default();
        let wait_cancellation = cancellation.clone();
        let mut connection = apply_att_sqlite_cancellable_read_write_policy(
            Connection::open_in_memory().unwrap(),
            move || wait_cancellation.is_requested(),
        )
        .unwrap();
        connection
            .execute_batch("CREATE TABLE changed(value INTEGER NOT NULL)")
            .unwrap();
        let commit_seen = Arc::new(AtomicBool::new(false));
        let hook_commit_seen = Arc::clone(&commit_seen);
        let hook_cancellation = cancellation.clone();
        connection
            .authorizer(Some(
                move |context: rusqlite::hooks::AuthContext<'_>| match context.action {
                    AuthAction::Transaction {
                        operation: TransactionOperation::Unknown,
                    } => {
                        hook_commit_seen.store(true, Ordering::Release);
                        hook_cancellation.request();
                        Authorization::Allow
                    }
                    _ => Authorization::Allow,
                },
            ))
            .unwrap();

        let result = run_cancellable_transaction(
            &mut connection,
            &cancellation,
            &RunPerformanceCounters::default(),
            SqliteTransactionScope::WritePlan,
            "开始提交取消测试事务",
            "提交取消测试事务",
            "回滚提交取消测试事务",
            |transaction| {
                transaction
                    .execute("INSERT INTO changed VALUES (1)", [])
                    .map_err(|source| GenericProjectError::Sqlite {
                        operation: "写入提交取消测试数据",
                        source,
                    })?;
                Ok(())
            },
        );
        assert!(result.is_ok(), "COMMIT 开始后到达的取消不得改写成功终态");
        assert!(commit_seen.load(Ordering::Acquire));
        assert!(cancellation.is_requested());
        assert!(connection.is_autocommit());
        let finalization_cancellation = connection.cancellation_handle();
        let finalization = suspend_att_sqlite_cancellation(&finalization_cancellation);
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM changed", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        drop(finalization);
    }

    #[test]
    fn failed_commit_with_confirmed_rollback_is_not_mapped_to_cancelled() {
        use rusqlite::hooks::{AuthAction, Authorization, TransactionOperation};

        let cancellation = CooperativeCancellation::default();
        let wait_cancellation = cancellation.clone();
        let mut connection = apply_att_sqlite_cancellable_read_write_policy(
            Connection::open_in_memory().unwrap(),
            move || wait_cancellation.is_requested(),
        )
        .unwrap();
        connection
            .execute_batch("CREATE TABLE changed(value INTEGER NOT NULL)")
            .unwrap();
        let hook_cancellation = cancellation.clone();
        connection
            .authorizer(Some(
                move |context: rusqlite::hooks::AuthContext<'_>| match context.action {
                    AuthAction::Transaction {
                        operation: TransactionOperation::Unknown,
                    } => {
                        hook_cancellation.request();
                        Authorization::Deny
                    }
                    _ => Authorization::Allow,
                },
            ))
            .unwrap();

        let result = run_cancellable_transaction(
            &mut connection,
            &cancellation,
            &RunPerformanceCounters::default(),
            SqliteTransactionScope::WritePlan,
            "开始提交失败测试事务",
            "提交失败测试事务",
            "回滚提交失败测试事务",
            |transaction| {
                transaction
                    .execute("INSERT INTO changed VALUES (1)", [])
                    .map_err(|source| GenericProjectError::Sqlite {
                        operation: "写入提交失败测试数据",
                        source,
                    })?;
                Ok(())
            },
        );
        let classifier = GenericProjectStore::for_workspace_with_cancellation(
            PathBuf::new(),
            cancellation.clone(),
            Arc::new(RunPerformanceCounters::default()),
        );
        let result = classifier.finish_cancellable(result);
        let error = result.expect_err("COMMIT 被拒绝后必须报告确认未提交");
        assert!(matches!(
            &error,
            GenericProjectError::TransactionNotCommitted { .. }
        ));
        assert!(!error.is_cancelled());
        assert!(connection.is_autocommit());
        let finalization_cancellation = connection.cancellation_handle();
        let finalization = suspend_att_sqlite_cancellation(&finalization_cancellation);
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM changed", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        drop(finalization);
        let diagnostic = error.diagnostic_report(
            GenericDiagnosticStage::Translate,
            Path::new("project.db"),
            StateEffect::OutcomeUnknown,
        );
        assert_eq!(diagnostic.effect(), StateEffect::Unchanged);
        assert!(diagnostic.related().is_empty());
        let wire = serde_json::to_string(&diagnostic).expect("回滚终态诊断必须可序列化");
        assert!(wire.contains("\"transaction\":\"rolled_back\""));
    }

    #[test]
    fn commit_and_rollback_failures_report_outcome_unknown_with_both_causes() {
        use rusqlite::hooks::{AuthAction, Authorization, TransactionOperation};

        let cancellation = CooperativeCancellation::default();
        let wait_cancellation = cancellation.clone();
        let mut connection = apply_att_sqlite_cancellable_read_write_policy(
            Connection::open_in_memory().unwrap(),
            move || wait_cancellation.is_requested(),
        )
        .unwrap();
        connection
            .execute_batch("CREATE TABLE changed(value INTEGER NOT NULL)")
            .unwrap();
        let hook_cancellation = cancellation.clone();
        connection
            .authorizer(Some(
                move |context: rusqlite::hooks::AuthContext<'_>| match context.action {
                    AuthAction::Transaction {
                        operation: TransactionOperation::Unknown,
                    } => {
                        hook_cancellation.request();
                        Authorization::Deny
                    }
                    AuthAction::Transaction {
                        operation: TransactionOperation::Rollback,
                    } => Authorization::Deny,
                    _ => Authorization::Allow,
                },
            ))
            .unwrap();

        let result = run_cancellable_transaction(
            &mut connection,
            &cancellation,
            &RunPerformanceCounters::default(),
            SqliteTransactionScope::WritePlan,
            "开始提交终态未知测试事务",
            "提交终态未知测试事务",
            "回滚提交终态未知测试事务",
            |transaction| {
                transaction
                    .execute("INSERT INTO changed VALUES (1)", [])
                    .map_err(|source| GenericProjectError::Sqlite {
                        operation: "写入提交终态未知测试数据",
                        source,
                    })?;
                Ok(())
            },
        );
        let classifier = GenericProjectStore::for_workspace_with_cancellation(
            PathBuf::new(),
            cancellation.clone(),
            Arc::new(RunPerformanceCounters::default()),
        );
        let result = classifier.finish_cancellable(result);
        let error = result.expect_err("COMMIT 与 ROLLBACK 都失败时结果必须未知");
        match &error {
            GenericProjectError::TransactionOutcomeUnknown {
                primary: Some(primary),
                finalization: GenericTransactionFinalizationFailure::Sqlite { operation, .. },
                ..
            } => {
                assert!(matches!(
                    primary.as_ref(),
                    GenericProjectError::Sqlite {
                        operation: "提交终态未知测试事务",
                        ..
                    }
                ));
                assert_eq!(*operation, "回滚提交终态未知测试事务");
            }
            other => panic!("应保留 COMMIT 与 ROLLBACK 两个失败，实际为 {other:?}"),
        }
        assert!(!error.is_cancelled());
        let diagnostic = error.diagnostic_report(
            GenericDiagnosticStage::Translate,
            Path::new("project.db"),
            StateEffect::Unchanged,
        );
        assert_eq!(diagnostic.effect(), StateEffect::OutcomeUnknown);
        assert_eq!(diagnostic.primary().code(), "sqlite.driver");
        assert_eq!(diagnostic.related().len(), 1);
        assert_eq!(
            diagnostic.related()[0].relation(),
            RelatedFailureRelation::Finalization
        );
        assert_eq!(
            diagnostic.related()[0].report().effect(),
            StateEffect::OutcomeUnknown
        );
        let wire = serde_json::to_string(&diagnostic).expect("事务未知诊断必须可序列化");
        assert_eq!(wire.matches("sqlite.driver").count(), 2);
        assert!(wire.contains("\"transaction\":\"active\""));
        assert!(wire.contains("\"transaction\":\"outcome_unknown\""));
    }

    #[test]
    fn extract_preserves_logical_units_when_equal_text_siblings_reorder() {
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
                state_fingerprint: Sha256Fingerprint::from_bytes([8; 32]),
                expected_translation: None,
                was_current_rejected: false,
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

        let moved = store.load_snapshot().unwrap();
        let units = moved.files()[0].groups()[0].units();
        assert_eq!(units[0].id(), "b");
        assert_eq!(units[0].translation().unwrap().translation(), "译文-b");
        assert_eq!(units[1].id(), "a");
        assert_eq!(units[1].translation().unwrap().translation(), "译文-a");
        assert_eq!(units[2].translation().unwrap().translation(), "译文-c");
    }

    #[test]
    fn applying_resources_rejects_invalid_terminology_and_preserves_valid_raw_text() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write_source(
            &source,
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"u\",\"text\":\"原文\"}]}\n",
        );
        let workspace = temp.path().join("project");
        let store = init(&workspace, &source);
        store.extract().expect("首次 Extract 应成功");
        let expected_raw_fingerprint = store
            .open()
            .expect("应打开已提取项目")
            .extracted_raw_fingerprint()
            .expect("已提取项目应保存原始指纹");

        for (case, terminology_json, expected_code) in [
            (
                "条目类型错误",
                "[1]",
                "translation.terminology.invalid_snapshot_json",
            ),
            (
                "术语重复",
                r#"[{"term":"同名","translation":"译文一","triggers":["触发一"]},{"term":"同名","translation":"译文二","triggers":["触发二"]}]"#,
                "translation.terminology.duplicate_term",
            ),
        ] {
            let error = store
                .apply_translation_resources(expected_raw_fingerprint, terminology_json, "[]", &[])
                .expect_err(case);
            assert!(matches!(&error, GenericProjectError::InvalidResource(_)));
            assert!(std::error::Error::source(&error).is_some());
            let diagnostic = error.diagnostic_report(
                GenericDiagnosticStage::Translate,
                store.database_path(),
                StateEffect::Unchanged,
            );
            assert_eq!(diagnostic.primary().code(), expected_code);

            let resources = store
                .load_translation_resources()
                .expect("拒绝无效术语后项目资源仍应可读取");
            assert_eq!(resources.terminology_json(), "[]");
            assert_eq!(resources.placeholder_rules_json(), "[]");
        }

        let terminology_with_whitespace =
            r#"[{"term":" 原文 ","translation":" 译文 ","triggers":[" 原文 "]}]"#;
        store
            .apply_translation_resources(
                expected_raw_fingerprint,
                terminology_with_whitespace,
                "[]",
                &[],
            )
            .expect("术语原值中的首尾空白应由项目资源边界原样接受");
        let resources = store
            .load_translation_resources()
            .expect("合法术语应保存到当前项目");
        assert_eq!(resources.terminology_json(), terminology_with_whitespace);
    }

    #[test]
    fn applying_new_resources_moves_invalid_manual_translation_to_rejected_atomically() {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");
        write_source(
            &source,
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"u\",\"text\":\"Open [A].\"}]}\n",
        );
        let store = init(&workspace, &source);
        store.extract().expect("首次 Extract 应成功");
        let snapshot = store.load_snapshot().expect("应该可读取首次快照");
        let group = &snapshot.files()[0].groups()[0];
        let unit = &group.units()[0];
        let source_lines = vec![unit.source_text().to_owned()];
        let applicability = crate::manual::generic_manual_applicability(
            group.id(),
            unit.id(),
            "text.jsonl",
            group.kind(),
            "ja",
            "zh-Hans",
            &source_lines,
        );
        let connection = Connection::open(&store.database_path).expect("应该可打开项目数据库");
        crate::manual::apply_generic_manual_translations(
            &connection,
            &[crate::manual::ValidatedManualTranslation {
                id: "text.jsonl:line1:unit1:text".to_owned(),
                kind: crate::manual::ManualTranslationType::Free,
                source: source_lines,
                translation: vec!["打开 [B]。".to_owned()],
                locator: crate::manual::ManualTranslationLocator::Generic {
                    group_id: group.id().to_owned(),
                    unit_id: unit.id().to_owned(),
                },
                applicability,
            }],
        )
        .expect("人工译文应该可保存");
        drop(connection);

        let snapshot = store.load_snapshot().expect("应该可读取人工译文");
        let unit = &snapshot.files()[0].groups()[0].units()[0];
        let previous = unit.translation().expect("人工译文应该是当前译文").clone();
        assert_eq!(previous.origin(), TranslationOrigin::Manual);

        let outcome = store
            .apply_translation_resources(
                snapshot.project().extracted_raw_fingerprint().unwrap(),
                "[]",
                "[]",
                &[TranslationClear {
                    group_id: group.id().to_owned(),
                    unit_id: unit.id().to_owned(),
                    readable_id: "text.jsonl:line1:unit1:text".to_owned(),
                    expected_source_text: unit.source_text().to_owned(),
                    expected_group_context: group.context_fingerprint(),
                    expected_translation: previous,
                    violation: ProvenInvariantViolation::PlaceholderMismatch,
                    rejection_planning_state: Sha256Fingerprint::from_bytes([8; 32]),
                }],
            )
            .expect("资源和失效人工译文应该可原子更新");

        assert_eq!(outcome.committed, 1);
        assert!(outcome.conflicts.is_empty());
        let snapshot = store.load_snapshot().expect("应该可读取失效后的项目状态");
        let unit = &snapshot.files()[0].groups()[0].units()[0];
        assert!(
            unit.translation().is_none(),
            "已转入 Rejected 的人工译文不得继续作为 Current"
        );
        let rejected = unit.rejected().expect("失效人工译文必须保存在 Rejected");
        assert_eq!(rejected.origin(), TranslationOrigin::Manual);
        assert_eq!(
            rejected.translation(),
            Some(["打开 [B]。".to_owned()].as_slice())
        );
        assert_eq!(
            rejected.violation(),
            &ProvenInvariantViolation::PlaceholderMismatch
        );
    }
}
