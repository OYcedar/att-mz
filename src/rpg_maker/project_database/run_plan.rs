//! RPG Maker 命令最近一次成功运行时需要复用的最小项目状态。

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::future::Future;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use crate::diagnostic::RecoveryFact;
use crate::json_diagnostic::JsonErrorCategory;
use crate::rpg_maker::extract::rules::{RulesProgramError, validate_rules_canonical_json};
use crate::storage::sqlite::{
    ExecuteFinalTransactionError, QueryExistingDatabaseError, SqliteCommand,
    SqliteFinalTransactionExecutor, SqliteQuery, SqliteQueryExecutor, SqliteRow,
    SqliteTransactionPlan, SqliteTransactionStep, SqliteValue,
};

pub(super) const CREATE_INIT_RUN_PLAN_TABLE: &str = r#"CREATE TABLE init_run_plan (
    singleton         INTEGER NOT NULL PRIMARY KEY CHECK (singleton = 1),
    source_path_utf16 BLOB NOT NULL CHECK (
        typeof(source_path_utf16) = 'blob'
        AND length(source_path_utf16) > 0
        AND length(source_path_utf16) % 2 = 0
    )
)"#;

pub(super) const CREATE_EXTRACT_RUN_PLAN_TABLE: &str = r#"CREATE TABLE extract_run_plan (
    singleton       INTEGER NOT NULL PRIMARY KEY CHECK (singleton = 1),
    builtin_enabled INTEGER NOT NULL CHECK (builtin_enabled IN (0, 1)),
    rules_enabled   INTEGER NOT NULL CHECK (rules_enabled IN (0, 1)),
    CHECK (builtin_enabled + rules_enabled > 0)
)"#;

pub(super) const CREATE_EXTRACT_RULES_DEFINITION_TABLE: &str = r#"CREATE TABLE extract_rules_definition (
    singleton      INTEGER NOT NULL PRIMARY KEY CHECK (singleton = 1),
    canonical_json TEXT NOT NULL CHECK (
        typeof(canonical_json) = 'text'
        AND length(canonical_json) > 0
        AND json_valid(canonical_json)
        AND json_type(canonical_json) = 'array'
        AND json_array_length(canonical_json) > 0
    )
)"#;

pub(super) const CREATE_TRANSLATE_RUN_PLAN_TABLE: &str = r#"CREATE TABLE translate_run_plan (
    singleton  INTEGER NOT NULL PRIMARY KEY CHECK (singleton = 1),
    profile_id TEXT NOT NULL CHECK (
        typeof(profile_id) = 'text'
        AND length(profile_id) > 0
        AND trim(profile_id) = profile_id
    )
)"#;

pub(crate) const SELECT_RUN_PLAN_SINGLETONS: &str = r#"SELECT
    (SELECT source_path_utf16 FROM init_run_plan WHERE singleton = 1),
    (SELECT builtin_enabled FROM extract_run_plan WHERE singleton = 1),
    (SELECT rules_enabled FROM extract_run_plan WHERE singleton = 1),
    (SELECT canonical_json FROM extract_rules_definition WHERE singleton = 1),
    (SELECT profile_id FROM translate_run_plan WHERE singleton = 1)"#;

const DELETE_INIT_RUN_PLAN: &str = "DELETE FROM init_run_plan";
const INSERT_INIT_RUN_PLAN: &str =
    "INSERT INTO init_run_plan (singleton, source_path_utf16) VALUES (1, ?1)";
const DELETE_EXTRACT_RUN_PLAN: &str = "DELETE FROM extract_run_plan";
const DELETE_EXTRACT_RULES_DEFINITION: &str = "DELETE FROM extract_rules_definition";
const INSERT_EXTRACT_RUN_PLAN: &str = r#"INSERT INTO extract_run_plan (
    singleton,
    builtin_enabled,
    rules_enabled
) VALUES (1, ?1, ?2)"#;
const INSERT_EXTRACT_RULES_DEFINITION: &str = r#"INSERT INTO extract_rules_definition (
    singleton,
    canonical_json
) VALUES (1, ?1)"#;
const DELETE_TRANSLATE_RUN_PLAN: &str = "DELETE FROM translate_run_plan";
const INSERT_TRANSLATE_RUN_PLAN: &str =
    "INSERT INTO translate_run_plan (singleton, profile_id) VALUES (1, ?1)";

