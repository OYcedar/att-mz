//! RPG Maker 命令上次成功运行方案及可信 Lua 主程序快照。

use std::collections::BTreeMap;
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::future::Future;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::diagnostic::RecoveryFact;
use crate::fingerprint::Sha256Fingerprint;
use crate::json_diagnostic::JsonErrorCategory;
use crate::rpg_maker::extract::rules::{RulesProgramError, validate_rules_canonical_json};
use crate::storage::sqlite::{
    ExecuteFinalTransactionError, QueryExistingDatabaseError, SqliteCommand,
    SqliteFinalTransactionExecutor, SqliteQuery, SqliteQueryExecutor, SqliteRow,
    SqliteTransactionPlan, SqliteTransactionStep, SqliteValue,
};

pub(super) const CREATE_INIT_RUN_PLAN_TABLE: &str = r#"CREATE TABLE init_run_plan (
    singleton                  INTEGER NOT NULL PRIMARY KEY CHECK (singleton = 1),
    source_path_utf16          BLOB NOT NULL CHECK (
        typeof(source_path_utf16) = 'blob'
        AND length(source_path_utf16) > 0
        AND length(source_path_utf16) % 2 = 0
    )
)"#;

pub(super) const CREATE_EXTRACT_RUN_PLAN_TABLE: &str = r#"CREATE TABLE extract_run_plan (
    singleton       INTEGER NOT NULL PRIMARY KEY CHECK (singleton = 1),
    builtin_enabled INTEGER NOT NULL CHECK (builtin_enabled IN (0, 1)),
    rules_enabled   INTEGER NOT NULL CHECK (rules_enabled IN (0, 1)),
    lua_enabled     INTEGER NOT NULL CHECK (lua_enabled IN (0, 1)),
    CHECK (builtin_enabled + rules_enabled + lua_enabled > 0)
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

pub(super) const CREATE_WRITE_BACK_RUN_PLAN_TABLE: &str = r#"CREATE TABLE write_back_run_plan (
    singleton   INTEGER NOT NULL PRIMARY KEY CHECK (singleton = 1),
    lua_enabled INTEGER NOT NULL CHECK (lua_enabled IN (0, 1))
)"#;

pub(super) const CREATE_LUA_PROGRAM_TABLE: &str = r#"CREATE TABLE lua_program (
    phase               TEXT NOT NULL PRIMARY KEY CHECK (phase IN ('extract', 'translate', 'write_back')),
    source              BLOB NOT NULL CHECK (
        typeof(source) = 'blob'
        AND length(source) > 0
    ),
    source_sha256       BLOB NOT NULL CHECK (
        typeof(source_sha256) = 'blob'
        AND length(source_sha256) = 32
    ),
    resolved_path_utf16 BLOB NOT NULL CHECK (
        typeof(resolved_path_utf16) = 'blob'
        AND length(resolved_path_utf16) > 0
        AND length(resolved_path_utf16) % 2 = 0
    )
)"#;

pub(super) const SELECT_RUN_PLAN_SINGLETONS: &str = r#"SELECT
    (SELECT source_path_utf16 FROM init_run_plan WHERE singleton = 1),
    (SELECT builtin_enabled FROM extract_run_plan WHERE singleton = 1),
    (SELECT rules_enabled FROM extract_run_plan WHERE singleton = 1),
    (SELECT lua_enabled FROM extract_run_plan WHERE singleton = 1),
    (SELECT canonical_json FROM extract_rules_definition WHERE singleton = 1),
    (SELECT profile_id FROM translate_run_plan WHERE singleton = 1),
    (SELECT lua_enabled FROM write_back_run_plan WHERE singleton = 1)"#;

pub(super) const SELECT_LUA_PROGRAMS: &str = r#"SELECT
    phase,
    source,
    source_sha256,
    resolved_path_utf16
FROM lua_program
ORDER BY phase"#;

const DELETE_INIT_RUN_PLAN: &str = "DELETE FROM init_run_plan";
const INSERT_INIT_RUN_PLAN: &str =
    "INSERT INTO init_run_plan (singleton, source_path_utf16) VALUES (1, ?1)";
const DELETE_EXTRACT_RUN_PLAN: &str = "DELETE FROM extract_run_plan";
const DELETE_EXTRACT_RULES_DEFINITION: &str = "DELETE FROM extract_rules_definition";
const INSERT_EXTRACT_RUN_PLAN: &str = r#"INSERT INTO extract_run_plan (
    singleton,
    builtin_enabled,
    rules_enabled,
    lua_enabled
) VALUES (1, ?1, ?2, ?3)"#;
const INSERT_EXTRACT_RULES_DEFINITION: &str = r#"INSERT INTO extract_rules_definition (
    singleton,
    canonical_json
) VALUES (1, ?1)"#;
const DELETE_TRANSLATE_RUN_PLAN: &str = "DELETE FROM translate_run_plan";
const INSERT_TRANSLATE_RUN_PLAN: &str =
    "INSERT INTO translate_run_plan (singleton, profile_id) VALUES (1, ?1)";
const DELETE_WRITE_BACK_RUN_PLAN: &str = "DELETE FROM write_back_run_plan";
const INSERT_WRITE_BACK_RUN_PLAN: &str =
    "INSERT INTO write_back_run_plan (singleton, lua_enabled) VALUES (1, ?1)";
const DELETE_LUA_PROGRAM: &str = "DELETE FROM lua_program WHERE phase = ?1";
const INSERT_LUA_PROGRAM: &str = r#"INSERT INTO lua_program (
    phase,
    source,
    source_sha256,
    resolved_path_utf16
) VALUES (?1, ?2, ?3, ?4)"#;

/// 持久化 Lua 主程序所属的独立命令阶段。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum LuaProgramPhase {
    Extract,
    Translate,
    WriteBack,
}

impl LuaProgramPhase {
    pub(crate) const fn storage_name(self) -> &'static str {
        match self {
            Self::Extract => "extract",
            Self::Translate => "translate",
            Self::WriteBack => "write_back",
        }
    }

    const fn from_storage_name(value: &str) -> Option<Self> {
        match value.as_bytes() {
            b"extract" => Some(Self::Extract),
            b"translate" => Some(Self::Translate),
            b"write_back" => Some(Self::WriteBack),
            _ => None,
        }
    }
}

/// 运行方案中允许持久化为 Windows 路径的闭集对象。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunPlanPathPurpose {
    InitSource,
    LuaProgram,
}

impl RunPlanPathPurpose {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::InitSource => "Init 来源",
            Self::LuaProgram => "Lua 主程序",
        }
    }
}

/// `serde_json` 的稳定错误类别；不保存可能含 Rules 正文的错误文本。
pub(crate) type RunPlanJsonErrorCategory = JsonErrorCategory;

/// 一个非空、内容身份已经固定的可信 Lua 主程序。
///
/// `resolved_path` 使用 Windows 原始 UTF-16 单元保存。路径仅用于 chunk 名、模块搜索
/// 根与诊断；执行正文始终来自此快照的 `source`。
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct LuaProgramSnapshot {
    resolved_path: PathBuf,
    source: Vec<u8>,
    source_sha256: Sha256Fingerprint,
}

impl fmt::Debug for LuaProgramSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LuaProgramSnapshot")
            .field("source_bytes", &self.source.len())
            .field("source_sha256", &self.source_sha256)
            .finish_non_exhaustive()
    }
}