/// 运行方案中唯一需要保存的 Windows 路径用途。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunPlanPathPurpose {
    InitSource,
}

impl RunPlanPathPurpose {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::InitSource => "Init 来源",
        }
    }
}

/// `serde_json` 的稳定错误类别；不保存可能含 Rules 正文的错误文本。
pub(crate) type RunPlanJsonErrorCategory = JsonErrorCategory;

/// Init 最近一次成功使用的已解析来源目录。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InitRunPlan {
    source_path: PathBuf,
}

impl InitRunPlan {
    pub(crate) fn new(source_path: PathBuf) -> Result<Self, InvalidRunPlanValue> {
        validate_resolved_path(&source_path, RunPlanPathPurpose::InitSource)?;
        Ok(Self { source_path })
    }

    pub(crate) fn source_path(&self) -> &Path {
        &self.source_path
    }
}

/// Extract 最近一次成功使用的 Builtin/Rules 组合。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExtractRunPlan {
    builtin_enabled: bool,
    rules_definition: Option<ExtractRulesCanonicalJson>,
}

impl ExtractRunPlan {
    pub(crate) fn new(
        builtin_enabled: bool,
        rules_definition: Option<ExtractRulesCanonicalJson>,
    ) -> Result<Self, InvalidRunPlanValue> {
        if !builtin_enabled && rules_definition.is_none() {
            return Err(InvalidRunPlanValue::EmptyExtractOwners);
        }
        Ok(Self {
            builtin_enabled,
            rules_definition,
        })
    }

    pub(crate) const fn builtin_enabled(&self) -> bool {
        self.builtin_enabled
    }

    pub(crate) const fn rules_enabled(&self) -> bool {
        self.rules_definition.is_some()
    }

    pub(crate) fn rules_definition(&self) -> Option<&ExtractRulesCanonicalJson> {
        self.rules_definition.as_ref()
    }
}

/// 已由 Rules 语义所有者验证并规范编码的非空规则集合。
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ExtractRulesCanonicalJson(String);

impl fmt::Debug for ExtractRulesCanonicalJson {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExtractRulesCanonicalJson")
            .field("bytes", &self.0.len())
            .finish_non_exhaustive()
    }
}

impl ExtractRulesCanonicalJson {
    pub(crate) fn new(canonical_json: String) -> Result<Self, InvalidRunPlanValue> {
        let value: serde_json::Value = serde_json::from_str(&canonical_json).map_err(|source| {
            InvalidRunPlanValue::InvalidRulesCanonicalJson {
                line: usize_to_u64(source.line()),
                column: usize_to_u64(source.column()),
                category: JsonErrorCategory::from(&source),
            }
        })?;
        let serde_json::Value::Array(rules) = &value else {
            return Err(InvalidRunPlanValue::RulesCanonicalJsonNotArray);
        };
        if rules.is_empty() {
            return Err(InvalidRunPlanValue::EmptyRulesDefinition);
        }
        let encoded = serde_json::to_string(&value).map_err(|source| {
            InvalidRunPlanValue::RulesCanonicalJsonEncodingFailed {
                line: usize_to_u64(source.line()),
                column: usize_to_u64(source.column()),
                category: JsonErrorCategory::from(&source),
            }
        })?;
        if encoded != canonical_json {
            return Err(InvalidRunPlanValue::NonCanonicalRulesJson);
        }
        validate_rules_canonical_json(&canonical_json).map_err(|source| {
            InvalidRunPlanValue::InvalidRulesSemantics {
                fact: rules_program_recovery_fact(&source),
            }
        })?;
        Ok(Self(canonical_json))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// Translate 最近一次成功使用的 Profile。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslateRunPlan {
    profile_id: String,
}

impl TranslateRunPlan {
    pub(crate) fn new(profile_id: String) -> Result<Self, InvalidRunPlanValue> {
        if profile_id.trim().is_empty() {
            return Err(InvalidRunPlanValue::EmptyProfileId);
        }
        if profile_id.trim() != profile_id {
            return Err(InvalidRunPlanValue::ProfileIdHasOuterWhitespace);
        }
        Ok(Self { profile_id })
    }