impl LuaProgramSnapshot {
    /// 建立 Lua 主程序快照。
    ///
    /// 调用方必须传入文件根已经固定并返回的真实解析路径；持久化边界没有文件系统
    /// 权限，只校验该见证是可无损保存的绝对 Windows 路径。
    pub(crate) fn new(
        resolved_path: PathBuf,
        source: Vec<u8>,
    ) -> Result<Self, InvalidRunPlanValue> {
        validate_resolved_path(&resolved_path, RunPlanPathPurpose::LuaProgram)?;
        if source.is_empty() {
            return Err(InvalidRunPlanValue::EmptyLuaProgram);
        }
        let source_sha256 = Sha256Fingerprint::from_bytes(Sha256::digest(&source).into());
        Ok(Self {
            resolved_path,
            source,
            source_sha256,
        })
    }

    pub(crate) fn resolved_path(&self) -> &Path {
        &self.resolved_path
    }

    pub(crate) fn source(&self) -> &[u8] {
        &self.source
    }

    fn from_stored_parts(
        resolved_path: PathBuf,
        source: Vec<u8>,
        source_sha256: Sha256Fingerprint,
    ) -> Result<Self, InvalidRunPlanValue> {
        let snapshot = Self::new(resolved_path, source)?;
        if snapshot.source_sha256 != source_sha256 {
            return Err(InvalidRunPlanValue::LuaProgramHashMismatch);
        }
        Ok(snapshot)
    }
}

/// Init 上次成功使用的已解析来源目录。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InitRunPlan {
    source_path: PathBuf,
}

impl InitRunPlan {
    /// 保存 Init 已成功使用的来源路径。
    ///
    /// 调用方负责传入 Init 文件系统边界确认过的真实解析路径；此处不重新访问磁盘。
    pub(crate) fn new(source_path: PathBuf) -> Result<Self, InvalidRunPlanValue> {
        validate_resolved_path(&source_path, RunPlanPathPurpose::InitSource)?;
        Ok(Self { source_path })
    }

    pub(crate) fn source_path(&self) -> &Path {
        &self.source_path
    }
}

/// Extract 上次成功使用的完整 owner 集合及可选 Lua 主程序。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExtractRunPlan {
    builtin_enabled: bool,
    rules_definition: Option<ExtractRulesCanonicalJson>,
    lua_program: Option<LuaProgramSnapshot>,
}

impl ExtractRunPlan {
    pub(crate) fn new(
        builtin_enabled: bool,
        rules_definition: Option<ExtractRulesCanonicalJson>,
        lua_program: Option<LuaProgramSnapshot>,
    ) -> Result<Self, InvalidRunPlanValue> {
        if !builtin_enabled && rules_definition.is_none() && lua_program.is_none() {
            return Err(InvalidRunPlanValue::EmptyExtractOwners);
        }
        Ok(Self {
            builtin_enabled,
            rules_definition,
            lua_program,
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

    pub(crate) const fn lua_enabled(&self) -> bool {
        self.lua_program.is_some()
    }

    pub(crate) fn lua_program(&self) -> Option<&LuaProgramSnapshot> {
        self.lua_program.as_ref()
    }
}

/// 已由 Extract Rules 语义所有者验证并规范编码的非空规则集合。
///
/// 构造会交回 Rules 语义所有者完整重建来源、路径和 PCRE2，再确认它是无多余空白
/// 的 canonical JSON 非空数组。原 TOML 文本和文件路径不进入项目状态。
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

fn rules_program_recovery_fact(source: &RulesProgramError) -> Option<RecoveryFact> {
    source
        .safe_diagnostic(Path::new("extract_rules_definition.canonical_json"))
        .recovery
        .into_iter()
        .next()
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Translate 上次成功使用的 Profile 与该阶段当前 Lua 主程序。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslateRunPlan {
    profile_id: String,
    lua_program: Option<LuaProgramSnapshot>,
}

impl TranslateRunPlan {
    pub(crate) fn new(
        profile_id: String,
        lua_program: Option<LuaProgramSnapshot>,
    ) -> Result<Self, InvalidRunPlanValue> {
        if profile_id.trim().is_empty() {
            return Err(InvalidRunPlanValue::EmptyProfileId);
        }
        if profile_id.trim() != profile_id {
            return Err(InvalidRunPlanValue::ProfileIdHasOuterWhitespace);
        }
        Ok(Self {
            profile_id,
            lua_program,
        })
    }

    pub(crate) fn profile_id(&self) -> &str {
        &self.profile_id
    }

    pub(crate) fn lua_program(&self) -> Option<&LuaProgramSnapshot> {
        self.lua_program.as_ref()
    }
}

/// WriteBack 上次成功选择的 Standard-only 或 Standard + Lua 方案。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WriteBackRunPlan {
    lua_program: Option<LuaProgramSnapshot>,
}

impl WriteBackRunPlan {
    pub(crate) const fn standard_only() -> Self {
        Self { lua_program: None }
    }

    pub(crate) fn with_lua(lua_program: LuaProgramSnapshot) -> Self {
        Self {
            lua_program: Some(lua_program),
        }
    }

    pub(crate) const fn lua_enabled(&self) -> bool {
        self.lua_program.is_some()
    }

    pub(crate) fn lua_program(&self) -> Option<&LuaProgramSnapshot> {
        self.lua_program.as_ref()
    }
}

/// 四个命令当前可复用的完整受信状态。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ProjectRunPlans {
    init: Option<InitRunPlan>,
    extract: Option<ExtractRunPlan>,
    translate: Option<TranslateRunPlan>,
    write_back: Option<WriteBackRunPlan>,
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

    pub(crate) fn write_back(&self) -> Option<&WriteBackRunPlan> {
        self.write_back.as_ref()
    }
}

/// 最终短事务要精确替换或清除的一条命令运行方案。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProjectRunPlanReplacement {
    Init(InitRunPlan),
    Extract(Option<ExtractRunPlan>),
    Translate(TranslateRunPlan),
    WriteBack(WriteBackRunPlan),
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
    EmptyLuaProgram,
    LuaProgramHashLength {
        actual: u64,
    },
    LuaProgramHashMismatch,
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
    /// 可供命令边界直接作为结构化诊断对象使用的闭集标签。
    pub(crate) const fn safe_subject(&self) -> &'static str {
        match self {
            Self::EmptyPath { purpose }
            | Self::RelativePath { purpose }
            | Self::PathContainsNul { purpose }
            | Self::InvalidWindowsPathEncoding { purpose } => purpose.as_str(),
            Self::EmptyLuaProgram => "lua_program.source",
            Self::LuaProgramHashLength { .. } | Self::LuaProgramHashMismatch => {
                "lua_program.source_sha256"
            }
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

    /// 只由枚举字段重建的安全详情；不会读取任意错误文本或业务正文。
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
            Self::EmptyLuaProgram => "Lua 主程序正文不能为空".to_owned(),
            Self::LuaProgramHashLength { actual } => {
                format!("Lua source_sha256 应为 32 字节，实际为 {actual} 字节")
            }
            Self::LuaProgramHashMismatch => "Lua 主程序正文与 SHA-256 不匹配".to_owned(),
            Self::EmptyExtractOwners => "Extract 运行方案必须至少包含一个 owner".to_owned(),
            Self::InvalidRulesCanonicalJson {
                line,
                column,
                category,
            } => format!(
                "Extract Rules canonical JSON 无效：类别 {}，第 {line} 行第 {column} 列",
                category.storage_name()
            ),
            Self::RulesCanonicalJsonNotArray => {
                "Extract Rules canonical JSON 根值必须是数组".to_owned()
            }
            Self::RulesCanonicalJsonEncodingFailed {
                line,
                column,
                category,
            } => format!(
                "无法编码 Extract Rules canonical JSON：类别 {}，第 {line} 行第 {column} 列",
                category.storage_name()
            ),
            Self::InvalidRulesSemantics { .. } => {
                "Extract Rules canonical JSON 语义无效".to_owned()
            }
            Self::NonCanonicalRulesJson => "Extract Rules 定义必须使用 canonical JSON".to_owned(),
            Self::EmptyRulesDefinition => {
                "空 Extract Rules 定义表示停用，不能持久化为 active 方案".to_owned()
            }
            Self::EmptyProfileId => "Translate Profile ID 不能为空白".to_owned(),
            Self::ProfileIdHasOuterWhitespace => "Translate Profile ID 不能包含首尾空白".to_owned(),
        }
    }

    /// Rules 语义所有者已经筛选并清理的附加事实，例如规则自然编号与字段名。
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

/// 运行方案快照中的查询身份。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunPlanQuery {
    Snapshot,
    Singletons,
    LuaPrograms,
}

impl RunPlanQuery {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Snapshot => "run_plan.snapshot",
            Self::Singletons => "run_plan.singletons",
            Self::LuaPrograms => "run_plan.lua_programs",
        }
    }
}

/// SQLite 值的稳定存储类型；不携带 TEXT/BLOB 正文。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunPlanSqliteType {
    Null,
    Integer,
    Real,
    Text,
    Blob,
}

impl RunPlanSqliteType {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Null => "NULL",
            Self::Integer => "INTEGER",
            Self::Real => "REAL",
            Self::Text => "TEXT",
            Self::Blob => "BLOB",
        }
    }
}

impl From<&SqliteValue> for RunPlanSqliteType {
    fn from(value: &SqliteValue) -> Self {
        match value {
            SqliteValue::Null => Self::Null,
            SqliteValue::Integer(_) => Self::Integer,
            SqliteValue::Real(_) => Self::Real,
            SqliteValue::Text(_) => Self::Text,
            SqliteValue::Blob(_) => Self::Blob,
        }
    }
}

/// 一个字段允许的 SQLite 存储类型集合。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunPlanExpectedSqliteType {
    Integer,
    Text,
    Blob,
    TextOrNull,
    BlobOrNull,
}

impl RunPlanExpectedSqliteType {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Integer => "INTEGER",
            Self::Text => "TEXT",
            Self::Blob => "BLOB",
            Self::TextOrNull => "TEXT 或 NULL",
            Self::BlobOrNull => "BLOB 或 NULL",
        }
    }
}

/// 运行方案表中的闭集字段身份。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunPlanStorageField {
    InitSourcePath,
    ExtractBuiltinEnabled,
    ExtractRulesEnabled,
    ExtractLuaEnabled,
    ExtractRulesCanonicalJson,
    TranslateProfileId,
    WriteBackLuaEnabled,
    LuaPhase,
    LuaSource,
    LuaSourceSha256,
    LuaResolvedPath,
}

impl RunPlanStorageField {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::InitSourcePath => "init_run_plan.source_path_utf16",
            Self::ExtractBuiltinEnabled => "extract_run_plan.builtin_enabled",
            Self::ExtractRulesEnabled => "extract_run_plan.rules_enabled",
            Self::ExtractLuaEnabled => "extract_run_plan.lua_enabled",
            Self::ExtractRulesCanonicalJson => "extract_rules_definition.canonical_json",
            Self::TranslateProfileId => "translate_run_plan.profile_id",
            Self::WriteBackLuaEnabled => "write_back_run_plan.lua_enabled",
            Self::LuaPhase => "lua_program.phase",
            Self::LuaSource => "lua_program.source",
            Self::LuaSourceSha256 => "lua_program.source_sha256",
            Self::LuaResolvedPath => "lua_program.resolved_path_utf16",
        }
    }
}

/// 运行方案与其旁表内容之间的闭集关联对象。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunPlanComponent {
    ExtractPlan,
    ExtractRulesDefinition,
    ExtractLuaProgram,
    TranslatePlan,
    TranslateLuaProgram,
    WriteBackPlan,
    WriteBackLuaProgram,
}

impl RunPlanComponent {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ExtractPlan => "extract_run_plan",
            Self::ExtractRulesDefinition => "extract_rules_definition",
            Self::ExtractLuaProgram => "lua_program[phase=extract]",
            Self::TranslatePlan => "translate_run_plan",
            Self::TranslateLuaProgram => "lua_program[phase=translate]",
            Self::WriteBackPlan => "write_back_run_plan",
            Self::WriteBackLuaProgram => "lua_program[phase=write_back]",
        }
    }
}

/// 数据库行无法恢复为当前唯一运行方案模型的闭集原因。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum InvalidProjectRunPlanReason {
    UnexpectedResultSetCount {
        query: RunPlanQuery,
        expected: u64,
        actual: u64,
    },
    UnexpectedRowCount {
        query: RunPlanQuery,
        expected: u64,
        actual: u64,
    },
    UnexpectedColumnCount {
        query: RunPlanQuery,
        row: Option<u64>,
        expected: u64,
        actual: u64,
    },
    UnexpectedSqliteType {
        field: RunPlanStorageField,
        row: Option<u64>,
        expected: RunPlanExpectedSqliteType,
        actual: RunPlanSqliteType,
    },
    InvalidBoolean {
        field: RunPlanStorageField,
        actual: i64,
    },
    UnknownLuaPhase {
        row: u64,
        actual_utf8_bytes: u64,
    },
    DuplicateLuaPhase {
        row: u64,
        phase: LuaProgramPhase,
    },
    OrphanedComponent {
        missing: RunPlanComponent,
        present: RunPlanComponent,
    },
    IncompleteExtractOwnerFields {
        builtin_present: bool,
        rules_present: bool,
        lua_present: bool,
    },
    PresenceMismatch {
        flag: RunPlanStorageField,
        component: RunPlanComponent,
        enabled: bool,
        present: bool,
    },
    InvalidValue {
        field: RunPlanStorageField,
        row: Option<u64>,
        reason: InvalidRunPlanValue,
    },
}

/// 数据库行无法恢复为当前唯一运行方案模型。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InvalidProjectRunPlans {
    reason: InvalidProjectRunPlanReason,
}

impl InvalidProjectRunPlans {
    fn from_reason(reason: InvalidProjectRunPlanReason) -> Self {
        Self { reason }
    }

    pub(super) fn unexpected_snapshot_result_sets(actual: usize) -> Self {
        Self::from_reason(InvalidProjectRunPlanReason::UnexpectedResultSetCount {
            query: RunPlanQuery::Snapshot,
            expected: 2,
            actual: usize_to_u64(actual),
        })
    }