    pub(crate) fn profile_id(&self) -> &str {
        &self.profile_id
    }
}

/// 当前可供命令复用的全部运行方案。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProjectRunPlans {
    init: Option<InitRunPlan>,
    extract: Option<ExtractRunPlan>,
    translate: Option<TranslateRunPlan>,
}

impl ProjectRunPlans {
    pub(crate) fn init(&self) -> Option<&InitRunPlan> {
        self.init.as_ref()
    }

    pub(crate) fn extract(&self) -> Option<&ExtractRunPlan> {
        self.extract.as_ref()
    }

    pub(crate) fn translate(&self) -> Option<&TranslateRunPlan> {
        self.translate.as_ref()
    }
}

/// 最终短事务要精确替换或清除的一条命令运行方案。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProjectRunPlanReplacement {
    Init(InitRunPlan),
    Extract(Option<ExtractRunPlan>),
    Translate(TranslateRunPlan),
}

/// 受信运行方案值无法建立。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum InvalidRunPlanValue {
    EmptyPath {
        purpose: RunPlanPathPurpose,
    },
    RelativePath {
        purpose: RunPlanPathPurpose,
    },
    PathContainsNul {
        purpose: RunPlanPathPurpose,
    },
    InvalidWindowsPathEncoding {
        purpose: RunPlanPathPurpose,
    },
    EmptyExtractOwners,
    InvalidRulesCanonicalJson {
        line: u64,
        column: u64,
        category: RunPlanJsonErrorCategory,
    },
    RulesCanonicalJsonNotArray,
    RulesCanonicalJsonEncodingFailed {
        line: u64,
        column: u64,
        category: RunPlanJsonErrorCategory,
    },
    InvalidRulesSemantics {
        fact: Option<RecoveryFact>,
    },
    NonCanonicalRulesJson,
    EmptyRulesDefinition,
    EmptyProfileId,
    ProfileIdHasOuterWhitespace,
}