    pub(crate) const fn safe_subject(&self) -> &'static str {
        match &self.reason {
            InvalidProjectRunPlanReason::UnexpectedResultSetCount { query, .. }
            | InvalidProjectRunPlanReason::UnexpectedRowCount { query, .. }
            | InvalidProjectRunPlanReason::UnexpectedColumnCount { query, .. } => query.as_str(),
            InvalidProjectRunPlanReason::UnexpectedSqliteType { field, .. }
            | InvalidProjectRunPlanReason::InvalidBoolean { field, .. }
            | InvalidProjectRunPlanReason::InvalidValue { field, .. } => field.as_str(),
            InvalidProjectRunPlanReason::UnknownLuaPhase { .. }
            | InvalidProjectRunPlanReason::DuplicateLuaPhase { .. } => {
                RunPlanStorageField::LuaPhase.as_str()
            }
            InvalidProjectRunPlanReason::OrphanedComponent { missing, .. } => missing.as_str(),
            InvalidProjectRunPlanReason::IncompleteExtractOwnerFields { .. } => {
                RunPlanComponent::ExtractPlan.as_str()
            }
            InvalidProjectRunPlanReason::PresenceMismatch { flag, .. } => flag.as_str(),
        }
    }

    /// 只由闭集原因的数值、类型和静态标签重建；不会读取数据库正文。
    pub(crate) fn safe_detail(&self) -> String {
        match &self.reason {
            InvalidProjectRunPlanReason::UnexpectedResultSetCount {
                query,
                expected,
                actual,
            } => format!(
                "{} 应返回 {expected} 组结果，实际为 {actual} 组",
                query.as_str()
            ),
            InvalidProjectRunPlanReason::UnexpectedRowCount {
                query,
                expected,
                actual,
            } => format!(
                "{} 应返回 {expected} 行，实际为 {actual} 行",
                query.as_str()
            ),
            InvalidProjectRunPlanReason::UnexpectedColumnCount {
                query,
                row,
                expected,
                actual,
            } => format!(
                "{}{}应返回 {expected} 列，实际为 {actual} 列",
                query.as_str(),
                render_row(*row)
            ),
            InvalidProjectRunPlanReason::UnexpectedSqliteType {
                field,
                row,
                expected,
                actual,
            } => format!(
                "{}{}应为 {}，实际为 {}",
                field.as_str(),
                render_row(*row),
                expected.as_str(),
                actual.as_str()
            ),
            InvalidProjectRunPlanReason::InvalidBoolean { field, actual } => {
                format!("{} 只能是枚举值 0 或 1，实际为 {actual}", field.as_str())
            }
            InvalidProjectRunPlanReason::UnknownLuaPhase {
                row,
                actual_utf8_bytes,
            } => format!(
                "lua_program 第 {row} 行 phase 不在枚举 extract/translate/write_back 中；实际 TEXT 为 {actual_utf8_bytes} 字节，仅报告长度以保持诊断 schema 稳定并控制体积"
            ),
            InvalidProjectRunPlanReason::DuplicateLuaPhase { row, phase } => format!(
                "lua_program 第 {row} 行重复 phase 枚举值 {}",
                phase.storage_name()
            ),
            InvalidProjectRunPlanReason::OrphanedComponent { missing, present } => {
                format!("缺少 {}，但存在 {}", missing.as_str(), present.as_str())
            }
            InvalidProjectRunPlanReason::IncompleteExtractOwnerFields {
                builtin_present,
                rules_present,
                lua_present,
            } => format!(
                "extract_run_plan 三个 owner 字段必须同时存在；builtin_enabled 存在={builtin_present}，rules_enabled 存在={rules_present}，lua_enabled 存在={lua_present}"
            ),
            InvalidProjectRunPlanReason::PresenceMismatch {
                flag,
                component,
                enabled,
                present,
            } => format!(
                "{}={enabled} 与 {} 存在={present} 不一致",
                flag.as_str(),
                component.as_str()
            ),
            InvalidProjectRunPlanReason::InvalidValue { field, row, reason } => format!(
                "{}{}无效：{}",
                field.as_str(),
                render_row(*row),
                reason.safe_detail()
            ),
        }
    }

    pub(crate) const fn recovery_fact(&self) -> Option<&RecoveryFact> {
        match &self.reason {
            InvalidProjectRunPlanReason::InvalidValue { reason, .. } => reason.recovery_fact(),
            _ => None,
        }
    }
}

fn render_row(row: Option<u64>) -> String {
    row.map_or_else(String::new, |row| format!(" 第 {row} 行 "))
}

impl fmt::Display for InvalidProjectRunPlans {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.safe_detail())
    }
}

impl Error for InvalidProjectRunPlans {}

pub(super) fn decode_project_run_plans(
    singleton_rows: Vec<SqliteRow>,
    lua_rows: Vec<SqliteRow>,
) -> Result<ProjectRunPlans, InvalidProjectRunPlans> {
    let [singleton_row] = <[SqliteRow; 1]>::try_from(singleton_rows).map_err(|rows| {
        InvalidProjectRunPlans::from_reason(InvalidProjectRunPlanReason::UnexpectedRowCount {
            query: RunPlanQuery::Singletons,
            expected: 1,
            actual: usize_to_u64(rows.len()),
        })
    })?;
    let values = singleton_row.into_values();
    let [
        init_path,
        extract_builtin,
        extract_rules,
        extract_lua,
        extract_rules_definition,
        translate_profile,
        write_back_lua,
    ] = <[SqliteValue; 7]>::try_from(values).map_err(|values| {
        InvalidProjectRunPlans::from_reason(InvalidProjectRunPlanReason::UnexpectedColumnCount {
            query: RunPlanQuery::Singletons,
            row: Some(1),
            expected: 7,
            actual: usize_to_u64(values.len()),
        })
    })?;

    let mut lua_programs = decode_lua_programs(lua_rows)?;
    let init = match init_path {
        SqliteValue::Null => None,
        SqliteValue::Blob(bytes) => Some(
            InitRunPlan::new(
                decode_windows_path(bytes, RunPlanPathPurpose::InitSource).map_err(|reason| {
                    invalid_project_value(RunPlanStorageField::InitSourcePath, None, reason)
                })?,
            )
            .map_err(|reason| {
                invalid_project_value(RunPlanStorageField::InitSourcePath, None, reason)
            })?,
        ),
        value => {
            return Err(unexpected_sqlite_type(
                RunPlanStorageField::InitSourcePath,
                None,
                RunPlanExpectedSqliteType::BlobOrNull,
                &value,
            ));
        }
    };

    let extract_values = [extract_builtin, extract_rules, extract_lua];
    let extract = if extract_values
        .iter()
        .all(|value| matches!(value, SqliteValue::Null))
    {
        if lua_programs.contains_key(&LuaProgramPhase::Extract) {
            return Err(InvalidProjectRunPlans::from_reason(
                InvalidProjectRunPlanReason::OrphanedComponent {
                    missing: RunPlanComponent::ExtractPlan,
                    present: RunPlanComponent::ExtractLuaProgram,
                },
            ));
        }
        if !matches!(extract_rules_definition, SqliteValue::Null) {
            return Err(InvalidProjectRunPlans::from_reason(
                InvalidProjectRunPlanReason::OrphanedComponent {
                    missing: RunPlanComponent::ExtractPlan,
                    present: RunPlanComponent::ExtractRulesDefinition,
                },
            ));
        }
        None
    } else if extract_values
        .iter()
        .any(|value| matches!(value, SqliteValue::Null))
    {
        return Err(InvalidProjectRunPlans::from_reason(
            InvalidProjectRunPlanReason::IncompleteExtractOwnerFields {
                builtin_present: !matches!(extract_values[0], SqliteValue::Null),
                rules_present: !matches!(extract_values[1], SqliteValue::Null),
                lua_present: !matches!(extract_values[2], SqliteValue::Null),
            },
        ));
    } else {
        let builtin = decode_boolean(
            extract_values[0].clone(),
            RunPlanStorageField::ExtractBuiltinEnabled,
        )?;
        let rules = decode_boolean(
            extract_values[1].clone(),
            RunPlanStorageField::ExtractRulesEnabled,
        )?;
        let lua_enabled = decode_boolean(
            extract_values[2].clone(),
            RunPlanStorageField::ExtractLuaEnabled,
        )?;
        let rules_definition = match extract_rules_definition {
            SqliteValue::Null => None,
            SqliteValue::Text(value) => {
                Some(ExtractRulesCanonicalJson::new(value).map_err(|reason| {
                    invalid_project_value(
                        RunPlanStorageField::ExtractRulesCanonicalJson,
                        None,
                        reason,
                    )
                })?)
            }
            value => {
                return Err(unexpected_sqlite_type(
                    RunPlanStorageField::ExtractRulesCanonicalJson,
                    None,
                    RunPlanExpectedSqliteType::TextOrNull,
                    &value,
                ));
            }
        };
        if rules != rules_definition.is_some() {
            return Err(InvalidProjectRunPlans::from_reason(
                InvalidProjectRunPlanReason::PresenceMismatch {
                    flag: RunPlanStorageField::ExtractRulesEnabled,
                    component: RunPlanComponent::ExtractRulesDefinition,
                    enabled: rules,
                    present: rules_definition.is_some(),
                },
            ));
        }
        let lua_program = lua_programs.remove(&LuaProgramPhase::Extract);
        if lua_enabled != lua_program.is_some() {
            return Err(InvalidProjectRunPlans::from_reason(
                InvalidProjectRunPlanReason::PresenceMismatch {
                    flag: RunPlanStorageField::ExtractLuaEnabled,
                    component: RunPlanComponent::ExtractLuaProgram,
                    enabled: lua_enabled,
                    present: lua_program.is_some(),
                },
            ));
        }
        Some(
            ExtractRunPlan::new(builtin, rules_definition, lua_program).map_err(|reason| {
                invalid_project_value(RunPlanStorageField::ExtractBuiltinEnabled, None, reason)
            })?,
        )
    };

    let translate_lua = lua_programs.remove(&LuaProgramPhase::Translate);
    let translate = match translate_profile {
        SqliteValue::Null => {
            if translate_lua.is_some() {
                return Err(InvalidProjectRunPlans::from_reason(
                    InvalidProjectRunPlanReason::OrphanedComponent {
                        missing: RunPlanComponent::TranslatePlan,
                        present: RunPlanComponent::TranslateLuaProgram,
                    },
                ));
            }
            None
        }
        SqliteValue::Text(profile_id) => Some(
            TranslateRunPlan::new(profile_id, translate_lua).map_err(|reason| {
                invalid_project_value(RunPlanStorageField::TranslateProfileId, None, reason)
            })?,
        ),
        value => {
            return Err(unexpected_sqlite_type(
                RunPlanStorageField::TranslateProfileId,
                None,
                RunPlanExpectedSqliteType::TextOrNull,
                &value,
            ));
        }
    };

    let write_back_lua_program = lua_programs.remove(&LuaProgramPhase::WriteBack);
    let write_back = match write_back_lua {
        SqliteValue::Null => {
            if write_back_lua_program.is_some() {
                return Err(InvalidProjectRunPlans::from_reason(
                    InvalidProjectRunPlanReason::OrphanedComponent {
                        missing: RunPlanComponent::WriteBackPlan,
                        present: RunPlanComponent::WriteBackLuaProgram,
                    },
                ));
            }
            None
        }
        value => {
            let enabled = decode_boolean(value, RunPlanStorageField::WriteBackLuaEnabled)?;
            if enabled != write_back_lua_program.is_some() {
                return Err(InvalidProjectRunPlans::from_reason(
                    InvalidProjectRunPlanReason::PresenceMismatch {
                        flag: RunPlanStorageField::WriteBackLuaEnabled,
                        component: RunPlanComponent::WriteBackLuaProgram,
                        enabled,
                        present: write_back_lua_program.is_some(),
                    },
                ));
            }
            Some(
                write_back_lua_program
                    .map_or_else(WriteBackRunPlan::standard_only, WriteBackRunPlan::with_lua),
            )
        }
    };

    debug_assert!(lua_programs.is_empty(), "三个 Lua phase 均应已消费");
    Ok(ProjectRunPlans {
        init,
        extract,
        translate,
        write_back,
    })
}

fn invalid_project_value(
    field: RunPlanStorageField,
    row: Option<u64>,
    reason: InvalidRunPlanValue,
) -> InvalidProjectRunPlans {
    InvalidProjectRunPlans::from_reason(InvalidProjectRunPlanReason::InvalidValue {
        field,
        row,
        reason,
    })
}

fn unexpected_sqlite_type(
    field: RunPlanStorageField,
    row: Option<u64>,
    expected: RunPlanExpectedSqliteType,
    actual: &SqliteValue,
) -> InvalidProjectRunPlans {
    InvalidProjectRunPlans::from_reason(InvalidProjectRunPlanReason::UnexpectedSqliteType {
        field,
        row,
        expected,
        actual: actual.into(),
    })
}

fn decode_boolean(
    value: SqliteValue,
    field: RunPlanStorageField,
) -> Result<bool, InvalidProjectRunPlans> {
    match value {
        SqliteValue::Integer(0) => Ok(false),
        SqliteValue::Integer(1) => Ok(true),
        SqliteValue::Integer(actual) => Err(InvalidProjectRunPlans::from_reason(
            InvalidProjectRunPlanReason::InvalidBoolean { field, actual },
        )),
        value => Err(unexpected_sqlite_type(
            field,
            None,
            RunPlanExpectedSqliteType::Integer,
            &value,
        )),
    }
}