impl InvalidRunPlanValue {
    pub(crate) const fn safe_subject(&self) -> &'static str {
        match self {
            Self::EmptyPath { purpose }
            | Self::RelativePath { purpose }
            | Self::PathContainsNul { purpose }
            | Self::InvalidWindowsPathEncoding { purpose } => purpose.as_str(),
            Self::EmptyExtractOwners => "extract_run_plan.owners",
            Self::InvalidRulesCanonicalJson { .. }
            | Self::RulesCanonicalJsonNotArray
            | Self::RulesCanonicalJsonEncodingFailed { .. }
            | Self::InvalidRulesSemantics { .. }
            | Self::NonCanonicalRulesJson
            | Self::EmptyRulesDefinition => "extract_rules_definition.canonical_json",
            Self::EmptyProfileId | Self::ProfileIdHasOuterWhitespace => {
                "translate_run_plan.profile_id"
            }
        }
    }

    pub(crate) fn safe_detail(&self) -> String {
        match self {
            Self::EmptyPath { purpose } => format!("{}路径不能为空", purpose.as_str()),
            Self::RelativePath { purpose } => {
                format!("{}路径必须是绝对路径", purpose.as_str())
            }
            Self::PathContainsNul { purpose } => {
                format!("{}路径不能包含 NUL", purpose.as_str())
            }
            Self::InvalidWindowsPathEncoding { purpose } => format!(
                "{}路径不是完整的 Windows UTF-16LE 字节序列",
                purpose.as_str()
            ),
            Self::EmptyExtractOwners => "Extract 必须启用 Builtin 或 Rules".to_owned(),
            Self::InvalidRulesCanonicalJson {
                line,
                column,
                category,
            } => format!(
                "Extract Rules canonical JSON 无效：类别 {}，第 {line} 行第 {column} 列",
                category.storage_name()
            ),
            Self::RulesCanonicalJsonNotArray => {
                "Extract Rules canonical JSON 必须是数组".to_owned()
            }
            Self::RulesCanonicalJsonEncodingFailed {
                line,
                column,
                category,
            } => format!(
                "Extract Rules canonical JSON 重新编码失败：类别 {}，第 {line} 行第 {column} 列",
                category.storage_name()
            ),
            Self::InvalidRulesSemantics { .. } => {
                "Extract Rules canonical JSON 未通过规则语义校验".to_owned()
            }
            Self::NonCanonicalRulesJson => "Extract Rules JSON 必须使用当前规范编码".to_owned(),
            Self::EmptyRulesDefinition => "Extract Rules 定义不能为空".to_owned(),
            Self::EmptyProfileId => "Translate Profile ID 不能为空".to_owned(),
            Self::ProfileIdHasOuterWhitespace => "Translate Profile ID 不能包含首尾空白".to_owned(),
        }
    }

    pub(crate) const fn recovery_fact(&self) -> Option<&RecoveryFact> {
        match self {
            Self::InvalidRulesSemantics { fact } => fact.as_ref(),
            _ => None,
        }
    }
}

impl fmt::Display for InvalidRunPlanValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.safe_detail())
    }
}

impl Error for InvalidRunPlanValue {}

/// 数据库行无法恢复为当前唯一运行方案模型。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InvalidProjectRunPlans {
    subject: &'static str,
    detail: String,
    recovery: Option<RecoveryFact>,
}

impl InvalidProjectRunPlans {
    fn new(subject: &'static str, detail: impl Into<String>) -> Self {
        Self {
            subject,
            detail: detail.into(),
            recovery: None,
        }
    }

    fn invalid_value(reason: InvalidRunPlanValue) -> Self {
        Self {
            subject: reason.safe_subject(),
            detail: reason.safe_detail(),
            recovery: reason.recovery_fact().cloned(),
        }
    }

    pub(super) fn unexpected_snapshot_result_sets(actual: usize) -> Self {
        Self::new(
            "run_plan.snapshot",
            format!("运行方案快照应返回 1 组结果，实际为 {actual} 组"),
        )
    }

    pub(crate) const fn safe_subject(&self) -> &'static str {
        self.subject
    }

    pub(crate) fn safe_detail(&self) -> String {
        self.detail.clone()
    }

    pub(crate) const fn recovery_fact(&self) -> Option<&RecoveryFact> {
        self.recovery.as_ref()
    }
}