fn decode_lua_programs(
    rows: Vec<SqliteRow>,
) -> Result<BTreeMap<LuaProgramPhase, LuaProgramSnapshot>, InvalidProjectRunPlans> {
    let mut programs = BTreeMap::new();
    for (index, row) in rows.into_iter().enumerate() {
        let row_number = usize_to_u64(index).saturating_add(1);
        let values = row.into_values();
        let [phase, source, source_sha256, resolved_path] = <[SqliteValue; 4]>::try_from(values)
            .map_err(|values| {
                InvalidProjectRunPlans::from_reason(
                    InvalidProjectRunPlanReason::UnexpectedColumnCount {
                        query: RunPlanQuery::LuaPrograms,
                        row: Some(row_number),
                        expected: 4,
                        actual: usize_to_u64(values.len()),
                    },
                )
            })?;
        let phase = match phase {
            SqliteValue::Text(value) => {
                LuaProgramPhase::from_storage_name(&value).ok_or_else(|| {
                    InvalidProjectRunPlans::from_reason(
                        InvalidProjectRunPlanReason::UnknownLuaPhase {
                            row: row_number,
                            actual_utf8_bytes: usize_to_u64(value.len()),
                        },
                    )
                })?
            }
            value => {
                return Err(unexpected_sqlite_type(
                    RunPlanStorageField::LuaPhase,
                    Some(row_number),
                    RunPlanExpectedSqliteType::Text,
                    &value,
                ));
            }
        };
        let source = match source {
            SqliteValue::Blob(value) => value,
            value => {
                return Err(unexpected_sqlite_type(
                    RunPlanStorageField::LuaSource,
                    Some(row_number),
                    RunPlanExpectedSqliteType::Blob,
                    &value,
                ));
            }
        };
        let source_sha256 = match source_sha256 {
            SqliteValue::Blob(value) => Sha256Fingerprint::from_slice(&value).map_err(|error| {
                invalid_project_value(
                    RunPlanStorageField::LuaSourceSha256,
                    Some(row_number),
                    InvalidRunPlanValue::LuaProgramHashLength {
                        actual: usize_to_u64(error.actual()),
                    },
                )
            })?,
            value => {
                return Err(unexpected_sqlite_type(
                    RunPlanStorageField::LuaSourceSha256,
                    Some(row_number),
                    RunPlanExpectedSqliteType::Blob,
                    &value,
                ));
            }
        };
        let resolved_path = match resolved_path {
            SqliteValue::Blob(value) => decode_windows_path(value, RunPlanPathPurpose::LuaProgram)
                .map_err(|reason| {
                    invalid_project_value(
                        RunPlanStorageField::LuaResolvedPath,
                        Some(row_number),
                        reason,
                    )
                })?,
            value => {
                return Err(unexpected_sqlite_type(
                    RunPlanStorageField::LuaResolvedPath,
                    Some(row_number),
                    RunPlanExpectedSqliteType::Blob,
                    &value,
                ));
            }
        };
        let snapshot = LuaProgramSnapshot::from_stored_parts(resolved_path, source, source_sha256)
            .map_err(|reason| {
                invalid_project_value(
                    RunPlanStorageField::LuaSourceSha256,
                    Some(row_number),
                    reason,
                )
            })?;
        if programs.insert(phase, snapshot).is_some() {
            return Err(InvalidProjectRunPlans::from_reason(
                InvalidProjectRunPlanReason::DuplicateLuaPhase {
                    row: row_number,
                    phase,
                },
            ));
        }
    }
    Ok(programs)
}

/// 从主 SQLite 根的一致性快照读取命令运行方案。
///
/// 命令编排方必须在调用 `read` 前取得项目租约，并持续持有到业务执行、全部
/// 必要非日志根资源收尾和最终运行方案事务返回明确终态。方案写入只能
/// 通过 `ProjectRunPlanFinalizer` 完成；主 SQLite 根不提供写方案的捷径。
pub(crate) trait ProjectRunPlanRepository: Send + Sync {
    type ReadError: Error + Send + Sync + 'static;

    fn read(
        &self,
        database_path: PathBuf,
    ) -> impl Future<Output = Result<ProjectRunPlans, Self::ReadError>> + Send;
}

/// 所有主业务根确认关闭后，提交最后运行方案事务的职责。
///
/// 只有业务已成功且每个必要非日志根都确认完成收尾时，编排方才能调用
/// 此职责。业务失败、取消或主根收尾失败时不得尝试替换方案。项目租约必须
/// 持续保持到该调用返回明确终态；一旦 Future 开始执行，不得丢弃它来伪造取消。
pub(crate) trait ProjectRunPlanFinalizer: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    /// 使用独立短生命周期 SQLite 连接替换方案并完成连接关闭。
    fn replace_final(
        &self,
        database_path: PathBuf,
        replacement: ProjectRunPlanReplacement,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 把项目运行方案事务交给独立短生命周期 SQLite 根。
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

/// 使用主 SQLite 根的查询快照读取运行方案。
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
                    SqliteQuery::new(SELECT_LUA_PROGRAMS, Vec::new())
                        .with_id("run_plan.lua_programs"),
                ],
            )
            .await
            .map_err(|error| {
                ProjectRunPlanReadError::from_executor(database_path.clone(), error)
            })?;
        if results.len() != 2 {
            return Err(ProjectRunPlanReadError::InvalidState {
                path: database_path,
                reason: InvalidProjectRunPlans::unexpected_snapshot_result_sets(results.len()),
            });
        }
        let lua_rows = results.pop().expect("已确认有两组运行方案查询结果");
        let singleton_rows = results.pop().expect("已确认有两组运行方案查询结果");
        decode_project_run_plans(singleton_rows, lua_rows).map_err(|reason| {
            ProjectRunPlanReadError::InvalidState {
                path: database_path,
                reason,
            }
        })
    }
}

/// 运行方案读取失败。
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

/// 最终运行方案替换未能确认提交。
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
                delete_lua_program(LuaProgramPhase::Extract),
            ];
            if let Some(plan) = plan {
                steps.push(execute(
                    INSERT_EXTRACT_RUN_PLAN,
                    vec![
                        boolean_value(plan.builtin_enabled()),
                        boolean_value(plan.rules_enabled()),
                        boolean_value(plan.lua_enabled()),
                    ],
                ));
                if let Some(definition) = plan.rules_definition {
                    steps.push(execute(
                        INSERT_EXTRACT_RULES_DEFINITION,
                        vec![SqliteValue::Text(definition.0)],
                    ));
                }
                if let Some(program) = plan.lua_program {
                    steps.push(insert_lua_program(LuaProgramPhase::Extract, program));
                }
            }
            steps
        }
        ProjectRunPlanReplacement::Translate(plan) => {
            let mut steps = vec![
                execute(DELETE_TRANSLATE_RUN_PLAN, Vec::new()),
                delete_lua_program(LuaProgramPhase::Translate),
                execute(
                    INSERT_TRANSLATE_RUN_PLAN,
                    vec![SqliteValue::Text(plan.profile_id)],
                ),
            ];
            if let Some(program) = plan.lua_program {
                steps.push(insert_lua_program(LuaProgramPhase::Translate, program));
            }
            steps
        }
        ProjectRunPlanReplacement::WriteBack(plan) => {
            let mut steps = vec![
                execute(DELETE_WRITE_BACK_RUN_PLAN, Vec::new()),
                delete_lua_program(LuaProgramPhase::WriteBack),
                execute(
                    INSERT_WRITE_BACK_RUN_PLAN,
                    vec![boolean_value(plan.lua_enabled())],
                ),
            ];
            if let Some(program) = plan.lua_program {
                steps.push(insert_lua_program(LuaProgramPhase::WriteBack, program));
            }
            steps
        }
    }
}

fn execute(statement: &'static str, parameters: Vec<SqliteValue>) -> SqliteTransactionStep {
    SqliteTransactionStep::Execute(SqliteCommand::new(statement, parameters))
}

fn boolean_value(value: bool) -> SqliteValue {
    SqliteValue::Integer(i64::from(value))
}

fn delete_lua_program(phase: LuaProgramPhase) -> SqliteTransactionStep {
    execute(
        DELETE_LUA_PROGRAM,
        vec![SqliteValue::Text(phase.storage_name().to_owned())],
    )
}