impl fmt::Display for InvalidProjectRunPlans {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl Error for InvalidProjectRunPlans {}

pub(crate) fn decode_project_run_plans(
    rows: Vec<SqliteRow>,
) -> Result<ProjectRunPlans, InvalidProjectRunPlans> {
    let [row] = <[SqliteRow; 1]>::try_from(rows).map_err(|rows| {
        InvalidProjectRunPlans::new(
            "run_plan.singletons",
            format!("运行方案单例查询应返回 1 行，实际为 {} 行", rows.len()),
        )
    })?;
    let values = row.into_values();
    let [
        init_path,
        extract_builtin,
        extract_rules,
        rules_json,
        translate_profile,
    ] = <[SqliteValue; 5]>::try_from(values).map_err(|values| {
        InvalidProjectRunPlans::new(
            "run_plan.singletons",
            format!("运行方案单例查询应返回 5 列，实际为 {} 列", values.len()),
        )
    })?;

    let init = match init_path {
        SqliteValue::Null => None,
        SqliteValue::Blob(bytes) => Some(
            decode_windows_path(bytes, RunPlanPathPurpose::InitSource)
                .and_then(InitRunPlan::new)
                .map_err(InvalidProjectRunPlans::invalid_value)?,
        ),
        actual => {
            return Err(wrong_type(
                "init_run_plan.source_path_utf16",
                "BLOB 或 NULL",
                &actual,
            ));
        }
    };

    let extract = match (extract_builtin, extract_rules, rules_json) {
        (SqliteValue::Null, SqliteValue::Null, SqliteValue::Null) => None,
        (builtin, rules, definition) => {
            let builtin = decode_boolean(builtin, "extract_run_plan.builtin_enabled")?;
            let rules = decode_boolean(rules, "extract_run_plan.rules_enabled")?;
            let definition = match definition {
                SqliteValue::Null => None,
                SqliteValue::Text(value) => Some(
                    ExtractRulesCanonicalJson::new(value)
                        .map_err(InvalidProjectRunPlans::invalid_value)?,
                ),
                actual => {
                    return Err(wrong_type(
                        "extract_rules_definition.canonical_json",
                        "TEXT 或 NULL",
                        &actual,
                    ));
                }
            };
            if rules != definition.is_some() {
                return Err(InvalidProjectRunPlans::new(
                    "extract_run_plan.rules_enabled",
                    format!(
                        "rules_enabled={rules} 与规则定义存在={} 不一致",
                        definition.is_some()
                    ),
                ));
            }
            Some(
                ExtractRunPlan::new(builtin, definition)
                    .map_err(InvalidProjectRunPlans::invalid_value)?,
            )
        }
    };

    let translate = match translate_profile {
        SqliteValue::Null => None,
        SqliteValue::Text(value) => {
            Some(TranslateRunPlan::new(value).map_err(InvalidProjectRunPlans::invalid_value)?)
        }
        actual => {
            return Err(wrong_type(
                "translate_run_plan.profile_id",
                "TEXT 或 NULL",
                &actual,
            ));
        }
    };

    Ok(ProjectRunPlans {
        init,
        extract,
        translate,
    })
}

fn decode_boolean(
    value: SqliteValue,
    subject: &'static str,
) -> Result<bool, InvalidProjectRunPlans> {
    match value {
        SqliteValue::Integer(0) => Ok(false),
        SqliteValue::Integer(1) => Ok(true),
        SqliteValue::Integer(actual) => Err(InvalidProjectRunPlans::new(
            subject,
            format!("{subject} 只能是 0 或 1，实际为 {actual}"),
        )),
        actual => Err(wrong_type(subject, "INTEGER", &actual)),
    }
}

fn wrong_type(
    subject: &'static str,
    expected: &'static str,
    actual: &SqliteValue,
) -> InvalidProjectRunPlans {
    InvalidProjectRunPlans::new(
        subject,
        format!(
            "{subject} 应为 {expected}，实际为 {}",
            sqlite_type_name(actual)
        ),
    )
}

fn sqlite_type_name(value: &SqliteValue) -> &'static str {
    match value {
        SqliteValue::Null => "NULL",
        SqliteValue::Integer(_) => "INTEGER",
        SqliteValue::Real(_) => "REAL",
        SqliteValue::Text(_) => "TEXT",
        SqliteValue::Blob(_) => "BLOB",
    }
}

fn rules_program_recovery_fact(source: &RulesProgramError) -> Option<RecoveryFact> {
    source
        .safe_diagnostic(Path::new("extract_rules_definition.canonical_json"))
        .recovery
        .into_iter()
        .next()
}

fn validate_resolved_path(
    path: &Path,
    purpose: RunPlanPathPurpose,
) -> Result<(), InvalidRunPlanValue> {
    if path.as_os_str().is_empty() {
        return Err(InvalidRunPlanValue::EmptyPath { purpose });
    }
    if !path.is_absolute() {
        return Err(InvalidRunPlanValue::RelativePath { purpose });
    }
    if path.as_os_str().encode_wide().any(|unit| unit == 0) {
        return Err(InvalidRunPlanValue::PathContainsNul { purpose });
    }
    Ok(())
}

fn encode_windows_path(path: &Path) -> Vec<u8> {
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn decode_windows_path(
    bytes: Vec<u8>,
    purpose: RunPlanPathPurpose,
) -> Result<PathBuf, InvalidRunPlanValue> {
    if bytes.is_empty() {
        return Err(InvalidRunPlanValue::EmptyPath { purpose });
    }
    let mut chunks = bytes.chunks_exact(2);
    let units = chunks
        .by_ref()
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    if !chunks.remainder().is_empty() {
        return Err(InvalidRunPlanValue::InvalidWindowsPathEncoding { purpose });
    }
    let path = PathBuf::from(OsString::from_wide(&units));
    validate_resolved_path(&path, purpose)?;
    Ok(path)
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// 从主 SQLite 根的一致性快照读取命令运行方案。
pub(crate) trait ProjectRunPlanRepository: Send + Sync {
    type ReadError: Error + Send + Sync + 'static;

    fn read(
        &self,
        database_path: PathBuf,
    ) -> impl Future<Output = Result<ProjectRunPlans, Self::ReadError>> + Send;
}

/// 主业务成功且必要资源关闭后，提交最终运行方案短事务。
pub(crate) trait ProjectRunPlanFinalizer: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn replace_final(
        &self,
        database_path: PathBuf,
        replacement: ProjectRunPlanReplacement,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

pub(crate) struct FinalProjectRunPlanPersistenceService<T> {
    transactions: T,
}

impl<T> FinalProjectRunPlanPersistenceService<T> {
    pub(crate) fn new(transactions: T) -> Self {
        Self { transactions }
    }
}

impl<T> ProjectRunPlanFinalizer for FinalProjectRunPlanPersistenceService<T>
where
    T: SqliteFinalTransactionExecutor,
{
    type Error = ProjectRunPlanReplaceError<T::Error>;

    async fn replace_final(
        &self,
        database_path: PathBuf,
        replacement: ProjectRunPlanReplacement,
    ) -> Result<(), Self::Error> {
        self.transactions
            .execute_final_transaction(
                database_path.clone(),
                SqliteTransactionPlan::new(replacement_steps(replacement)),
            )
            .await
            .map_err(|error| ProjectRunPlanReplaceError::from_final_executor(database_path, error))
    }
}

pub(crate) struct ProjectRunPlanPersistenceService<Q> {
    queries: Q,
}

impl<Q> ProjectRunPlanPersistenceService<Q> {
    pub(crate) fn new(queries: Q) -> Self {
        Self { queries }
    }
}

impl<Q> ProjectRunPlanRepository for ProjectRunPlanPersistenceService<Q>
where
    Q: SqliteQueryExecutor,
{
    type ReadError = ProjectRunPlanReadError<Q::Error>;

    async fn read(&self, database_path: PathBuf) -> Result<ProjectRunPlans, Self::ReadError> {
        let mut results = self
            .queries
            .query_existing_database_snapshot(
                database_path.clone(),
                vec![
                    SqliteQuery::new(SELECT_RUN_PLAN_SINGLETONS, Vec::new())
                        .with_id("run_plan.singletons"),
                ],
            )
            .await
            .map_err(|error| {
                ProjectRunPlanReadError::from_executor(database_path.clone(), error)
            })?;
        if results.len() != 1 {
            return Err(ProjectRunPlanReadError::InvalidState {
                path: database_path,
                reason: InvalidProjectRunPlans::unexpected_snapshot_result_sets(results.len()),
            });
        }
        decode_project_run_plans(results.pop().expect("已确认只有一组结果")).map_err(|reason| {
            ProjectRunPlanReadError::InvalidState {
                path: database_path,
                reason,
            }
        })
    }
}

#[derive(Debug)]
pub(crate) enum ProjectRunPlanReadError<E> {
    DatabaseNotFound {
        path: PathBuf,
    },
    ReadDatabase {
        path: PathBuf,
        source: E,
    },
    InvalidState {
        path: PathBuf,
        reason: InvalidProjectRunPlans,
    },
}

impl<E> ProjectRunPlanReadError<E> {
    fn from_executor(path: PathBuf, error: QueryExistingDatabaseError<E>) -> Self {
        match error {
            QueryExistingDatabaseError::NotFound => Self::DatabaseNotFound { path },
            QueryExistingDatabaseError::QueryFailed(source) => Self::ReadDatabase { path, source },
        }
    }
}

impl<E: fmt::Display> fmt::Display for ProjectRunPlanReadError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DatabaseNotFound { path } => {
                write!(formatter, "项目数据库不存在：{}", path.display())
            }
            Self::ReadDatabase { path, source } => {
                write!(
                    formatter,
                    "读取项目运行方案失败 {}：{source}",
                    path.display()
                )
            }
            Self::InvalidState { path, reason } => {
                write!(formatter, "项目运行方案无效 {}：{reason}", path.display())
            }
        }
    }
}