fn insert_lua_program(
    phase: LuaProgramPhase,
    program: LuaProgramSnapshot,
) -> SqliteTransactionStep {
    execute(
        INSERT_LUA_PROGRAM,
        vec![
            SqliteValue::Text(phase.storage_name().to_owned()),
            SqliteValue::Blob(program.source),
            SqliteValue::Blob(program.source_sha256.as_bytes().to_vec()),
            SqliteValue::Blob(encode_windows_path(&program.resolved_path)),
        ],
    )
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fmt;
    use std::num::NonZeroUsize;

    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::*;
    use crate::runtime::sqlite::{RusqliteFinalTransactionExecutor, RusqliteStorageConfiguration};

    #[derive(Debug)]
    struct TestDriverError;

    impl fmt::Display for TestDriverError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("driver")
        }
    }

    impl Error for TestDriverError {}

    fn nonzero(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).expect("测试资源预算必须非零")
    }

    fn final_transaction_configuration() -> RusqliteStorageConfiguration {
        RusqliteStorageConfiguration::new(nonzero(1), nonzero(1024 * 1024))
    }

    struct OutcomeUnknownFinalExecutor;

    impl SqliteFinalTransactionExecutor for OutcomeUnknownFinalExecutor {
        type Error = TestDriverError;

        async fn execute_final_transaction(
            &self,
            _path: PathBuf,
            _plan: SqliteTransactionPlan,
        ) -> Result<(), ExecuteFinalTransactionError<Self::Error>> {
            Err(ExecuteFinalTransactionError::OutcomeUnknown(
                TestDriverError,
            ))
        }
    }

    fn empty_singletons() -> Vec<SqliteRow> {
        vec![SqliteRow::new(vec![
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
            SqliteValue::Null,
        ])]
    }

    fn lua_row(phase: LuaProgramPhase, path: &Path, source: &[u8]) -> SqliteRow {
        SqliteRow::new(vec![
            SqliteValue::Text(phase.storage_name().to_owned()),
            SqliteValue::Blob(source.to_vec()),
            SqliteValue::Blob(Sha256::digest(source).to_vec()),
            SqliteValue::Blob(encode_windows_path(path)),
        ])
    }

    #[test]
    fn windows_paths_round_trip_without_utf8_conversion() {
        let path = PathBuf::from(OsString::from_wide(&[
            b'C' as u16,
            b':' as u16,
            b'\\' as u16,
            0xd800,
            b'.' as u16,
            b'l' as u16,
            b'u' as u16,
            b'a' as u16,
        ]));

        let encoded = encode_windows_path(&path);
        assert_eq!(
            decode_windows_path(encoded, RunPlanPathPurpose::LuaProgram)
                .expect("任意非 NUL UTF-16 路径应无损恢复"),
            path
        );
    }

    #[test]
    fn exact_run_plan_state_round_trips_and_verifies_lua_hash() {
        let source = b"return { run = function() end }";
        let mut singleton = empty_singletons()[0].clone().into_values();
        singleton[0] = SqliteValue::Blob(encode_windows_path(Path::new("C:/games/demo")));
        singleton[1] = SqliteValue::Integer(1);
        singleton[2] = SqliteValue::Integer(0);
        singleton[3] = SqliteValue::Integer(1);
        singleton[5] = SqliteValue::Text("quality".to_owned());
        singleton[6] = SqliteValue::Integer(0);

        let plans = decode_project_run_plans(
            vec![SqliteRow::new(singleton)],
            vec![lua_row(
                LuaProgramPhase::Extract,
                Path::new("C:/scripts/extract.lua"),
                source,
            )],
        )
        .expect("一致的四命令方案应恢复");

        assert_eq!(
            plans.init().expect("Init 方案应存在").source_path(),
            Path::new("C:/games/demo")
        );
        let extract = plans.extract().expect("Extract 方案应存在");
        assert!(extract.builtin_enabled());
        assert!(!extract.rules_enabled());
        assert_eq!(
            extract.lua_program().expect("Extract Lua 应存在").source(),
            source
        );
        assert_eq!(
            plans
                .translate()
                .expect("Translate 方案应存在")
                .profile_id(),
            "quality"
        );
        assert!(
            !plans
                .write_back()
                .expect("WriteBack 方案应存在")
                .lua_enabled()
        );

        let mut invalid = lua_row(
            LuaProgramPhase::Extract,
            Path::new("C:/scripts/extract.lua"),
            source,
        )
        .into_values();
        invalid[2] = SqliteValue::Blob(vec![0x5a; 32]);
        assert!(
            decode_project_run_plans(
                vec![SqliteRow::new(vec![
                    SqliteValue::Null,
                    SqliteValue::Integer(1),
                    SqliteValue::Integer(0),
                    SqliteValue::Integer(1),
                    SqliteValue::Null,
                    SqliteValue::Null,
                    SqliteValue::Null,
                ])],
                vec![SqliteRow::new(invalid)],
            )
            .is_err()
        );
    }

    #[test]
    fn empty_extract_plan_and_orphan_lua_are_rejected() {
        assert!(matches!(
            ExtractRunPlan::new(false, None, None),
            Err(InvalidRunPlanValue::EmptyExtractOwners)
        ));
        assert!(
            decode_project_run_plans(
                empty_singletons(),
                vec![lua_row(
                    LuaProgramPhase::Translate,
                    Path::new("C:/scripts/translate.lua"),
                    b"return {}",
                )],
            )
            .is_err()
        );
    }

    #[test]
    fn three_lua_phases_and_rules_semantics_remain_independent() {
        let canonical_rules = r#"[{"file":"Actors.json","path":"[].name"}]"#;
        let plans = decode_project_run_plans(
            vec![SqliteRow::new(vec![
                SqliteValue::Null,
                SqliteValue::Integer(0),
                SqliteValue::Integer(1),
                SqliteValue::Integer(1),
                SqliteValue::Text(canonical_rules.to_owned()),
                SqliteValue::Text("balanced".to_owned()),
                SqliteValue::Integer(1),
            ])],
            vec![
                lua_row(
                    LuaProgramPhase::Extract,
                    Path::new("C:/scripts/extract.lua"),
                    b"return { phase = 'extract' }",
                ),
                lua_row(
                    LuaProgramPhase::Translate,
                    Path::new("C:/scripts/translate.lua"),
                    b"return { phase = 'translate' }",
                ),
                lua_row(
                    LuaProgramPhase::WriteBack,
                    Path::new("C:/scripts/write-back.lua"),
                    b"return { phase = 'write-back' }",
                ),
            ],
        )
        .expect("三个 phase 的 Lua 快照应按 key 独立恢复");

        let extract = plans.extract().expect("Extract 方案应存在");
        assert_eq!(
            extract
                .rules_definition()
                .expect("Rules 应 active")
                .as_str(),
            canonical_rules
        );
        assert_eq!(
            extract
                .lua_program()
                .expect("Extract Lua 应存在")
                .resolved_path(),
            Path::new("C:/scripts/extract.lua")
        );
        assert_eq!(
            plans
                .translate()
                .and_then(TranslateRunPlan::lua_program)
                .expect("Translate Lua 应存在")
                .resolved_path(),
            Path::new("C:/scripts/translate.lua")
        );
        assert_eq!(
            plans
                .write_back()
                .and_then(WriteBackRunPlan::lua_program)
                .expect("WriteBack Lua 应存在")
                .resolved_path(),
            Path::new("C:/scripts/write-back.lua")
        );
    }

    #[test]
    fn command_replacement_updates_plan_and_phase_program_in_one_transaction() {
        let program = LuaProgramSnapshot::new(
            PathBuf::from("C:/scripts/extract.lua"),
            b"return {}".to_vec(),
        )
        .expect("测试 Lua 快照应合法");
        let rules = ExtractRulesCanonicalJson::new(
            r#"[{"file":"Actors.json","path":"[].name"}]"#.to_owned(),
        )
        .expect("测试 Rules canonical JSON 应合法");
        let plan = ExtractRunPlan::new(true, Some(rules), Some(program)).expect("owner 集合应非空");

        let steps = replacement_steps(ProjectRunPlanReplacement::Extract(Some(plan)));

        assert_eq!(steps.len(), 6);
        let statements = steps
            .iter()
            .map(|step| match step {
                SqliteTransactionStep::Execute(command) => command.statement(),
                _ => panic!("运行方案替换只应包含写命令"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            statements,
            vec![
                DELETE_EXTRACT_RUN_PLAN,
                DELETE_EXTRACT_RULES_DEFINITION,
                DELETE_LUA_PROGRAM,
                INSERT_EXTRACT_RUN_PLAN,
                INSERT_EXTRACT_RULES_DEFINITION,
                INSERT_LUA_PROGRAM,
            ]
        );

        let clear = replacement_steps(ProjectRunPlanReplacement::Extract(None));
        assert_eq!(
            clear.len(),
            3,
            "清空 Extract 必须同步删除 Rules 与 Lua 快照"
        );
    }

    #[tokio::test]
    async fn rollback_confirmed_preserves_the_complete_previous_translate_plan() {
        let directory = tempdir().expect("应可创建测试目录");
        let database_path = directory.path().join("project.db");
        let old_source = b"return { phase = 'old' }";
        let old_path = Path::new("C:/scripts/old-translate.lua");
        let connection = Connection::open(&database_path).expect("应可创建测试数据库");
        connection
            .execute_batch(&format!(
                "{CREATE_TRANSLATE_RUN_PLAN_TABLE};\n{CREATE_LUA_PROGRAM_TABLE};\n\
                 INSERT INTO translate_run_plan (singleton, profile_id) VALUES (1, 'old-profile');\n\
                 CREATE TRIGGER reject_new_translate_plan\n\
                 BEFORE INSERT ON translate_run_plan\n\
                 WHEN NEW.profile_id = 'new-profile'\n\
                 BEGIN SELECT RAISE(ABORT, 'test rollback'); END;"
            ))
            .expect("应可建立旧方案与失败注入 trigger");
        connection
            .execute(
                INSERT_LUA_PROGRAM,
                rusqlite::params![
                    LuaProgramPhase::Translate.storage_name(),
                    old_source,
                    Sha256::digest(old_source).to_vec(),
                    encode_windows_path(old_path),
                ],
            )
            .expect("应可保存旧 Translate Lua 快照");
        connection.close().expect("测试建库连接应可关闭");

        let new_program = LuaProgramSnapshot::new(
            PathBuf::from("C:/scripts/new-translate.lua"),
            b"return { phase = 'new' }".to_vec(),
        )
        .expect("新 Lua 快照应合法");
        let replacement = TranslateRunPlan::new("new-profile".to_owned(), Some(new_program))
            .map(ProjectRunPlanReplacement::Translate)
            .expect("新 Translate 方案应合法");
        let service = FinalProjectRunPlanPersistenceService::new(
            RusqliteFinalTransactionExecutor::new(final_transaction_configuration()),
        );

        let result = service
            .replace_final(database_path.clone(), replacement)
            .await;

        match result.expect_err("trigger 必须使最终方案事务确认回滚") {
            ProjectRunPlanReplaceError::RollbackConfirmed { path, .. } => {
                assert_eq!(path, database_path);
            }
            other => panic!("应保留确认回滚语义，实际为 {other:?}"),
        }
        let connection = Connection::open(&database_path).expect("应可重新读取项目数据库");
        let profile_id: String = connection
            .query_row(
                "SELECT profile_id FROM translate_run_plan WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .expect("确认回滚后旧 Profile 必须仍存在");
        let (source, resolved_path): (Vec<u8>, Vec<u8>) = connection
            .query_row(
                "SELECT source, resolved_path_utf16 FROM lua_program WHERE phase = 'translate'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("确认回滚后旧 Translate Lua 快照必须仍存在");
        assert_eq!(profile_id, "old-profile");
        assert_eq!(source, old_source);
        assert_eq!(
            decode_windows_path(resolved_path, RunPlanPathPurpose::LuaProgram)
                .expect("旧 Lua 路径应保持合法"),
            old_path
        );
        connection.close().expect("测试读取连接应可关闭");
    }

    #[tokio::test]
    async fn outcome_unknown_is_not_reported_as_a_saved_replacement() {
        let database_path = PathBuf::from("C:/projects/demo/project.db");
        let replacement = TranslateRunPlan::new("new-profile".to_owned(), None)
            .map(ProjectRunPlanReplacement::Translate)
            .expect("测试 Translate 方案应合法");
        let service = FinalProjectRunPlanPersistenceService::new(OutcomeUnknownFinalExecutor);

        let result = service
            .replace_final(database_path.clone(), replacement)
            .await;

        match result.expect_err("提交终态未知不得被伪造成方案保存成功") {
            ProjectRunPlanReplaceError::OutcomeUnknown { path, .. } => {
                assert_eq!(path, database_path);
            }
            other => panic!("应保留提交终态未知语义，实际为 {other:?}"),
        }
    }

    #[test]
    fn final_transaction_terminal_states_remain_precise() {
        let path = PathBuf::from("C:/projects/demo/project.db");
        assert!(matches!(
            ProjectRunPlanReplaceError::<TestDriverError>::from_final_executor(
                path.clone(),
                ExecuteFinalTransactionError::NotFound
            ),
            ProjectRunPlanReplaceError::DatabaseNotFound { .. }
        ));
        assert!(matches!(
            ProjectRunPlanReplaceError::<TestDriverError>::from_final_executor(
                path.clone(),
                ExecuteFinalTransactionError::RequirementFailed
            ),
            ProjectRunPlanReplaceError::RequirementFailed { .. }
        ));
        assert!(matches!(
            ProjectRunPlanReplaceError::from_final_executor(
                path.clone(),
                ExecuteFinalTransactionError::NotCommitted(TestDriverError)
            ),
            ProjectRunPlanReplaceError::RollbackConfirmed { .. }
        ));
        assert!(matches!(
            ProjectRunPlanReplaceError::from_final_executor(
                path.clone(),
                ExecuteFinalTransactionError::OutcomeUnknown(TestDriverError)
            ),
            ProjectRunPlanReplaceError::OutcomeUnknown { .. }
        ));
        assert!(matches!(
            ProjectRunPlanReplaceError::from_final_executor(
                path,
                ExecuteFinalTransactionError::CommittedButFinalizationFailed(TestDriverError)
            ),
            ProjectRunPlanReplaceError::CommittedButFinalizationFailed { .. }
        ));
    }
}