impl<E: Error + 'static> Error for ProjectRunPlanReadError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadDatabase { source, .. } => Some(source),
            Self::InvalidState { reason, .. } => Some(reason),
            Self::DatabaseNotFound { .. } => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum ProjectRunPlanReplaceError<E> {
    DatabaseNotFound { path: PathBuf },
    RequirementFailed { path: PathBuf },
    RollbackConfirmed { path: PathBuf, source: E },
    OutcomeUnknown { path: PathBuf, source: E },
    CommittedButFinalizationFailed { path: PathBuf, source: E },
}

impl<E> ProjectRunPlanReplaceError<E> {
    fn from_final_executor(path: PathBuf, error: ExecuteFinalTransactionError<E>) -> Self {
        match error {
            ExecuteFinalTransactionError::NotFound => Self::DatabaseNotFound { path },
            ExecuteFinalTransactionError::RequirementFailed
            | ExecuteFinalTransactionError::RequirementFailedWithRow { .. } => {
                Self::RequirementFailed { path }
            }
            ExecuteFinalTransactionError::RequirementFailedWithRowOutcomeUnknown {
                source, ..
            } => Self::OutcomeUnknown {
                path,
                source: *source,
            },
            ExecuteFinalTransactionError::RequirementFailedWithRowAndFinalizationFailed {
                source,
                ..
            } => Self::RollbackConfirmed {
                path,
                source: *source,
            },
            ExecuteFinalTransactionError::NotCommitted(source) => {
                Self::RollbackConfirmed { path, source }
            }
            ExecuteFinalTransactionError::OutcomeUnknown(source) => {
                Self::OutcomeUnknown { path, source }
            }
            ExecuteFinalTransactionError::CommittedButFinalizationFailed(source) => {
                Self::CommittedButFinalizationFailed { path, source }
            }
        }
    }
}

impl<E: fmt::Display> fmt::Display for ProjectRunPlanReplaceError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DatabaseNotFound { path } => write!(
                formatter,
                "项目运行方案未保存，因为项目数据库不存在：{}",
                path.display()
            ),
            Self::RequirementFailed { path } => write!(
                formatter,
                "项目运行方案未保存，事务前置条件不成立：{}",
                path.display()
            ),
            Self::RollbackConfirmed { path, source } => write!(
                formatter,
                "项目运行方案事务确认未提交，事务修改未生效 {}：{source}",
                path.display()
            ),
            Self::OutcomeUnknown { path, source } => write!(
                formatter,
                "项目运行方案是否保存无法确认 {}：{source}",
                path.display()
            ),
            Self::CommittedButFinalizationFailed { path, source } => write!(
                formatter,
                "项目运行方案已保存，但最终数据库连接关闭失败 {}：{source}",
                path.display()
            ),
        }
    }
}

impl<E: Error + 'static> Error for ProjectRunPlanReplaceError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::RollbackConfirmed { source, .. }
            | Self::OutcomeUnknown { source, .. }
            | Self::CommittedButFinalizationFailed { source, .. } => Some(source),
            Self::DatabaseNotFound { .. } | Self::RequirementFailed { .. } => None,
        }
    }
}

fn replacement_steps(replacement: ProjectRunPlanReplacement) -> Vec<SqliteTransactionStep> {
    match replacement {
        ProjectRunPlanReplacement::Init(plan) => vec![
            execute(DELETE_INIT_RUN_PLAN, Vec::new()),
            execute(
                INSERT_INIT_RUN_PLAN,
                vec![SqliteValue::Blob(encode_windows_path(plan.source_path()))],
            ),
        ],
        ProjectRunPlanReplacement::Extract(plan) => {
            let mut steps = vec![
                execute(DELETE_EXTRACT_RUN_PLAN, Vec::new()),
                execute(DELETE_EXTRACT_RULES_DEFINITION, Vec::new()),
            ];
            if let Some(plan) = plan {
                steps.push(execute(
                    INSERT_EXTRACT_RUN_PLAN,
                    vec![
                        boolean_value(plan.builtin_enabled()),
                        boolean_value(plan.rules_enabled()),
                    ],
                ));
                if let Some(definition) = plan.rules_definition {
                    steps.push(execute(
                        INSERT_EXTRACT_RULES_DEFINITION,
                        vec![SqliteValue::Text(definition.0)],
                    ));
                }
            }
            steps
        }
        ProjectRunPlanReplacement::Translate(plan) => vec![
            execute(DELETE_TRANSLATE_RUN_PLAN, Vec::new()),
            execute(
                INSERT_TRANSLATE_RUN_PLAN,
                vec![SqliteValue::Text(plan.profile_id)],
            ),
        ],
    }
}

fn execute(statement: &'static str, parameters: Vec<SqliteValue>) -> SqliteTransactionStep {
    SqliteTransactionStep::Execute(SqliteCommand::new(statement, parameters))
}

fn boolean_value(value: bool) -> SqliteValue {
    SqliteValue::Integer(i64::from(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_path(path: &Path) -> SqliteValue {
        SqliteValue::Blob(encode_windows_path(path))
    }

    #[test]
    fn current_run_plan_snapshot_round_trips_all_three_current_phases() {
        let rows = vec![SqliteRow::new(vec![
            encode_path(Path::new("C:/games/alice")),
            SqliteValue::Integer(1),
            SqliteValue::Integer(1),
            SqliteValue::Text(r#"[{"file":"Items.json","path":"[].name"}]"#.to_owned()),
            SqliteValue::Text("primary".to_owned()),
        ])];

        let plans = decode_project_run_plans(rows).expect("当前运行方案应可读取");
        assert_eq!(
            plans.init().expect("Init 方案应存在").source_path(),
            Path::new("C:/games/alice")
        );
        let extract = plans.extract().expect("Extract 方案应存在");
        assert!(extract.builtin_enabled());
        assert!(extract.rules_enabled());
        assert_eq!(
            plans
                .translate()
                .expect("Translate 方案应存在")
                .profile_id(),
            "primary"
        );
    }

    #[test]
    fn extract_rules_flag_and_definition_must_move_together() {
        let error = decode_project_run_plans(vec![SqliteRow::new(vec![
            SqliteValue::Null,
            SqliteValue::Integer(1),
            SqliteValue::Integer(1),
            SqliteValue::Null,
            SqliteValue::Null,
        ])])
        .expect_err("启用 Rules 但没有定义必须失败");
        assert_eq!(error.safe_subject(), "extract_run_plan.rules_enabled");
    }
}
