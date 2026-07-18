//! 项目数据库的创建职责。

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};

use crate::att_mz::ProjectName;
use crate::att_mz::standard_asset::MzStandardAssetOwner;
use crate::fingerprint::{InvalidSha256FingerprintLength, Sha256Fingerprint};
use crate::storage::sqlite::{
    CreateDatabaseError, ExecuteTransactionError, QueryExistingDatabaseError, SqliteCommand,
    SqliteDatabaseCreator, SqliteQuery, SqliteQueryExecutor, SqliteRow, SqliteTransactionExecutor,
    SqliteTransactionPlan, SqliteTransactionStep, SqliteValue,
};

const PROJECT_DATABASE_FILE_NAME: &str = "project.db";
const CREATE_METADATA_TABLE: &str = r#"CREATE TABLE metadata (
    name                                TEXT NOT NULL PRIMARY KEY,
    source_language                     TEXT NOT NULL,
    target_language                     TEXT NOT NULL,
    source_snapshot_fingerprint         BLOB NOT NULL CHECK (
        typeof(source_snapshot_fingerprint) = 'blob'
        AND length(source_snapshot_fingerprint) = 32
    ),
    dialogue_max_fullwidth_chars        INTEGER NOT NULL CHECK (dialogue_max_fullwidth_chars > 0),
    scrolling_text_max_fullwidth_chars  INTEGER NOT NULL CHECK (scrolling_text_max_fullwidth_chars > 0),
    help_description_max_fullwidth_chars INTEGER NOT NULL CHECK (help_description_max_fullwidth_chars > 0)
)"#;

const CREATE_STANDARD_ASSET_OWNER_STATE_TABLE: &str = r#"CREATE TABLE standard_asset_owner_state (
    owner                       TEXT NOT NULL PRIMARY KEY CHECK (owner IN ('builtin', 'rules', 'lua')),
    source_snapshot_fingerprint BLOB NOT NULL CHECK (
        typeof(source_snapshot_fingerprint) = 'blob'
        AND length(source_snapshot_fingerprint) = 32
    )
)"#;

const CREATE_ENTRY_TABLE: &str = r#"CREATE TABLE entry (
    owner             TEXT NOT NULL CHECK (owner IN ('builtin', 'rules', 'lua')),
    exact_location    TEXT NOT NULL,
    group_location    TEXT NOT NULL,
    field_name        TEXT NOT NULL,
    original_text     TEXT NOT NULL,
    translation       TEXT,
    translation_state BLOB,
    PRIMARY KEY (owner, exact_location),
    FOREIGN KEY (owner) REFERENCES standard_asset_owner_state(owner) ON DELETE CASCADE,
    CHECK (
        (translation IS NULL AND translation_state IS NULL)
        OR (
            translation IS NOT NULL
            AND typeof(translation_state) = 'blob'
            AND length(translation_state) = 32
        )
    )
)"#;

const CREATE_SYSTEM_TEXT_TABLE: &str = r#"CREATE TABLE system_text (
    owner             TEXT NOT NULL CHECK (owner IN ('builtin', 'rules', 'lua')),
    exact_location    TEXT NOT NULL,
    group_location    TEXT NOT NULL,
    field_name        TEXT NOT NULL,
    original_text     TEXT NOT NULL,
    translation       TEXT,
    translation_state BLOB,
    PRIMARY KEY (owner, exact_location),
    FOREIGN KEY (owner) REFERENCES standard_asset_owner_state(owner) ON DELETE CASCADE,
    CHECK (
        (translation IS NULL AND translation_state IS NULL)
        OR (
            translation IS NOT NULL
            AND typeof(translation_state) = 'blob'
            AND length(translation_state) = 32
        )
    )
)"#;

const CREATE_MAP_TEXT_TABLE: &str = r#"CREATE TABLE map_text (
    owner             TEXT NOT NULL CHECK (owner IN ('builtin', 'rules', 'lua')),
    exact_location    TEXT NOT NULL,
    group_location    TEXT NOT NULL,
    field_name        TEXT NOT NULL,
    original_text     TEXT NOT NULL,
    translation       TEXT,
    translation_state BLOB,
    PRIMARY KEY (owner, exact_location),
    FOREIGN KEY (owner) REFERENCES standard_asset_owner_state(owner) ON DELETE CASCADE,
    CHECK (
        (translation IS NULL AND translation_state IS NULL)
        OR (
            translation IS NOT NULL
            AND typeof(translation_state) = 'blob'
            AND length(translation_state) = 32
        )
    )
)"#;

const CREATE_TEXT_BODY_TABLE: &str = r#"CREATE TABLE text_body (
    owner             TEXT NOT NULL CHECK (owner IN ('builtin', 'rules', 'lua')),
    exact_location    TEXT NOT NULL,
    group_location    TEXT NOT NULL,
    field_name        TEXT NOT NULL,
    unit_type         TEXT NOT NULL CHECK (
        unit_type IN ('dialogue', 'choices', 'scrolling_text', 'event_command')
    ),
    original_text     TEXT NOT NULL,
    translation       TEXT,
    translation_state BLOB,
    PRIMARY KEY (owner, exact_location),
    FOREIGN KEY (owner) REFERENCES standard_asset_owner_state(owner) ON DELETE CASCADE,
    CHECK (
        (translation IS NULL AND translation_state IS NULL)
        OR (
            translation IS NOT NULL
            AND typeof(translation_state) = 'blob'
            AND length(translation_state) = 32
        )
    )
)"#;

const CREATE_PLUGIN_PARAM_TABLE: &str = r#"CREATE TABLE plugin_param (
    owner             TEXT NOT NULL CHECK (owner IN ('builtin', 'rules', 'lua')),
    exact_location    TEXT NOT NULL,
    group_location    TEXT NOT NULL,
    field_name        TEXT NOT NULL,
    original_text     TEXT NOT NULL,
    translation       TEXT,
    translation_state BLOB,
    PRIMARY KEY (owner, exact_location),
    FOREIGN KEY (owner) REFERENCES standard_asset_owner_state(owner) ON DELETE CASCADE,
    CHECK (
        (translation IS NULL AND translation_state IS NULL)
        OR (
            translation IS NOT NULL
            AND typeof(translation_state) = 'blob'
            AND length(translation_state) = 32
        )
    )
)"#;

const CREATE_ENTRY_EXACT_LOCATION_INDEX: &str =
    "CREATE INDEX entry_exact_location_idx ON entry(exact_location)";
const CREATE_SYSTEM_TEXT_EXACT_LOCATION_INDEX: &str =
    "CREATE INDEX system_text_exact_location_idx ON system_text(exact_location)";
const CREATE_MAP_TEXT_EXACT_LOCATION_INDEX: &str =
    "CREATE INDEX map_text_exact_location_idx ON map_text(exact_location)";
const CREATE_TEXT_BODY_EXACT_LOCATION_INDEX: &str =
    "CREATE INDEX text_body_exact_location_idx ON text_body(exact_location)";
const CREATE_PLUGIN_PARAM_EXACT_LOCATION_INDEX: &str =
    "CREATE INDEX plugin_param_exact_location_idx ON plugin_param(exact_location)";

pub(crate) const STANDARD_TRANSLATION_RESOURCE_TABLE_NAME: &str = "standard_translation_resource";
pub(crate) const TERMINOLOGY_RESOURCE_KIND: &str = "terminology";
pub(crate) const PLACEHOLDER_RULES_RESOURCE_KIND: &str = "placeholder_rules";

const CREATE_STANDARD_TRANSLATION_RESOURCE_TABLE: &str = r#"CREATE TABLE standard_translation_resource (
    resource_kind  TEXT NOT NULL PRIMARY KEY CHECK (
        resource_kind IN ('terminology', 'placeholder_rules')
    ),
    canonical_json TEXT NOT NULL CHECK (length(canonical_json) > 0)
)"#;

const INSERT_METADATA: &str = r#"INSERT INTO metadata (
    name,
    source_language,
    target_language,
    source_snapshot_fingerprint,
    dialogue_max_fullwidth_chars,
    scrolling_text_max_fullwidth_chars,
    help_description_max_fullwidth_chars
) VALUES (?, ?, ?, ?, ?, ?, ?)"#;
const INSERT_STANDARD_TRANSLATION_RESOURCE: &str = r#"INSERT INTO standard_translation_resource (
    resource_kind,
    canonical_json
) VALUES (?, ?)"#;
const SELECT_METADATA: &str = r#"SELECT
    name,
    source_language,
    target_language,
    source_snapshot_fingerprint,
    dialogue_max_fullwidth_chars,
    scrolling_text_max_fullwidth_chars,
    help_description_max_fullwidth_chars
FROM metadata"#;

const SELECT_SCHEMA_VERSION: &str = "SELECT schema_version FROM pragma_schema_version";
const SELECT_MANAGED_SCHEMA: &str = r#"SELECT type, name, tbl_name, sql
FROM sqlite_schema
WHERE sql IS NOT NULL
  AND (
    tbl_name IN (
      'metadata',
      'standard_asset_owner_state',
      'entry',
      'system_text',
      'map_text',
      'text_body',
      'plugin_param',
      'standard_translation_resource'
    )
    OR name IN (
      'entry_exact_location_idx',
      'system_text_exact_location_idx',
      'map_text_exact_location_idx',
      'text_body_exact_location_idx',
      'plugin_param_exact_location_idx'
    )
  )
ORDER BY type, name"#;
const SELECT_OWNER_STATES: &str = r#"SELECT owner, source_snapshot_fingerprint
FROM standard_asset_owner_state
ORDER BY owner"#;
const SELECT_TRANSLATION_RESOURCES: &str = r#"SELECT resource_kind, canonical_json
FROM standard_translation_resource
ORDER BY resource_kind"#;
const SELECT_QUICK_CHECK: &str = "PRAGMA quick_check";
const SELECT_FOREIGN_KEY_CHECK: &str = "PRAGMA foreign_key_check";

const UPDATE_METADATA: &str = r#"UPDATE metadata
SET source_language = ?1,
    target_language = ?2,
    source_snapshot_fingerprint = ?3,
    dialogue_max_fullwidth_chars = ?4,
    scrolling_text_max_fullwidth_chars = ?5,
    help_description_max_fullwidth_chars = ?6
WHERE name = ?7"#;
const CLEAR_ENTRY_TRANSLATIONS: &str = "UPDATE entry SET translation = NULL, translation_state = NULL WHERE translation IS NOT NULL OR translation_state IS NOT NULL";
const CLEAR_SYSTEM_TEXT_TRANSLATIONS: &str = "UPDATE system_text SET translation = NULL, translation_state = NULL WHERE translation IS NOT NULL OR translation_state IS NOT NULL";
const CLEAR_MAP_TEXT_TRANSLATIONS: &str = "UPDATE map_text SET translation = NULL, translation_state = NULL WHERE translation IS NOT NULL OR translation_state IS NOT NULL";
const CLEAR_TEXT_BODY_TRANSLATIONS: &str = "UPDATE text_body SET translation = NULL, translation_state = NULL WHERE translation IS NOT NULL OR translation_state IS NOT NULL";
const CLEAR_PLUGIN_PARAM_TRANSLATIONS: &str = "UPDATE plugin_param SET translation = NULL, translation_state = NULL WHERE translation IS NOT NULL OR translation_state IS NOT NULL";
const RESET_TERMINOLOGY_RESOURCE: &str = r#"UPDATE standard_translation_resource
SET canonical_json = '[]'
WHERE resource_kind = 'terminology'"#;

/// 冻结 `source/data` 与 `source/js` 完整内容的精确身份。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct SourceSnapshotFingerprint(Sha256Fingerprint);

impl SourceSnapshotFingerprint {
    pub(crate) const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Sha256Fingerprint::from_bytes(bytes))
    }

    pub(crate) fn from_slice(bytes: &[u8]) -> Result<Self, InvalidSha256FingerprintLength> {
        Sha256Fingerprint::from_slice(bytes).map(Self)
    }

    pub(crate) const fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }
}

/// 一个 MZ 项目工作区中所有固定位置的唯一派生结果。
///
/// 工作区创建、数据库读取、项目开启与写回都从该值取得路径，避免各自重新解释
/// 工作区结构。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectWorkspaceLayout {
    workspace_root: PathBuf,
    database_path: PathBuf,
    source_root: PathBuf,
    source_data: PathBuf,
    source_js: PathBuf,
    write_back_root: PathBuf,
}

impl ProjectWorkspaceLayout {
    /// 从项目集合根和受信项目名定位工作区。
    pub(crate) fn for_project(projects_root: &Path, name: &ProjectName) -> Self {
        Self::from_workspace_root(projects_root.join(name.as_str()))
    }

    /// 从已经确定的工作区根建立全部固定位置。
    pub(crate) fn from_workspace_root(workspace_root: PathBuf) -> Self {
        let database_path = workspace_root.join(PROJECT_DATABASE_FILE_NAME);
        let source_root = workspace_root.join("source");
        let source_data = source_root.join("data");
        let source_js = source_root.join("js");
        let write_back_root = workspace_root.join("write_back");

        Self {
            workspace_root,
            database_path,
            source_root,
            source_data,
            source_js,
            write_back_root,
        }
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

    pub(crate) fn source_data(&self) -> &Path {
        &self.source_data
    }

    pub(crate) fn source_js(&self) -> &Path {
        &self.source_js
    }

    pub(crate) fn write_back_root(&self) -> &Path {
        &self.write_back_root
    }
}

/// 一个游戏显示区域允许的每行最大全角字符数。
///
/// 零不能表达可用的显示宽度，因此只能通过受检构造建立该值。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaxFullwidthChars(u32);

impl MaxFullwidthChars {
    /// 建立一个严格大于零的显示宽度。
    pub fn new(value: u32) -> Result<Self, MaxFullwidthCharsError> {
        if value == 0 {
            Err(MaxFullwidthCharsError)
        } else {
            Ok(Self(value))
        }
    }

    /// 返回每行最大全角字符数。
    pub fn get(self) -> u32 {
        self.0
    }
}

impl TryFrom<u32> for MaxFullwidthChars {
    type Error = MaxFullwidthCharsError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// 每行最大全角字符数不是正整数。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaxFullwidthCharsError;

impl fmt::Display for MaxFullwidthCharsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("每行最大全角字符数必须大于零")
    }
}

impl Error for MaxFullwidthCharsError {}

/// MZ 标准写回所使用的三个显示区域宽度。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MzWriteBackLayoutProfile {
    dialogue_body: MaxFullwidthChars,
    scrolling_text: MaxFullwidthChars,
    help_description: MaxFullwidthChars,
}

impl MzWriteBackLayoutProfile {
    /// 汇集已经分别校验的显示区域宽度。
    pub fn new(
        dialogue_body: MaxFullwidthChars,
        scrolling_text: MaxFullwidthChars,
        help_description: MaxFullwidthChars,
    ) -> Self {
        Self {
            dialogue_body,
            scrolling_text,
            help_description,
        }
    }

    pub fn dialogue_body(&self) -> MaxFullwidthChars {
        self.dialogue_body
    }

    pub fn scrolling_text(&self) -> MaxFullwidthChars {
        self.scrolling_text
    }

    pub fn help_description(&self) -> MaxFullwidthChars {
        self.help_description
    }
}

/// 从项目数据库中读取的受信项目记录。
///
/// 数据库定位、metadata 读取和记录完整性由读取器负责；消费方无需再次解释
/// SQLite 表或字段。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredProjectRecord {
    name: ProjectName,
    layout: ProjectWorkspaceLayout,
    source_language: String,
    target_language: String,
    source_snapshot_fingerprint: SourceSnapshotFingerprint,
    layout_profile: MzWriteBackLayoutProfile,
}

impl StoredProjectRecord {
    /// 建立一条已经由项目数据库读取器确认可信的记录。
    #[cfg(test)]
    pub(crate) fn new(
        name: ProjectName,
        workspace_root: PathBuf,
        database_path: PathBuf,
        source_language: String,
        target_language: String,
        layout_profile: MzWriteBackLayoutProfile,
    ) -> Self {
        let layout = ProjectWorkspaceLayout::from_workspace_root(workspace_root);
        assert_eq!(
            layout.database_path(),
            database_path,
            "受信项目记录的数据库路径必须属于同一工作区布局"
        );
        Self::from_layout(
            name,
            layout,
            source_language,
            target_language,
            SourceSnapshotFingerprint::from_bytes([0xa5; 32]),
            layout_profile,
        )
    }

    /// 直接复用已经建立的工作区布局。
    pub(crate) fn from_layout(
        name: ProjectName,
        layout: ProjectWorkspaceLayout,
        source_language: String,
        target_language: String,
        source_snapshot_fingerprint: SourceSnapshotFingerprint,
        layout_profile: MzWriteBackLayoutProfile,
    ) -> Self {
        Self {
            name,
            layout,
            source_language,
            target_language,
            source_snapshot_fingerprint,
            layout_profile,
        }
    }

    pub(crate) fn name(&self) -> &ProjectName {
        &self.name
    }

    pub(crate) fn layout(&self) -> &ProjectWorkspaceLayout {
        &self.layout
    }

    pub(crate) fn workspace_root(&self) -> &Path {
        self.layout.workspace_root()
    }

    pub(crate) fn source_root(&self) -> &Path {
        self.layout.source_root()
    }

    pub(crate) fn database_path(&self) -> &Path {
        self.layout.database_path()
    }

    pub(crate) fn source_language(&self) -> &str {
        &self.source_language
    }

    pub(crate) fn target_language(&self) -> &str {
        &self.target_language
    }

    pub(crate) const fn source_snapshot_fingerprint(&self) -> SourceSnapshotFingerprint {
        self.source_snapshot_fingerprint
    }

    pub(crate) fn layout_profile(&self) -> &MzWriteBackLayoutProfile {
        &self.layout_profile
    }
}

/// 按项目名称读取现存项目记录的职责契约。
pub(crate) trait ProjectDatabaseRecordReader: Send + Sync {
    /// 项目记录定位、打开或读取失败。
    type Error: Error + Send + Sync + 'static;

    /// 读取一个现存项目的完整受信记录。
    ///
    /// 实现负责定位 `<name>/project.db`、以只读意图打开数据库，并确认 metadata 记录
    /// 完整且属于请求的项目。不存在、损坏与底层数据库失败均通过本职责错误返回。
    fn read(
        &self,
        name: &ProjectName,
    ) -> impl Future<Output = Result<StoredProjectRecord, Self::Error>> + Send;
}

/// 使用只读 SQLite 查询建立受信项目记录。
pub(crate) struct ProjectDatabaseRecordReadingService<S> {
    projects_root: PathBuf,
    sqlite: S,
}

impl<S> ProjectDatabaseRecordReadingService<S> {
    /// 创建服务；项目工作区根目录由外部配置边界明确注入。
    pub(crate) fn new(projects_root: PathBuf, sqlite: S) -> Self {
        Self {
            projects_root,
            sqlite,
        }
    }
}

impl<S> ProjectDatabaseRecordReader for ProjectDatabaseRecordReadingService<S>
where
    S: SqliteQueryExecutor,
{
    type Error = ProjectDatabaseReadError<S::Error>;

    async fn read(&self, requested_name: &ProjectName) -> Result<StoredProjectRecord, Self::Error> {
        let layout = ProjectWorkspaceLayout::for_project(&self.projects_root, requested_name);
        let database_path = layout.database_path().to_path_buf();
        let rows = self
            .sqlite
            .query_existing_database(
                database_path.clone(),
                SqliteQuery::new(SELECT_METADATA, Vec::new()),
            )
            .await
            .map_err(|error| {
                ProjectDatabaseReadError::from_executor(database_path.clone(), error)
            })?;

        record_from_rows(requested_name, layout, rows)
    }
}

fn record_from_rows<E>(
    requested_name: &ProjectName,
    layout: ProjectWorkspaceLayout,
    rows: Vec<SqliteRow>,
) -> Result<StoredProjectRecord, ProjectDatabaseReadError<E>> {
    let database_path = layout.database_path().to_path_buf();
    let metadata = metadata_facts_from_rows(requested_name, rows).map_err(|reason| {
        ProjectDatabaseReadError::InvalidMetadata {
            path: database_path,
            reason,
        }
    })?;

    Ok(StoredProjectRecord::from_layout(
        metadata.name,
        layout,
        metadata.source_language,
        metadata.target_language,
        metadata.source_snapshot_fingerprint,
        metadata.layout_profile,
    ))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProjectMetadataFacts {
    name: ProjectName,
    source_language: String,
    target_language: String,
    source_snapshot_fingerprint: SourceSnapshotFingerprint,
    layout_profile: MzWriteBackLayoutProfile,
}

fn metadata_facts_from_rows(
    requested_name: &ProjectName,
    rows: Vec<SqliteRow>,
) -> Result<ProjectMetadataFacts, InvalidProjectMetadata> {
    let mut rows = rows.into_iter();
    let row = rows.next().ok_or(InvalidProjectMetadata::MissingRow)?;

    if rows.next().is_some() {
        return Err(InvalidProjectMetadata::MultipleRows);
    }

    let values = row.into_values();
    if values.len() != 7 {
        return Err(InvalidProjectMetadata::WrongColumnCount {
            actual: values.len(),
        });
    }

    let mut values = values.into_iter();
    let stored_name = text_column(values.next().expect("已确认 metadata 恰好有七列"), "name")?;
    let source_language = text_column(
        values.next().expect("已确认 metadata 恰好有七列"),
        "source_language",
    )?;
    let target_language = text_column(
        values.next().expect("已确认 metadata 恰好有七列"),
        "target_language",
    )?;
    let source_snapshot_fingerprint =
        source_snapshot_fingerprint_column(values.next().expect("已确认 metadata 恰好有七列"))?;
    let dialogue_max_fullwidth_chars = max_fullwidth_chars_column(
        values.next().expect("已确认 metadata 恰好有七列"),
        "dialogue_max_fullwidth_chars",
    )?;
    let scrolling_text_max_fullwidth_chars = max_fullwidth_chars_column(
        values.next().expect("已确认 metadata 恰好有七列"),
        "scrolling_text_max_fullwidth_chars",
    )?;
    let help_description_max_fullwidth_chars = max_fullwidth_chars_column(
        values.next().expect("已确认 metadata 恰好有七列"),
        "help_description_max_fullwidth_chars",
    )?;

    let stored_name = stored_name
        .parse::<ProjectName>()
        .map_err(|message| InvalidProjectMetadata::InvalidProjectName { message })?;
    if &stored_name != requested_name {
        return Err(InvalidProjectMetadata::NameMismatch {
            requested: requested_name.as_str().to_owned(),
            stored: stored_name.as_str().to_owned(),
        });
    }

    if source_language.trim().is_empty() {
        return Err(InvalidProjectMetadata::BlankLanguage {
            column: "source_language",
        });
    }
    if target_language.trim().is_empty() {
        return Err(InvalidProjectMetadata::BlankLanguage {
            column: "target_language",
        });
    }

    Ok(ProjectMetadataFacts {
        name: stored_name,
        source_language,
        target_language,
        source_snapshot_fingerprint,
        layout_profile: MzWriteBackLayoutProfile::new(
            dialogue_max_fullwidth_chars,
            scrolling_text_max_fullwidth_chars,
            help_description_max_fullwidth_chars,
        ),
    })
}

fn text_column(value: SqliteValue, column: &'static str) -> Result<String, InvalidProjectMetadata> {
    match value {
        SqliteValue::Text(value) => Ok(value),
        value => Err(InvalidProjectMetadata::WrongColumnType {
            column,
            expected: "TEXT",
            actual: value.kind_name(),
        }),
    }
}

fn source_snapshot_fingerprint_column(
    value: SqliteValue,
) -> Result<SourceSnapshotFingerprint, InvalidProjectMetadata> {
    let SqliteValue::Blob(value) = value else {
        return Err(InvalidProjectMetadata::WrongColumnType {
            column: "source_snapshot_fingerprint",
            expected: "BLOB",
            actual: value.kind_name(),
        });
    };
    SourceSnapshotFingerprint::from_slice(&value).map_err(|source| {
        InvalidProjectMetadata::InvalidSourceSnapshotFingerprintLength {
            actual: source.actual(),
        }
    })
}

fn max_fullwidth_chars_column(
    value: SqliteValue,
    column: &'static str,
) -> Result<MaxFullwidthChars, InvalidProjectMetadata> {
    let SqliteValue::Integer(value) = value else {
        return Err(InvalidProjectMetadata::WrongColumnType {
            column,
            expected: "INTEGER",
            actual: value.kind_name(),
        });
    };

    let value = u32::try_from(value).map_err(|_| InvalidProjectMetadata::InvalidLineWidth {
        column,
        actual: value,
    })?;
    MaxFullwidthChars::new(value).map_err(|_| InvalidProjectMetadata::InvalidLineWidth {
        column,
        actual: i64::from(value),
    })
}

/// 项目数据库读取失败，并保留目标路径和底层原因。
#[derive(Debug)]
pub(crate) enum ProjectDatabaseReadError<E> {
    DatabaseNotFound {
        path: PathBuf,
    },
    ReadDatabase {
        path: PathBuf,
        source: E,
    },
    InvalidMetadata {
        path: PathBuf,
        reason: InvalidProjectMetadata,
    },
}

impl<E> ProjectDatabaseReadError<E> {
    fn from_executor(path: PathBuf, error: QueryExistingDatabaseError<E>) -> Self {
        match error {
            QueryExistingDatabaseError::NotFound => Self::DatabaseNotFound { path },
            QueryExistingDatabaseError::QueryFailed(source) => Self::ReadDatabase { path, source },
        }
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::DatabaseNotFound { path }
            | Self::ReadDatabase { path, .. }
            | Self::InvalidMetadata { path, .. } => path,
        }
    }
}

impl<E: fmt::Display> fmt::Display for ProjectDatabaseReadError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DatabaseNotFound { path } => {
                write!(formatter, "项目数据库不存在：{}", path.display())
            }
            Self::ReadDatabase { path, source } => {
                write!(formatter, "无法读取项目数据库 {}：{source}", path.display())
            }
            Self::InvalidMetadata { path, reason } => {
                write!(
                    formatter,
                    "项目数据库 metadata 无效 {}：{reason}",
                    path.display()
                )
            }
        }
    }
}

impl<E: Error + 'static> Error for ProjectDatabaseReadError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadDatabase { source, .. } => Some(source),
            Self::DatabaseNotFound { .. } | Self::InvalidMetadata { .. } => None,
        }
    }
}

/// metadata 不能重新建立为内部受信项目事实的具体原因。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum InvalidProjectMetadata {
    MissingRow,
    MultipleRows,
    WrongColumnCount {
        actual: usize,
    },
    WrongColumnType {
        column: &'static str,
        expected: &'static str,
        actual: &'static str,
    },
    InvalidProjectName {
        message: String,
    },
    NameMismatch {
        requested: String,
        stored: String,
    },
    BlankLanguage {
        column: &'static str,
    },
    InvalidLineWidth {
        column: &'static str,
        actual: i64,
    },
    InvalidSourceSnapshotFingerprintLength {
        actual: usize,
    },
}

impl fmt::Display for InvalidProjectMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRow => formatter.write_str("缺少项目记录"),
            Self::MultipleRows => formatter.write_str("包含多条项目记录"),
            Self::WrongColumnCount { actual } => {
                write!(formatter, "查询结果应有 7 列，实际为 {actual} 列")
            }
            Self::WrongColumnType {
                column,
                expected,
                actual,
            } => {
                write!(formatter, "字段 {column} 应为 {expected}，实际为 {actual}")
            }
            Self::InvalidProjectName { message } => {
                write!(formatter, "项目名称无效：{message}")
            }
            Self::NameMismatch { requested, stored } => write!(
                formatter,
                "项目名称不匹配，请求 {requested:?}，数据库记录 {stored:?}"
            ),
            Self::BlankLanguage { column } => write!(formatter, "{column} 不能为空白"),
            Self::InvalidLineWidth { column, actual } => {
                write!(
                    formatter,
                    "{column} 必须是 u32 范围内的正整数，实际为 {actual}"
                )
            }
            Self::InvalidSourceSnapshotFingerprintLength { actual } => write!(
                formatter,
                "source_snapshot_fingerprint 必须是 32 字节 BLOB，实际为 {actual} 字节"
            ),
        }
    }
}

impl Error for InvalidProjectMetadata {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ActiveOwnerState {
    owner: MzStandardAssetOwner,
    source_snapshot_fingerprint: SourceSnapshotFingerprint,
}

/// 一个 active owner 相对于当前项目冻结来源的精确新鲜度。
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StandardAssetOwnerFreshness {
    owner: MzStandardAssetOwner,
    fresh: bool,
}

/// 已经按当前完整 schema 验证的项目数据库事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectDatabaseState {
    metadata: ProjectMetadataFacts,
    owners: Vec<ActiveOwnerState>,
    terminology_json: String,
    placeholder_rules_json: String,
    schema_version: i64,
}

impl ProjectDatabaseState {
    #[cfg(test)]
    pub(crate) fn for_test(
        name: ProjectName,
        source_language: String,
        target_language: String,
        source_snapshot_fingerprint: SourceSnapshotFingerprint,
        layout_profile: MzWriteBackLayoutProfile,
        owners: Vec<(MzStandardAssetOwner, SourceSnapshotFingerprint)>,
    ) -> Self {
        Self {
            metadata: ProjectMetadataFacts {
                name,
                source_language,
                target_language,
                source_snapshot_fingerprint,
                layout_profile,
            },
            owners: owners
                .into_iter()
                .map(|(owner, source_snapshot_fingerprint)| ActiveOwnerState {
                    owner,
                    source_snapshot_fingerprint,
                })
                .collect(),
            terminology_json: "[]".to_owned(),
            placeholder_rules_json: "[]".to_owned(),
            schema_version: 13,
        }
    }

    pub(crate) fn source_language(&self) -> &str {
        &self.metadata.source_language
    }

    pub(crate) fn target_language(&self) -> &str {
        &self.metadata.target_language
    }

    pub(crate) fn layout_profile(&self) -> &MzWriteBackLayoutProfile {
        &self.metadata.layout_profile
    }

    pub(crate) const fn source_snapshot_fingerprint(&self) -> SourceSnapshotFingerprint {
        self.metadata.source_snapshot_fingerprint
    }

    #[cfg(test)]
    pub(crate) fn active_owner_freshness(&self) -> Vec<StandardAssetOwnerFreshness> {
        self.owners
            .iter()
            .map(|state| StandardAssetOwnerFreshness {
                owner: state.owner,
                fresh: state.source_snapshot_fingerprint
                    == self.metadata.source_snapshot_fingerprint,
            })
            .collect()
    }

    pub(crate) fn stale_owners(&self) -> Vec<MzStandardAssetOwner> {
        let mut owners = self
            .owners
            .iter()
            .filter_map(|state| {
                (state.source_snapshot_fingerprint != self.metadata.source_snapshot_fingerprint)
                    .then_some(state.owner)
            })
            .collect::<Vec<_>>();
        owners.sort_by_key(|owner| owner_sort_key(*owner));
        owners
    }
}

fn owner_sort_key(owner: MzStandardAssetOwner) -> u8 {
    match owner {
        MzStandardAssetOwner::Builtin => 0,
        MzStandardAssetOwner::Rules => 1,
        MzStandardAssetOwner::Lua => 2,
    }
}

/// 项目数据库无法按当前唯一 schema 重建为受信事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum InvalidCurrentProjectDatabase {
    ManagedSchema { reason: String },
    SchemaChangedDuringInspection { before: i64, after: i64 },
    Metadata(InvalidProjectMetadata),
    OwnerState { reason: String },
    TranslationResources { reason: String },
    Integrity { reason: String },
}

impl fmt::Display for InvalidCurrentProjectDatabase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ManagedSchema { reason } => write!(formatter, "受管 schema 无效：{reason}"),
            Self::SchemaChangedDuringInspection { before, after } => {
                write!(formatter, "检查期间 schema 发生变化：{before} -> {after}")
            }
            Self::Metadata(reason) => write!(formatter, "metadata 无效：{reason}"),
            Self::OwnerState { reason } => write!(formatter, "owner state 无效：{reason}"),
            Self::TranslationResources { reason } => {
                write!(formatter, "翻译资源状态无效：{reason}")
            }
            Self::Integrity { reason } => write!(formatter, "数据库完整性无效：{reason}"),
        }
    }
}

impl Error for InvalidCurrentProjectDatabase {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Metadata(source) => Some(source),
            Self::ManagedSchema { .. }
            | Self::SchemaChangedDuringInspection { .. }
            | Self::OwnerState { .. }
            | Self::TranslationResources { .. }
            | Self::Integrity { .. } => None,
        }
    }
}

fn expected_managed_schema() -> Vec<(&'static str, &'static str, &'static str, &'static str)> {
    vec![
        ("table", "metadata", "metadata", CREATE_METADATA_TABLE),
        (
            "table",
            "standard_asset_owner_state",
            "standard_asset_owner_state",
            CREATE_STANDARD_ASSET_OWNER_STATE_TABLE,
        ),
        ("table", "entry", "entry", CREATE_ENTRY_TABLE),
        (
            "table",
            "system_text",
            "system_text",
            CREATE_SYSTEM_TEXT_TABLE,
        ),
        ("table", "map_text", "map_text", CREATE_MAP_TEXT_TABLE),
        ("table", "text_body", "text_body", CREATE_TEXT_BODY_TABLE),
        (
            "table",
            "plugin_param",
            "plugin_param",
            CREATE_PLUGIN_PARAM_TABLE,
        ),
        (
            "table",
            STANDARD_TRANSLATION_RESOURCE_TABLE_NAME,
            STANDARD_TRANSLATION_RESOURCE_TABLE_NAME,
            CREATE_STANDARD_TRANSLATION_RESOURCE_TABLE,
        ),
        (
            "index",
            "entry_exact_location_idx",
            "entry",
            CREATE_ENTRY_EXACT_LOCATION_INDEX,
        ),
        (
            "index",
            "system_text_exact_location_idx",
            "system_text",
            CREATE_SYSTEM_TEXT_EXACT_LOCATION_INDEX,
        ),
        (
            "index",
            "map_text_exact_location_idx",
            "map_text",
            CREATE_MAP_TEXT_EXACT_LOCATION_INDEX,
        ),
        (
            "index",
            "text_body_exact_location_idx",
            "text_body",
            CREATE_TEXT_BODY_EXACT_LOCATION_INDEX,
        ),
        (
            "index",
            "plugin_param_exact_location_idx",
            "plugin_param",
            CREATE_PLUGIN_PARAM_EXACT_LOCATION_INDEX,
        ),
    ]
}

fn validate_managed_schema(rows: Vec<SqliteRow>) -> Result<(), InvalidCurrentProjectDatabase> {
    let mut actual = Vec::with_capacity(rows.len());
    for row in rows {
        let values = row.into_values();
        if values.len() != 4 {
            return Err(InvalidCurrentProjectDatabase::ManagedSchema {
                reason: format!("sqlite_schema 查询应返回 4 列，实际为 {}", values.len()),
            });
        }
        let mut values = values.into_iter();
        let kind = schema_text(values.next().expect("已确认有四列"), "type")?;
        let name = schema_text(values.next().expect("已确认有四列"), "name")?;
        let table = schema_text(values.next().expect("已确认有四列"), "tbl_name")?;
        let sql = schema_text(values.next().expect("已确认有四列"), "sql")?;
        actual.push((kind, name, table, sql));
    }
    actual.sort();
    let mut expected = expected_managed_schema()
        .into_iter()
        .map(|(kind, name, table, sql)| {
            (
                kind.to_owned(),
                name.to_owned(),
                table.to_owned(),
                sql.to_owned(),
            )
        })
        .collect::<Vec<_>>();
    expected.sort();
    if actual == expected {
        Ok(())
    } else {
        let actual_names = actual
            .iter()
            .map(|(kind, name, _, _)| format!("{kind}:{name}"))
            .collect::<Vec<_>>()
            .join(", ");
        Err(InvalidCurrentProjectDatabase::ManagedSchema {
            reason: format!("受管对象定义或集合不匹配，实际对象为 [{actual_names}]"),
        })
    }
}

fn schema_text(
    value: SqliteValue,
    column: &'static str,
) -> Result<String, InvalidCurrentProjectDatabase> {
    match value {
        SqliteValue::Text(value) => Ok(value),
        value => Err(InvalidCurrentProjectDatabase::ManagedSchema {
            reason: format!(
                "sqlite_schema.{column} 应为 TEXT，实际为 {}",
                value.kind_name()
            ),
        }),
    }
}

fn decode_schema_version(rows: Vec<SqliteRow>) -> Result<i64, InvalidCurrentProjectDatabase> {
    let [row] = <[SqliteRow; 1]>::try_from(rows).map_err(|rows| {
        InvalidCurrentProjectDatabase::ManagedSchema {
            reason: format!("schema_version 应返回一行，实际为 {} 行", rows.len()),
        }
    })?;
    let [value] = <[SqliteValue; 1]>::try_from(row.into_values()).map_err(|values| {
        InvalidCurrentProjectDatabase::ManagedSchema {
            reason: format!("schema_version 应返回一列，实际为 {} 列", values.len()),
        }
    })?;
    match value {
        SqliteValue::Integer(value) if value >= 0 => Ok(value),
        value => Err(InvalidCurrentProjectDatabase::ManagedSchema {
            reason: format!(
                "schema_version 应为非负 INTEGER，实际为 {}",
                value.kind_name()
            ),
        }),
    }
}

fn decode_owner_states(
    rows: Vec<SqliteRow>,
) -> Result<Vec<ActiveOwnerState>, InvalidCurrentProjectDatabase> {
    let mut owners = Vec::with_capacity(rows.len());
    for row in rows {
        let values = row.into_values();
        if values.len() != 2 {
            return Err(InvalidCurrentProjectDatabase::OwnerState {
                reason: format!("owner state 应有 2 列，实际为 {}", values.len()),
            });
        }
        let mut values = values.into_iter();
        let owner = match values.next().expect("已确认有两列") {
            SqliteValue::Text(value) => MzStandardAssetOwner::from_storage_name(&value).ok_or(
                InvalidCurrentProjectDatabase::OwnerState {
                    reason: format!("未知 owner {value:?}"),
                },
            )?,
            value => {
                return Err(InvalidCurrentProjectDatabase::OwnerState {
                    reason: format!("owner 应为 TEXT，实际为 {}", value.kind_name()),
                });
            }
        };
        let fingerprint = match values.next().expect("已确认有两列") {
            SqliteValue::Blob(value) => {
                SourceSnapshotFingerprint::from_slice(&value).map_err(|source| {
                    InvalidCurrentProjectDatabase::OwnerState {
                        reason: source.to_string(),
                    }
                })?
            }
            value => {
                return Err(InvalidCurrentProjectDatabase::OwnerState {
                    reason: format!(
                        "source_snapshot_fingerprint 应为 BLOB，实际为 {}",
                        value.kind_name()
                    ),
                });
            }
        };
        if owners
            .iter()
            .any(|state: &ActiveOwnerState| state.owner == owner)
        {
            return Err(InvalidCurrentProjectDatabase::OwnerState {
                reason: format!("owner {:?} 重复", owner.storage_name()),
            });
        }
        owners.push(ActiveOwnerState {
            owner,
            source_snapshot_fingerprint: fingerprint,
        });
    }
    owners.sort_by_key(|state| owner_sort_key(state.owner));
    Ok(owners)
}

fn decode_translation_resources(
    rows: Vec<SqliteRow>,
) -> Result<(String, String), InvalidCurrentProjectDatabase> {
    let mut resources = BTreeMap::new();
    for row in rows {
        let values = row.into_values();
        if values.len() != 2 {
            return Err(InvalidCurrentProjectDatabase::TranslationResources {
                reason: format!("翻译资源应有 2 列，实际为 {}", values.len()),
            });
        }
        let mut values = values.into_iter();
        let kind = resource_text(values.next().expect("已确认有两列"), "resource_kind")?;
        let canonical_json = resource_text(values.next().expect("已确认有两列"), "canonical_json")?;
        let json: serde_json::Value = serde_json::from_str(&canonical_json).map_err(|source| {
            InvalidCurrentProjectDatabase::TranslationResources {
                reason: format!("{kind} 不是有效 JSON：{source}"),
            }
        })?;
        if !json.is_array() {
            return Err(InvalidCurrentProjectDatabase::TranslationResources {
                reason: format!("{kind} 必须是 JSON 数组"),
            });
        }
        if resources.insert(kind.clone(), canonical_json).is_some() {
            return Err(InvalidCurrentProjectDatabase::TranslationResources {
                reason: format!("资源 {kind:?} 重复"),
            });
        }
    }
    if resources.len() != 2 {
        return Err(InvalidCurrentProjectDatabase::TranslationResources {
            reason: format!("必须且只能保存两份资源，实际为 {}", resources.len()),
        });
    }
    let terminology = resources.remove(TERMINOLOGY_RESOURCE_KIND).ok_or_else(|| {
        InvalidCurrentProjectDatabase::TranslationResources {
            reason: format!("缺少 {TERMINOLOGY_RESOURCE_KIND}"),
        }
    })?;
    let placeholders = resources
        .remove(PLACEHOLDER_RULES_RESOURCE_KIND)
        .ok_or_else(|| InvalidCurrentProjectDatabase::TranslationResources {
            reason: format!("缺少 {PLACEHOLDER_RULES_RESOURCE_KIND}"),
        })?;
    Ok((terminology, placeholders))
}

fn resource_text(
    value: SqliteValue,
    column: &'static str,
) -> Result<String, InvalidCurrentProjectDatabase> {
    match value {
        SqliteValue::Text(value) => Ok(value),
        value => Err(InvalidCurrentProjectDatabase::TranslationResources {
            reason: format!("{column} 应为 TEXT，实际为 {}", value.kind_name()),
        }),
    }
}

fn validate_integrity(
    quick_check: Vec<SqliteRow>,
    foreign_key_check: Vec<SqliteRow>,
) -> Result<(), InvalidCurrentProjectDatabase> {
    if quick_check != vec![SqliteRow::new(vec![SqliteValue::Text("ok".to_owned())])] {
        return Err(InvalidCurrentProjectDatabase::Integrity {
            reason: "PRAGMA quick_check 未返回唯一 ok".to_owned(),
        });
    }
    if !foreign_key_check.is_empty() {
        return Err(InvalidCurrentProjectDatabase::Integrity {
            reason: format!(
                "PRAGMA foreign_key_check 返回 {} 条违规",
                foreign_key_check.len()
            ),
        });
    }
    Ok(())
}

/// 已由初始化用例建立并可以在内部信任的新项目事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NewProject {
    name: ProjectName,
    source_language: String,
    target_language: String,
    source_snapshot_fingerprint: SourceSnapshotFingerprint,
    layout_profile: MzWriteBackLayoutProfile,
}

impl NewProject {
    /// 汇集创建项目数据库所需的全部受信事实。
    pub(crate) fn new(
        name: ProjectName,
        source_language: String,
        target_language: String,
        source_snapshot_fingerprint: SourceSnapshotFingerprint,
        layout_profile: MzWriteBackLayoutProfile,
    ) -> Self {
        Self {
            name,
            source_language,
            target_language,
            source_snapshot_fingerprint,
            layout_profile,
        }
    }

    pub(crate) fn name(&self) -> &ProjectName {
        &self.name
    }

    pub(crate) fn source_language(&self) -> &str {
        &self.source_language
    }

    pub(crate) fn target_language(&self) -> &str {
        &self.target_language
    }

    pub(crate) const fn source_snapshot_fingerprint(&self) -> SourceSnapshotFingerprint {
        self.source_snapshot_fingerprint
    }

    pub(crate) fn layout_profile(&self) -> &MzWriteBackLayoutProfile {
        &self.layout_profile
    }
}

/// 对现存项目数据库完成一次严格检查或状态收敛后的结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectDatabaseReconciliation {
    state: ProjectDatabaseState,
}

impl ProjectDatabaseReconciliation {
    #[cfg(test)]
    pub(crate) fn for_test(state: ProjectDatabaseState) -> Self {
        Self { state }
    }

    #[cfg(test)]
    pub(crate) fn state(&self) -> &ProjectDatabaseState {
        &self.state
    }

    pub(crate) fn stale_owners(&self) -> Vec<MzStandardAssetOwner> {
        self.state.stale_owners()
    }
}

/// 严格检查现存项目数据库失败。
#[derive(Debug)]
pub(crate) enum ProjectDatabaseInspectionError<E> {
    DatabaseNotFound {
        path: PathBuf,
    },
    ReadDatabase {
        path: PathBuf,
        stage: &'static str,
        source: E,
    },
    InvalidDatabase {
        path: PathBuf,
        reason: InvalidCurrentProjectDatabase,
    },
}

impl<E: fmt::Display> fmt::Display for ProjectDatabaseInspectionError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DatabaseNotFound { path } => {
                write!(formatter, "项目数据库不存在：{}", path.display())
            }
            Self::ReadDatabase {
                path,
                stage,
                source,
            } => write!(
                formatter,
                "检查项目数据库 {} 的{stage}失败：{source}",
                path.display()
            ),
            Self::InvalidDatabase { path, reason } => {
                write!(formatter, "项目数据库 {} 无效：{reason}", path.display())
            }
        }
    }
}

impl<E: Error + 'static> Error for ProjectDatabaseInspectionError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadDatabase { source, .. } => Some(source),
            Self::InvalidDatabase { reason, .. } => Some(reason),
            Self::DatabaseNotFound { .. } => None,
        }
    }
}

/// 对现存项目数据库执行 CAS 收敛失败。
#[derive(Debug)]
pub(crate) enum ProjectDatabaseReconciliationError<R, W> {
    Inspection(ProjectDatabaseInspectionError<R>),
    ConcurrentModification { path: PathBuf },
    DatabaseNotFound { path: PathBuf },
    NotCommitted { path: PathBuf, source: W },
    OutcomeUnknown { path: PathBuf, source: W },
}

impl<R: fmt::Display, W: fmt::Display> fmt::Display for ProjectDatabaseReconciliationError<R, W> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inspection(source) => source.fmt(formatter),
            Self::ConcurrentModification { path } => {
                write!(
                    formatter,
                    "项目数据库 {} 在对账期间被外部改变",
                    path.display()
                )
            }
            Self::DatabaseNotFound { path } => {
                write!(formatter, "项目数据库在对账前消失：{}", path.display())
            }
            Self::NotCommitted { path, source } => {
                write!(
                    formatter,
                    "项目数据库 {} 未完成对账：{source}",
                    path.display()
                )
            }
            Self::OutcomeUnknown { path, source } => write!(
                formatter,
                "项目数据库 {} 的对账结果未知：{source}",
                path.display()
            ),
        }
    }
}

impl<R, W> Error for ProjectDatabaseReconciliationError<R, W>
where
    R: Error + 'static,
    W: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Inspection(source) => Some(source),
            Self::NotCommitted { source, .. } | Self::OutcomeUnknown { source, .. } => Some(source),
            Self::ConcurrentModification { .. } | Self::DatabaseNotFound { .. } => None,
        }
    }
}

/// Init 所依赖的路径级项目数据库检查与收敛职责。
pub(crate) trait ProjectDatabaseStateReconciler: Send + Sync {
    type InspectionError: Error + Send + Sync + 'static;
    type ReconciliationError: Error + Send + Sync + 'static;

    fn inspect(
        &self,
        database_path: PathBuf,
        expected_name: ProjectName,
    ) -> impl Future<Output = Result<ProjectDatabaseState, Self::InspectionError>> + Send;

    fn reconcile(
        &self,
        database_path: PathBuf,
        requested: NewProject,
    ) -> impl Future<Output = Result<ProjectDatabaseReconciliation, Self::ReconciliationError>> + Send;
}

/// 使用查询根与短事务根实现当前项目数据库的严格状态收敛。
pub(crate) struct ProjectDatabaseStateReconciliationService<Q, T> {
    queries: Q,
    transactions: T,
}

impl<Q, T> ProjectDatabaseStateReconciliationService<Q, T> {
    pub(crate) fn new(queries: Q, transactions: T) -> Self {
        Self {
            queries,
            transactions,
        }
    }
}

impl<Q, T> ProjectDatabaseStateReconciler for ProjectDatabaseStateReconciliationService<Q, T>
where
    Q: SqliteQueryExecutor,
    T: SqliteTransactionExecutor,
{
    type InspectionError = ProjectDatabaseInspectionError<Q::Error>;
    type ReconciliationError = ProjectDatabaseReconciliationError<Q::Error, T::Error>;

    async fn inspect(
        &self,
        database_path: PathBuf,
        expected_name: ProjectName,
    ) -> Result<ProjectDatabaseState, Self::InspectionError> {
        inspect_project_database(&self.queries, database_path, &expected_name).await
    }

    async fn reconcile(
        &self,
        database_path: PathBuf,
        requested: NewProject,
    ) -> Result<ProjectDatabaseReconciliation, Self::ReconciliationError> {
        let current =
            inspect_project_database(&self.queries, database_path.clone(), requested.name())
                .await
                .map_err(ProjectDatabaseReconciliationError::Inspection)?;

        reconcile_project_database::<Q::Error, _>(
            &self.transactions,
            database_path,
            current,
            requested,
        )
        .await
    }
}

async fn read_inspection_rows<Q>(
    queries: &Q,
    database_path: &Path,
    stage: &'static str,
    query: SqliteQuery,
) -> Result<Vec<SqliteRow>, ProjectDatabaseInspectionError<Q::Error>>
where
    Q: SqliteQueryExecutor,
{
    queries
        .query_existing_database(database_path.to_path_buf(), query)
        .await
        .map_err(|error| match error {
            QueryExistingDatabaseError::NotFound => {
                ProjectDatabaseInspectionError::DatabaseNotFound {
                    path: database_path.to_path_buf(),
                }
            }
            QueryExistingDatabaseError::QueryFailed(source) => {
                ProjectDatabaseInspectionError::ReadDatabase {
                    path: database_path.to_path_buf(),
                    stage,
                    source,
                }
            }
        })
}

async fn inspect_project_database<Q>(
    queries: &Q,
    database_path: PathBuf,
    expected_name: &ProjectName,
) -> Result<ProjectDatabaseState, ProjectDatabaseInspectionError<Q::Error>>
where
    Q: SqliteQueryExecutor,
{
    let schema_version_before = decode_schema_version(
        read_inspection_rows(
            queries,
            &database_path,
            "读取初始 schema version",
            SqliteQuery::new(SELECT_SCHEMA_VERSION, Vec::new()),
        )
        .await?,
    )
    .map_err(|reason| ProjectDatabaseInspectionError::InvalidDatabase {
        path: database_path.clone(),
        reason,
    })?;

    let schema = read_inspection_rows(
        queries,
        &database_path,
        "读取受管 schema",
        SqliteQuery::new(SELECT_MANAGED_SCHEMA, Vec::new()),
    )
    .await?;
    validate_managed_schema(schema).map_err(|reason| {
        ProjectDatabaseInspectionError::InvalidDatabase {
            path: database_path.clone(),
            reason,
        }
    })?;

    let metadata = metadata_facts_from_rows(
        expected_name,
        read_inspection_rows(
            queries,
            &database_path,
            "读取 metadata",
            SqliteQuery::new(SELECT_METADATA, Vec::new()),
        )
        .await?,
    )
    .map_err(|reason| ProjectDatabaseInspectionError::InvalidDatabase {
        path: database_path.clone(),
        reason: InvalidCurrentProjectDatabase::Metadata(reason),
    })?;
    let owners = decode_owner_states(
        read_inspection_rows(
            queries,
            &database_path,
            "读取 owner state",
            SqliteQuery::new(SELECT_OWNER_STATES, Vec::new()),
        )
        .await?,
    )
    .map_err(|reason| ProjectDatabaseInspectionError::InvalidDatabase {
        path: database_path.clone(),
        reason,
    })?;
    let (terminology_json, placeholder_rules_json) = decode_translation_resources(
        read_inspection_rows(
            queries,
            &database_path,
            "读取翻译资源",
            SqliteQuery::new(SELECT_TRANSLATION_RESOURCES, Vec::new()),
        )
        .await?,
    )
    .map_err(|reason| ProjectDatabaseInspectionError::InvalidDatabase {
        path: database_path.clone(),
        reason,
    })?;

    let quick_check = read_inspection_rows(
        queries,
        &database_path,
        "执行 quick_check",
        SqliteQuery::new(SELECT_QUICK_CHECK, Vec::new()),
    )
    .await?;
    let foreign_key_check = read_inspection_rows(
        queries,
        &database_path,
        "执行 foreign_key_check",
        SqliteQuery::new(SELECT_FOREIGN_KEY_CHECK, Vec::new()),
    )
    .await?;
    validate_integrity(quick_check, foreign_key_check).map_err(|reason| {
        ProjectDatabaseInspectionError::InvalidDatabase {
            path: database_path.clone(),
            reason,
        }
    })?;

    let schema_version_after = decode_schema_version(
        read_inspection_rows(
            queries,
            &database_path,
            "复核 schema version",
            SqliteQuery::new(SELECT_SCHEMA_VERSION, Vec::new()),
        )
        .await?,
    )
    .map_err(|reason| ProjectDatabaseInspectionError::InvalidDatabase {
        path: database_path.clone(),
        reason,
    })?;
    if schema_version_before != schema_version_after {
        return Err(ProjectDatabaseInspectionError::InvalidDatabase {
            path: database_path,
            reason: InvalidCurrentProjectDatabase::SchemaChangedDuringInspection {
                before: schema_version_before,
                after: schema_version_after,
            },
        });
    }

    Ok(ProjectDatabaseState {
        metadata,
        owners,
        terminology_json,
        placeholder_rules_json,
        schema_version: schema_version_after,
    })
}

fn schema_version_cas(state: &ProjectDatabaseState) -> SqliteTransactionStep {
    SqliteTransactionStep::RequireNoRows(SqliteQuery::new(
        "SELECT 1 WHERE NOT EXISTS (SELECT 1 FROM pragma_schema_version WHERE schema_version = ?1)",
        vec![SqliteValue::Integer(state.schema_version)],
    ))
}

fn metadata_cas(state: &ProjectDatabaseState) -> SqliteTransactionStep {
    SqliteTransactionStep::RequireNoRows(SqliteQuery::new(
        r#"SELECT 1
WHERE (SELECT COUNT(*) FROM metadata) <> 1
   OR NOT EXISTS (
     SELECT 1 FROM metadata
     WHERE name = ?1
       AND source_language = ?2
       AND target_language = ?3
       AND source_snapshot_fingerprint = ?4
       AND dialogue_max_fullwidth_chars = ?5
       AND scrolling_text_max_fullwidth_chars = ?6
       AND help_description_max_fullwidth_chars = ?7
   )"#,
        vec![
            SqliteValue::Text(state.metadata.name.as_str().to_owned()),
            SqliteValue::Text(state.metadata.source_language.clone()),
            SqliteValue::Text(state.metadata.target_language.clone()),
            SqliteValue::Blob(
                state
                    .metadata
                    .source_snapshot_fingerprint
                    .as_bytes()
                    .to_vec(),
            ),
            SqliteValue::Integer(i64::from(
                state.metadata.layout_profile.dialogue_body().get(),
            )),
            SqliteValue::Integer(i64::from(
                state.metadata.layout_profile.scrolling_text().get(),
            )),
            SqliteValue::Integer(i64::from(
                state.metadata.layout_profile.help_description().get(),
            )),
        ],
    ))
}

fn owner_state_cas(state: &ProjectDatabaseState) -> SqliteTransactionStep {
    let mut statement =
        "SELECT 1 WHERE (SELECT COUNT(*) FROM standard_asset_owner_state) <> ?1".to_owned();
    let mut parameters = vec![SqliteValue::Integer(
        i64::try_from(state.owners.len()).expect("owner 数量固定小于 i64 上限"),
    )];
    if !state.owners.is_empty() {
        statement.push_str(" OR EXISTS (SELECT 1 FROM standard_asset_owner_state WHERE NOT (");
        for (index, owner) in state.owners.iter().enumerate() {
            if index != 0 {
                statement.push_str(" OR ");
            }
            let owner_parameter = parameters.len() + 1;
            let fingerprint_parameter = owner_parameter + 1;
            statement.push_str(&format!(
                "(owner = ?{owner_parameter} AND source_snapshot_fingerprint = ?{fingerprint_parameter})"
            ));
            parameters.push(SqliteValue::Text(owner.owner.storage_name().to_owned()));
            parameters.push(SqliteValue::Blob(
                owner.source_snapshot_fingerprint.as_bytes().to_vec(),
            ));
        }
        statement.push_str("))");
    }
    SqliteTransactionStep::RequireNoRows(SqliteQuery::new(statement, parameters))
}

fn translation_resources_cas(state: &ProjectDatabaseState) -> SqliteTransactionStep {
    SqliteTransactionStep::RequireNoRows(SqliteQuery::new(
        r#"SELECT 1
WHERE (SELECT COUNT(*) FROM standard_translation_resource) <> 2
   OR NOT EXISTS (
     SELECT 1 FROM standard_translation_resource
     WHERE resource_kind = 'terminology' AND canonical_json = ?1
   )
   OR NOT EXISTS (
     SELECT 1 FROM standard_translation_resource
     WHERE resource_kind = 'placeholder_rules' AND canonical_json = ?2
   )"#,
        vec![
            SqliteValue::Text(state.terminology_json.clone()),
            SqliteValue::Text(state.placeholder_rules_json.clone()),
        ],
    ))
}

async fn reconcile_project_database<R, T>(
    transactions: &T,
    database_path: PathBuf,
    current: ProjectDatabaseState,
    requested: NewProject,
) -> Result<ProjectDatabaseReconciliation, ProjectDatabaseReconciliationError<R, T::Error>>
where
    R: Error + Send + Sync + 'static,
    T: SqliteTransactionExecutor,
{
    let language_changed = current.metadata.source_language != requested.source_language
        || current.metadata.target_language != requested.target_language;
    let changed = language_changed
        || current.metadata.source_snapshot_fingerprint != requested.source_snapshot_fingerprint
        || current.metadata.layout_profile != requested.layout_profile;
    if !changed {
        return Ok(ProjectDatabaseReconciliation { state: current });
    }

    let mut steps = vec![
        schema_version_cas(&current),
        metadata_cas(&current),
        owner_state_cas(&current),
        translation_resources_cas(&current),
    ];
    if language_changed {
        for statement in [
            CLEAR_ENTRY_TRANSLATIONS,
            CLEAR_SYSTEM_TEXT_TRANSLATIONS,
            CLEAR_MAP_TEXT_TRANSLATIONS,
            CLEAR_TEXT_BODY_TRANSLATIONS,
            CLEAR_PLUGIN_PARAM_TRANSLATIONS,
            RESET_TERMINOLOGY_RESOURCE,
        ] {
            steps.push(SqliteTransactionStep::Execute(SqliteCommand::new(
                statement,
                Vec::new(),
            )));
        }
    }
    steps.push(SqliteTransactionStep::Execute(SqliteCommand::new(
        UPDATE_METADATA,
        vec![
            SqliteValue::Text(requested.source_language.clone()),
            SqliteValue::Text(requested.target_language.clone()),
            SqliteValue::Blob(requested.source_snapshot_fingerprint.as_bytes().to_vec()),
            SqliteValue::Integer(i64::from(requested.layout_profile.dialogue_body().get())),
            SqliteValue::Integer(i64::from(requested.layout_profile.scrolling_text().get())),
            SqliteValue::Integer(i64::from(requested.layout_profile.help_description().get())),
            SqliteValue::Text(requested.name.as_str().to_owned()),
        ],
    )));

    transactions
        .execute_transaction(database_path.clone(), SqliteTransactionPlan::new(steps))
        .await
        .map_err(|error| match error {
            ExecuteTransactionError::NotFound => {
                ProjectDatabaseReconciliationError::DatabaseNotFound {
                    path: database_path.clone(),
                }
            }
            ExecuteTransactionError::RequirementFailed => {
                ProjectDatabaseReconciliationError::ConcurrentModification {
                    path: database_path.clone(),
                }
            }
            ExecuteTransactionError::NotCommitted(source) => {
                ProjectDatabaseReconciliationError::NotCommitted {
                    path: database_path.clone(),
                    source,
                }
            }
            ExecuteTransactionError::OutcomeUnknown(source) => {
                ProjectDatabaseReconciliationError::OutcomeUnknown {
                    path: database_path.clone(),
                    source,
                }
            }
        })?;

    let state = ProjectDatabaseState {
        metadata: ProjectMetadataFacts {
            name: requested.name,
            source_language: requested.source_language,
            target_language: requested.target_language,
            source_snapshot_fingerprint: requested.source_snapshot_fingerprint,
            layout_profile: requested.layout_profile,
        },
        owners: current.owners,
        terminology_json: if language_changed {
            "[]".to_owned()
        } else {
            current.terminology_json
        },
        placeholder_rules_json: current.placeholder_rules_json,
        schema_version: current.schema_version,
    };
    Ok(ProjectDatabaseReconciliation { state })
}

/// 创建项目数据库的职责契约。
pub(crate) trait ProjectDatabaseCreator: Send + Sync {
    /// 数据库创建失败。
    type Error: Error + Send + Sync + 'static;

    /// 创建并初始化一个全新的项目数据库。
    ///
    /// `destination_path` 是工作区创建器已经选择的精确暂存路径；本职责不得再按
    /// 项目名推导或改写它。
    ///
    /// 一旦返回的 Future 开始产生副作用，调用方必须持续等待到明确终态；本契约
    /// 不承诺 Future 被丢弃、中止或进程终止后的清理结果。
    fn create(
        &self,
        destination_path: PathBuf,
        project: NewProject,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 使用 SQLite 驱动创建项目数据库。
pub(crate) struct ProjectDatabaseCreationService<S> {
    sqlite: S,
}

impl<S> ProjectDatabaseCreationService<S> {
    /// 创建服务。
    ///
    /// 目标数据库路径由调用方逐次提供，本服务不创建其父目录。
    pub(crate) fn new(sqlite: S) -> Self {
        Self { sqlite }
    }
}

impl<S> ProjectDatabaseCreator for ProjectDatabaseCreationService<S>
where
    S: SqliteDatabaseCreator,
{
    type Error = ProjectDatabaseCreateError<S::Error>;

    async fn create(&self, database_path: PathBuf, project: NewProject) -> Result<(), Self::Error> {
        let commands = project_database_commands(&project);

        self.sqlite
            .create_new_database(database_path.clone(), commands)
            .await
            .map_err(|error| {
                ProjectDatabaseCreateError::from_driver(database_path.clone(), error)
            })?;

        Ok(())
    }
}

fn project_database_commands(project: &NewProject) -> Vec<SqliteCommand> {
    let mut commands = [
        CREATE_METADATA_TABLE,
        CREATE_STANDARD_ASSET_OWNER_STATE_TABLE,
        CREATE_ENTRY_TABLE,
        CREATE_SYSTEM_TEXT_TABLE,
        CREATE_MAP_TEXT_TABLE,
        CREATE_TEXT_BODY_TABLE,
        CREATE_PLUGIN_PARAM_TABLE,
        CREATE_ENTRY_EXACT_LOCATION_INDEX,
        CREATE_SYSTEM_TEXT_EXACT_LOCATION_INDEX,
        CREATE_MAP_TEXT_EXACT_LOCATION_INDEX,
        CREATE_TEXT_BODY_EXACT_LOCATION_INDEX,
        CREATE_PLUGIN_PARAM_EXACT_LOCATION_INDEX,
        CREATE_STANDARD_TRANSLATION_RESOURCE_TABLE,
    ]
    .into_iter()
    .map(|statement| SqliteCommand::new(statement, Vec::new()))
    .collect::<Vec<_>>();
    commands.push(SqliteCommand::new(
        INSERT_METADATA,
        vec![
            SqliteValue::Text(project.name().as_str().to_owned()),
            SqliteValue::Text(project.source_language().to_owned()),
            SqliteValue::Text(project.target_language().to_owned()),
            SqliteValue::Blob(project.source_snapshot_fingerprint().as_bytes().to_vec()),
            SqliteValue::Integer(i64::from(project.layout_profile().dialogue_body().get())),
            SqliteValue::Integer(i64::from(project.layout_profile().scrolling_text().get())),
            SqliteValue::Integer(i64::from(project.layout_profile().help_description().get())),
        ],
    ));
    for resource_kind in [TERMINOLOGY_RESOURCE_KIND, PLACEHOLDER_RULES_RESOURCE_KIND] {
        commands.push(SqliteCommand::new(
            INSERT_STANDARD_TRANSLATION_RESOURCE,
            vec![
                SqliteValue::Text(resource_kind.to_owned()),
                SqliteValue::Text("[]".to_owned()),
            ],
        ));
    }
    commands
}

/// 项目数据库创建失败，并保留目标路径和底层原因。
#[derive(Debug)]
pub(crate) enum ProjectDatabaseCreateError<E> {
    /// 目标路径已经存在，未覆盖旧库。
    AlreadyExists { path: PathBuf },
    /// 确认没有创建出数据库产物。
    NotCreated { path: PathBuf, source: E },
    /// 无法确认初始化事务是否生效。
    OutcomeUnknown { path: PathBuf, source: E },
    /// 创建未完成，并且存在未清除的残留文件。
    ResidualArtifact { path: PathBuf, source: E },
}

impl<E> ProjectDatabaseCreateError<E> {
    fn from_driver(path: PathBuf, error: CreateDatabaseError<E>) -> Self {
        match error {
            CreateDatabaseError::AlreadyExists => Self::AlreadyExists { path },
            CreateDatabaseError::NotCreated(source) => Self::NotCreated { path, source },
            CreateDatabaseError::OutcomeUnknown(source) => Self::OutcomeUnknown { path, source },
            CreateDatabaseError::ResidualArtifact(source) => {
                Self::ResidualArtifact { path, source }
            }
        }
    }

    /// 返回此次创建所针对的数据库路径。
    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::AlreadyExists { path }
            | Self::NotCreated { path, .. }
            | Self::OutcomeUnknown { path, .. }
            | Self::ResidualArtifact { path, .. } => path,
        }
    }
}

impl<E> fmt::Display for ProjectDatabaseCreateError<E>
where
    E: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyExists { path } => {
                write!(formatter, "项目数据库已经存在：{}", path.display())
            }
            Self::NotCreated { path, source } => {
                write!(formatter, "项目数据库未创建 {}：{source}", path.display())
            }
            Self::OutcomeUnknown { path, source } => write!(
                formatter,
                "项目数据库创建结果未知 {}：{source}",
                path.display()
            ),
            Self::ResidualArtifact { path, source } => write!(
                formatter,
                "项目数据库创建失败且存在残留文件 {}：{source}",
                path.display()
            ),
        }
    }
}

impl<E> Error for ProjectDatabaseCreateError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::AlreadyExists { .. } => None,
            Self::NotCreated { source, .. }
            | Self::OutcomeUnknown { source, .. }
            | Self::ResidualArtifact { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::error::Error;
    use std::fmt;
    use std::future::Future;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    use super::*;

    #[test]
    fn workspace_layout_derives_every_fixed_location_from_one_root() {
        let name: ProjectName = "测试 游戏".parse().expect("test name should be valid");
        let layout = ProjectWorkspaceLayout::for_project(Path::new("C:/att/projects"), &name);

        assert_eq!(
            layout.workspace_root(),
            Path::new("C:/att/projects/测试 游戏")
        );
        assert_eq!(
            layout.database_path(),
            Path::new("C:/att/projects/测试 游戏/project.db")
        );
        assert_eq!(
            layout.source_root(),
            Path::new("C:/att/projects/测试 游戏/source")
        );
        assert_eq!(
            layout.source_data(),
            Path::new("C:/att/projects/测试 游戏/source/data")
        );
        assert_eq!(
            layout.source_js(),
            Path::new("C:/att/projects/测试 游戏/source/js")
        );
        assert_eq!(
            layout.write_back_root(),
            Path::new("C:/att/projects/测试 游戏/write_back")
        );
    }

    #[derive(Debug)]
    struct FakeDriverError(&'static str);

    impl fmt::Display for FakeDriverError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeDriverError {}

    #[derive(Debug)]
    struct Invocation {
        path: PathBuf,
        commands: Vec<SqliteCommand>,
    }

    struct RecordingDriver {
        invocations: Mutex<Vec<Invocation>>,
        responses: Mutex<VecDeque<Result<(), CreateDatabaseError<FakeDriverError>>>>,
    }

    impl RecordingDriver {
        fn succeeding() -> Self {
            Self::responding_with(Ok(()))
        }

        fn responding_with(response: Result<(), CreateDatabaseError<FakeDriverError>>) -> Self {
            Self {
                invocations: Mutex::new(Vec::new()),
                responses: Mutex::new(VecDeque::from([response])),
            }
        }
    }

    impl SqliteDatabaseCreator for RecordingDriver {
        type Error = FakeDriverError;

        fn create_new_database(
            &self,
            path: PathBuf,
            commands: Vec<SqliteCommand>,
        ) -> impl Future<Output = Result<(), CreateDatabaseError<Self::Error>>> + Send {
            self.invocations
                .lock()
                .expect("invocations mutex should not be poisoned")
                .push(Invocation { path, commands });
            let response = self
                .responses
                .lock()
                .expect("responses mutex should not be poisoned")
                .pop_front()
                .expect("test must provide one driver response per invocation");

            async move { response }
        }
    }

    fn project(name: &str) -> NewProject {
        NewProject::new(
            name.parse().expect("test project name should be valid"),
            "ja".to_owned(),
            "zh-CN".to_owned(),
            test_source_snapshot_fingerprint(),
            layout_profile(),
        )
    }

    fn test_source_snapshot_fingerprint() -> SourceSnapshotFingerprint {
        SourceSnapshotFingerprint::from_bytes([0x5a; 32])
    }

    fn width(value: u32) -> MaxFullwidthChars {
        MaxFullwidthChars::new(value).expect("test width should be positive")
    }

    fn layout_profile() -> MzWriteBackLayoutProfile {
        MzWriteBackLayoutProfile::new(width(24), width(30), width(18))
    }

    #[tokio::test]
    async fn creates_expected_database_and_parameterized_metadata_transaction() {
        let service = ProjectDatabaseCreationService::new(RecordingDriver::succeeding());

        service
            .create(
                PathBuf::from("C:/projects/测试 游戏/project.db"),
                project("测试 游戏"),
            )
            .await
            .expect("database creation should succeed");

        let invocations = service
            .sqlite
            .invocations
            .lock()
            .expect("invocations mutex should not be poisoned");
        assert_eq!(invocations.len(), 1);
        let invocation = &invocations[0];
        assert_eq!(
            invocation.path,
            PathBuf::from("C:/projects/测试 游戏/project.db")
        );
        assert_eq!(invocation.commands.len(), 16);
        assert_eq!(invocation.commands[0].statement(), CREATE_METADATA_TABLE);
        assert!(invocation.commands[0].parameters().is_empty());
        assert_eq!(
            invocation.commands[1].statement(),
            CREATE_STANDARD_ASSET_OWNER_STATE_TABLE
        );
        assert_eq!(invocation.commands[2].statement(), CREATE_ENTRY_TABLE);
        assert_eq!(invocation.commands[3].statement(), CREATE_SYSTEM_TEXT_TABLE);
        assert_eq!(invocation.commands[4].statement(), CREATE_MAP_TEXT_TABLE);
        assert_eq!(invocation.commands[5].statement(), CREATE_TEXT_BODY_TABLE);
        assert_eq!(
            invocation.commands[6].statement(),
            CREATE_PLUGIN_PARAM_TABLE
        );
        assert_eq!(invocation.commands[13].statement(), INSERT_METADATA);
        assert_eq!(
            invocation.commands[13].parameters(),
            &[
                SqliteValue::Text("测试 游戏".to_owned()),
                SqliteValue::Text("ja".to_owned()),
                SqliteValue::Text("zh-CN".to_owned()),
                SqliteValue::Blob(vec![0x5a; 32]),
                SqliteValue::Integer(24),
                SqliteValue::Integer(30),
                SqliteValue::Integer(18),
            ]
        );
        assert_eq!(
            invocation.commands[14].parameters(),
            &[
                SqliteValue::Text(TERMINOLOGY_RESOURCE_KIND.to_owned()),
                SqliteValue::Text("[]".to_owned()),
            ]
        );
        assert_eq!(
            invocation.commands[15].parameters(),
            &[
                SqliteValue::Text(PLACEHOLDER_RULES_RESOURCE_KIND.to_owned()),
                SqliteValue::Text("[]".to_owned()),
            ]
        );
        for table in &invocation.commands[2..7] {
            assert!(
                table
                    .statement()
                    .contains("PRIMARY KEY (owner, exact_location)")
            );
            assert!(table.statement().contains("translation_state"));
            assert!(table.statement().contains("ON DELETE CASCADE"));
        }
        assert!(
            invocation.commands[1]
                .statement()
                .contains("'builtin', 'rules', 'lua'")
        );
    }

    #[test]
    fn complete_schema_enforces_owner_identity_and_translation_state_pairing() {
        let connection = rusqlite::Connection::open_in_memory().expect("内存数据库应可打开");
        connection
            .pragma_update(None, "foreign_keys", true)
            .expect("测试必须启用外键约束");
        for statement in [
            CREATE_METADATA_TABLE,
            CREATE_STANDARD_ASSET_OWNER_STATE_TABLE,
            CREATE_ENTRY_TABLE,
            CREATE_SYSTEM_TEXT_TABLE,
            CREATE_MAP_TEXT_TABLE,
            CREATE_TEXT_BODY_TABLE,
            CREATE_PLUGIN_PARAM_TABLE,
            CREATE_ENTRY_EXACT_LOCATION_INDEX,
            CREATE_SYSTEM_TEXT_EXACT_LOCATION_INDEX,
            CREATE_MAP_TEXT_EXACT_LOCATION_INDEX,
            CREATE_TEXT_BODY_EXACT_LOCATION_INDEX,
            CREATE_PLUGIN_PARAM_EXACT_LOCATION_INDEX,
            CREATE_STANDARD_TRANSLATION_RESOURCE_TABLE,
        ] {
            connection
                .execute_batch(statement)
                .unwrap_or_else(|error| panic!("当前 schema 应可执行：{error}"));
        }

        let read_managed_schema = |connection: &rusqlite::Connection| {
            let mut statement = connection
                .prepare(SELECT_MANAGED_SCHEMA)
                .expect("应可准备受管 schema 查询");
            statement
                .query_map([], |row| {
                    Ok(SqliteRow::new(vec![
                        SqliteValue::Text(row.get(0)?),
                        SqliteValue::Text(row.get(1)?),
                        SqliteValue::Text(row.get(2)?),
                        SqliteValue::Text(row.get(3)?),
                    ]))
                })
                .expect("应可查询受管 schema")
                .collect::<Result<Vec<_>, _>>()
                .expect("schema 行应可读取")
        };
        validate_managed_schema(read_managed_schema(&connection))
            .expect("建库 DDL 必须与当前唯一 schema 精确一致");
        connection
            .execute_batch("CREATE TABLE lua_custom_state (value TEXT)")
            .expect("Lua 自建表应可存在");
        validate_managed_schema(read_managed_schema(&connection))
            .expect("Lua 自建表不得污染受管 schema 检查");
        connection
            .execute_batch(
                "CREATE TRIGGER injected_entry_trigger AFTER INSERT ON entry BEGIN SELECT 1; END",
            )
            .expect("测试触发器应可创建");
        assert!(matches!(
            validate_managed_schema(read_managed_schema(&connection)),
            Err(InvalidCurrentProjectDatabase::ManagedSchema { .. })
        ));

        let insert_asset = "INSERT INTO entry (owner, exact_location, group_location, field_name, original_text, translation, translation_state) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)";
        connection
            .execute(
                insert_asset,
                rusqlite::params![
                    "builtin",
                    "exact-a",
                    "group-a",
                    "name",
                    "original",
                    Option::<String>::None,
                    Option::<Vec<u8>>::None,
                ],
            )
            .expect_err("没有 owner state 的资产必须被外键拒绝");

        for owner in ["builtin", "rules", "lua"] {
            connection
                .execute(
                    "INSERT INTO standard_asset_owner_state (owner, source_snapshot_fingerprint) VALUES (?1, ?2)",
                    rusqlite::params![owner, vec![0x5a_u8; 32]],
                )
                .expect("三个当前 owner 编码都应合法");
        }

        connection
            .execute(
                insert_asset,
                rusqlite::params![
                    "builtin",
                    "exact-a",
                    "group-a",
                    "name",
                    "original",
                    Option::<String>::None,
                    Option::<Vec<u8>>::None,
                ],
            )
            .expect("未翻译资产应可保存");
        connection
            .execute(
                insert_asset,
                rusqlite::params![
                    "rules",
                    "exact-a",
                    "group-a",
                    "name",
                    "original",
                    "译文",
                    vec![0x33_u8; 32],
                ],
            )
            .expect("不同 owner 可以持有同一 exact location");
        connection
            .execute(
                insert_asset,
                rusqlite::params![
                    "lua",
                    "exact-b",
                    "group-b",
                    "name",
                    "original",
                    "译文",
                    Option::<Vec<u8>>::None,
                ],
            )
            .expect_err("译文与逐叶状态必须成对保存");
        connection
            .execute(
                insert_asset,
                rusqlite::params![
                    "builtin",
                    "exact-a",
                    "group-a",
                    "name",
                    "original",
                    Option::<String>::None,
                    Option::<Vec<u8>>::None,
                ],
            )
            .expect_err("同一 owner 不能重复保存同一 exact location");
    }

    #[tokio::test]
    async fn maps_all_driver_terminal_states_with_target_context() {
        let cases = [
            (
                CreateDatabaseError::AlreadyExists,
                ExpectedKind::AlreadyExists,
                None,
            ),
            (
                CreateDatabaseError::NotCreated(FakeDriverError("not-created")),
                ExpectedKind::NotCreated,
                Some("not-created"),
            ),
            (
                CreateDatabaseError::OutcomeUnknown(FakeDriverError("unknown")),
                ExpectedKind::OutcomeUnknown,
                Some("unknown"),
            ),
            (
                CreateDatabaseError::ResidualArtifact(FakeDriverError("residual")),
                ExpectedKind::ResidualArtifact,
                Some("residual"),
            ),
        ];

        for (driver_error, expected_kind, expected_source) in cases {
            let service = ProjectDatabaseCreationService::new(RecordingDriver::responding_with(
                Err(driver_error),
            ));

            let error = service
                .create(
                    PathBuf::from("C:/projects/demo/project.db"),
                    project("demo"),
                )
                .await
                .expect_err("driver failure should be preserved");

            assert_eq!(error.path(), Path::new("C:/projects/demo/project.db"));
            assert_eq!(ExpectedKind::from(&error), expected_kind);
            assert_eq!(
                error.source().map(ToString::to_string).as_deref(),
                expected_source
            );
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ExpectedKind {
        AlreadyExists,
        NotCreated,
        OutcomeUnknown,
        ResidualArtifact,
    }

    impl<E> From<&ProjectDatabaseCreateError<E>> for ExpectedKind {
        fn from(error: &ProjectDatabaseCreateError<E>) -> Self {
            match error {
                ProjectDatabaseCreateError::AlreadyExists { .. } => Self::AlreadyExists,
                ProjectDatabaseCreateError::NotCreated { .. } => Self::NotCreated,
                ProjectDatabaseCreateError::OutcomeUnknown { .. } => Self::OutcomeUnknown,
                ProjectDatabaseCreateError::ResidualArtifact { .. } => Self::ResidualArtifact,
            }
        }
    }

    struct ConcurrentDriver {
        entered: AtomicUsize,
        max_entered: AtomicUsize,
    }

    impl ConcurrentDriver {
        fn new() -> Self {
            Self {
                entered: AtomicUsize::new(0),
                max_entered: AtomicUsize::new(0),
            }
        }
    }

    impl SqliteDatabaseCreator for ConcurrentDriver {
        type Error = FakeDriverError;

        async fn create_new_database(
            &self,
            _path: PathBuf,
            _commands: Vec<SqliteCommand>,
        ) -> Result<(), CreateDatabaseError<Self::Error>> {
            let entered = self.entered.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_entered.fetch_max(entered, Ordering::SeqCst);
            YieldOnce::new().await;
            self.entered.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct YieldOnce(bool);

    impl YieldOnce {
        fn new() -> Self {
            Self(false)
        }
    }

    impl Future for YieldOnce {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                context.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    #[tokio::test]
    async fn does_not_serialize_creation_of_different_projects() {
        let service = ProjectDatabaseCreationService::new(ConcurrentDriver::new());

        let (first, second) = tokio::join!(
            service.create(
                PathBuf::from("C:/projects/first/project.db"),
                project("first")
            ),
            service.create(
                PathBuf::from("C:/projects/second/project.db"),
                project("second")
            )
        );

        first.expect("first database should be created");
        second.expect("second database should be created");
        assert_eq!(service.sqlite.max_entered.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn creation_future_is_send() {
        let service = ProjectDatabaseCreationService::new(RecordingDriver::succeeding());

        assert_send(service.create(
            PathBuf::from("C:/projects/demo/project.db"),
            project("demo"),
        ));
    }

    #[derive(Debug)]
    struct QueryInvocation {
        path: PathBuf,
        query: SqliteQuery,
    }

    struct RecordingQueryExecutor {
        invocations: Mutex<Vec<QueryInvocation>>,
        responses:
            Mutex<VecDeque<Result<Vec<SqliteRow>, QueryExistingDatabaseError<FakeDriverError>>>>,
    }

    impl RecordingQueryExecutor {
        fn responding_with(
            response: Result<Vec<SqliteRow>, QueryExistingDatabaseError<FakeDriverError>>,
        ) -> Self {
            Self::responding_with_many(vec![response])
        }

        fn responding_with_many(
            responses: Vec<Result<Vec<SqliteRow>, QueryExistingDatabaseError<FakeDriverError>>>,
        ) -> Self {
            Self {
                invocations: Mutex::new(Vec::new()),
                responses: Mutex::new(VecDeque::from(responses)),
            }
        }
    }

    impl SqliteQueryExecutor for RecordingQueryExecutor {
        type Error = FakeDriverError;

        fn query_existing_database(
            &self,
            path: PathBuf,
            query: SqliteQuery,
        ) -> impl Future<Output = Result<Vec<SqliteRow>, QueryExistingDatabaseError<Self::Error>>> + Send
        {
            self.invocations
                .lock()
                .expect("query invocations mutex should not be poisoned")
                .push(QueryInvocation { path, query });
            let response = self
                .responses
                .lock()
                .expect("query responses mutex should not be poisoned")
                .pop_front()
                .expect("test must provide one response per query");

            async move { response }
        }
    }

    struct RecordingTransactionExecutor {
        plans: Mutex<Vec<(PathBuf, SqliteTransactionPlan)>>,
        responses: Mutex<VecDeque<Result<(), ExecuteTransactionError<FakeDriverError>>>>,
    }

    impl RecordingTransactionExecutor {
        fn responding_with(response: Result<(), ExecuteTransactionError<FakeDriverError>>) -> Self {
            Self {
                plans: Mutex::new(Vec::new()),
                responses: Mutex::new(VecDeque::from([response])),
            }
        }
    }

    impl SqliteTransactionExecutor for RecordingTransactionExecutor {
        type Error = FakeDriverError;

        fn execute_transaction(
            &self,
            path: PathBuf,
            plan: SqliteTransactionPlan,
        ) -> impl Future<Output = Result<(), ExecuteTransactionError<Self::Error>>> + Send {
            self.plans
                .lock()
                .expect("transaction plans mutex should not be poisoned")
                .push((path, plan));
            let response = self
                .responses
                .lock()
                .expect("transaction responses mutex should not be poisoned")
                .pop_front()
                .expect("test must provide one transaction response per invocation");
            async move { response }
        }
    }

    fn valid_managed_schema_rows() -> Vec<SqliteRow> {
        expected_managed_schema()
            .into_iter()
            .map(|(kind, name, table, sql)| {
                SqliteRow::new(vec![
                    SqliteValue::Text(kind.to_owned()),
                    SqliteValue::Text(name.to_owned()),
                    SqliteValue::Text(table.to_owned()),
                    SqliteValue::Text(sql.to_owned()),
                ])
            })
            .collect()
    }

    fn valid_inspection_responses()
    -> Vec<Result<Vec<SqliteRow>, QueryExistingDatabaseError<FakeDriverError>>> {
        vec![
            Ok(vec![SqliteRow::new(vec![SqliteValue::Integer(13)])]),
            Ok(valid_managed_schema_rows()),
            Ok(vec![valid_metadata_row()]),
            Ok(vec![
                SqliteRow::new(vec![
                    SqliteValue::Text("builtin".to_owned()),
                    SqliteValue::Blob(vec![0x5a; 32]),
                ]),
                SqliteRow::new(vec![
                    SqliteValue::Text("lua".to_owned()),
                    SqliteValue::Blob(vec![0x6b; 32]),
                ]),
            ]),
            Ok(vec![
                SqliteRow::new(vec![
                    SqliteValue::Text("placeholder_rules".to_owned()),
                    SqliteValue::Text("[]".to_owned()),
                ]),
                SqliteRow::new(vec![
                    SqliteValue::Text("terminology".to_owned()),
                    SqliteValue::Text("[]".to_owned()),
                ]),
            ]),
            Ok(vec![SqliteRow::new(vec![SqliteValue::Text(
                "ok".to_owned(),
            )])]),
            Ok(Vec::new()),
            Ok(vec![SqliteRow::new(vec![SqliteValue::Integer(13)])]),
        ]
    }

    fn metadata_row(
        name: SqliteValue,
        source_language: SqliteValue,
        target_language: SqliteValue,
        dialogue_width: SqliteValue,
        scrolling_width: SqliteValue,
        help_width: SqliteValue,
    ) -> SqliteRow {
        SqliteRow::new(vec![
            name,
            source_language,
            target_language,
            SqliteValue::Blob(vec![0x5a; 32]),
            dialogue_width,
            scrolling_width,
            help_width,
        ])
    }

    fn valid_metadata_row() -> SqliteRow {
        metadata_row(
            SqliteValue::Text("测试 游戏".to_owned()),
            SqliteValue::Text("ja".to_owned()),
            SqliteValue::Text("zh-Hans".to_owned()),
            SqliteValue::Integer(24),
            SqliteValue::Integer(30),
            SqliteValue::Integer(18),
        )
    }

    fn metadata_row_with_fingerprint(fingerprint: SqliteValue) -> SqliteRow {
        let mut values = valid_metadata_row().into_values();
        values[3] = fingerprint;
        SqliteRow::new(values)
    }

    fn record_reading_service(
        response: Result<Vec<SqliteRow>, QueryExistingDatabaseError<FakeDriverError>>,
    ) -> ProjectDatabaseRecordReadingService<RecordingQueryExecutor> {
        ProjectDatabaseRecordReadingService::new(
            PathBuf::from("C:/att/projects"),
            RecordingQueryExecutor::responding_with(response),
        )
    }

    #[tokio::test]
    async fn strict_inspection_returns_active_owner_freshness_in_domain_order() {
        let queries = RecordingQueryExecutor::responding_with_many(valid_inspection_responses());
        let transactions = RecordingTransactionExecutor::responding_with(Ok(()));
        let service = ProjectDatabaseStateReconciliationService::new(queries, transactions);
        let expected_name: ProjectName = "测试 游戏".parse().expect("项目名应合法");

        let state = service
            .inspect(PathBuf::from("C:/projects/demo/project.db"), expected_name)
            .await
            .expect("当前 schema 与项目事实应通过严格检查");

        assert_eq!(state.source_language(), "ja");
        assert_eq!(state.target_language(), "zh-Hans");
        assert_eq!(
            state.active_owner_freshness(),
            vec![
                StandardAssetOwnerFreshness {
                    owner: MzStandardAssetOwner::Builtin,
                    fresh: true,
                },
                StandardAssetOwnerFreshness {
                    owner: MzStandardAssetOwner::Lua,
                    fresh: false,
                },
            ]
        );
        assert_eq!(state.stale_owners(), vec![MzStandardAssetOwner::Lua]);
        let invocations = service
            .queries
            .invocations
            .lock()
            .expect("query invocations mutex should not be poisoned");
        assert_eq!(invocations.len(), 8);
        assert_eq!(invocations[0].query.statement(), SELECT_SCHEMA_VERSION);
        assert_eq!(invocations[7].query.statement(), SELECT_SCHEMA_VERSION);
    }

    #[tokio::test]
    async fn reconciliation_clears_language_dependent_state_and_uses_cas_guards() {
        let queries = RecordingQueryExecutor::responding_with_many(valid_inspection_responses());
        let transactions = RecordingTransactionExecutor::responding_with(Ok(()));
        let service = ProjectDatabaseStateReconciliationService::new(queries, transactions);
        let requested = NewProject::new(
            "测试 游戏".parse().expect("项目名应合法"),
            "en".to_owned(),
            "zh-Hans".to_owned(),
            SourceSnapshotFingerprint::from_bytes([0x7c; 32]),
            MzWriteBackLayoutProfile::new(width(26), width(32), width(20)),
        );

        let result = service
            .reconcile(PathBuf::from("C:/projects/demo/project.db"), requested)
            .await
            .expect("对账事务应提交");

        assert_eq!(
            result.stale_owners(),
            vec![MzStandardAssetOwner::Builtin, MzStandardAssetOwner::Lua]
        );
        assert_eq!(result.state().source_language(), "en");
        assert_eq!(
            result.state().source_snapshot_fingerprint(),
            SourceSnapshotFingerprint::from_bytes([0x7c; 32])
        );
        let plans = service
            .transactions
            .plans
            .lock()
            .expect("transaction plans mutex should not be poisoned");
        assert_eq!(plans.len(), 1);
        let steps = plans[0].1.steps();
        let check_count = steps
            .iter()
            .filter(|step| matches!(step, SqliteTransactionStep::RequireNoRows(_)))
            .count();
        assert_eq!(check_count, 4);
        let executed = steps
            .iter()
            .filter_map(|step| match step {
                SqliteTransactionStep::Execute(command) => Some(command.statement()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(executed.contains(&CLEAR_ENTRY_TRANSLATIONS));
        assert!(executed.contains(&CLEAR_PLUGIN_PARAM_TRANSLATIONS));
        assert!(executed.contains(&RESET_TERMINOLOGY_RESOURCE));
        assert_eq!(executed.last().copied(), Some(UPDATE_METADATA));
    }

    #[tokio::test]
    async fn reconciliation_maps_failed_cas_without_executing_a_second_plan() {
        let queries = RecordingQueryExecutor::responding_with_many(valid_inspection_responses());
        let transactions = RecordingTransactionExecutor::responding_with(Err(
            ExecuteTransactionError::RequirementFailed,
        ));
        let service = ProjectDatabaseStateReconciliationService::new(queries, transactions);
        let requested = NewProject::new(
            "测试 游戏".parse().expect("项目名应合法"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            SourceSnapshotFingerprint::from_bytes([0x7c; 32]),
            layout_profile(),
        );

        let error = service
            .reconcile(PathBuf::from("C:/projects/demo/project.db"), requested)
            .await
            .expect_err("CAS 失败必须作为外部并发改变返回");

        assert!(matches!(
            error,
            ProjectDatabaseReconciliationError::ConcurrentModification { .. }
        ));
        assert_eq!(
            service
                .transactions
                .plans
                .lock()
                .expect("transaction plans mutex should not be poisoned")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn reads_exact_metadata_projection_into_trusted_record() {
        let service = record_reading_service(Ok(vec![valid_metadata_row()]));
        let requested: ProjectName = "测试 游戏".parse().expect("test name should be valid");

        let record = service
            .read(&requested)
            .await
            .expect("valid metadata should be read");

        assert_eq!(record.name(), &requested);
        assert_eq!(
            record.workspace_root(),
            Path::new("C:/att/projects/测试 游戏")
        );
        assert_eq!(
            record.database_path(),
            Path::new("C:/att/projects/测试 游戏/project.db")
        );
        assert_eq!(
            record.layout().source_data(),
            Path::new("C:/att/projects/测试 游戏/source/data")
        );
        assert_eq!(
            record.layout().source_js(),
            Path::new("C:/att/projects/测试 游戏/source/js")
        );
        assert_eq!(record.source_language(), "ja");
        assert_eq!(record.target_language(), "zh-Hans");
        assert_eq!(
            record.source_snapshot_fingerprint(),
            test_source_snapshot_fingerprint()
        );
        assert_eq!(record.layout_profile(), &layout_profile());

        let invocations = service
            .sqlite
            .invocations
            .lock()
            .expect("query invocations mutex should not be poisoned");
        assert_eq!(invocations.len(), 1);
        assert_eq!(
            invocations[0].path,
            PathBuf::from("C:/att/projects/测试 游戏/project.db")
        );
        assert_eq!(invocations[0].query.statement(), SELECT_METADATA);
        assert!(invocations[0].query.parameters().is_empty());
    }

    #[tokio::test]
    async fn maps_query_terminal_states_with_database_path() {
        let not_found = record_reading_service(Err(QueryExistingDatabaseError::NotFound))
            .read(&"demo".parse().expect("test name should be valid"))
            .await
            .expect_err("missing database should fail");
        assert!(matches!(
            not_found,
            ProjectDatabaseReadError::DatabaseNotFound { .. }
        ));
        assert_eq!(
            not_found.path(),
            Path::new("C:/att/projects/demo/project.db")
        );
        assert!(not_found.source().is_none());

        let read_failure = record_reading_service(Err(QueryExistingDatabaseError::QueryFailed(
            FakeDriverError("query failed"),
        )))
        .read(&"demo".parse().expect("test name should be valid"))
        .await
        .expect_err("query failure should be preserved");
        assert_eq!(
            read_failure.path(),
            Path::new("C:/att/projects/demo/project.db")
        );
        assert!(matches!(
            read_failure,
            ProjectDatabaseReadError::ReadDatabase { .. }
        ));
        assert_eq!(
            read_failure.source().map(ToString::to_string).as_deref(),
            Some("query failed")
        );
    }

    #[tokio::test]
    async fn rejects_metadata_rows_that_do_not_match_the_current_contract() {
        let service = record_reading_service(Err(QueryExistingDatabaseError::QueryFailed(
            FakeDriverError("no such column: dialogue_max_fullwidth_chars"),
        )));

        let error = service
            .read(&"demo".parse().expect("test name should be valid"))
            .await
            .expect_err("不符合当前 metadata 契约的记录必须被拒绝");

        assert!(matches!(
            error,
            ProjectDatabaseReadError::ReadDatabase { .. }
        ));
        let invocations = service
            .sqlite
            .invocations
            .lock()
            .expect("query invocations mutex should not be poisoned");
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].query.statement(), SELECT_METADATA);
    }

    #[tokio::test]
    async fn rejects_invalid_metadata_shape_and_storage_types() {
        let cases = [
            (Vec::new(), ExpectedInvalidMetadata::MissingRow),
            (
                vec![valid_metadata_row(), valid_metadata_row()],
                ExpectedInvalidMetadata::MultipleRows,
            ),
            (
                vec![SqliteRow::new(vec![SqliteValue::Text(
                    "测试 游戏".to_owned(),
                )])],
                ExpectedInvalidMetadata::WrongColumnCount,
            ),
            (
                vec![metadata_row(
                    SqliteValue::Text("测试 游戏".to_owned()),
                    SqliteValue::Blob(Vec::new()),
                    SqliteValue::Text("zh-Hans".to_owned()),
                    SqliteValue::Integer(24),
                    SqliteValue::Integer(30),
                    SqliteValue::Integer(18),
                )],
                ExpectedInvalidMetadata::WrongColumnType,
            ),
            (
                vec![metadata_row(
                    SqliteValue::Text("测试 游戏".to_owned()),
                    SqliteValue::Text("ja".to_owned()),
                    SqliteValue::Text("zh-Hans".to_owned()),
                    SqliteValue::Text("24".to_owned()),
                    SqliteValue::Integer(30),
                    SqliteValue::Integer(18),
                )],
                ExpectedInvalidMetadata::WrongColumnType,
            ),
            (
                vec![metadata_row_with_fingerprint(SqliteValue::Text(
                    "not-a-fingerprint".to_owned(),
                ))],
                ExpectedInvalidMetadata::WrongColumnType,
            ),
            (
                vec![metadata_row_with_fingerprint(SqliteValue::Blob(vec![
                    0x5a;
                    31
                ]))],
                ExpectedInvalidMetadata::InvalidSourceSnapshotFingerprintLength,
            ),
        ];

        for (rows, expected) in cases {
            let error = record_reading_service(Ok(rows))
                .read(&"测试 游戏".parse().expect("test name should be valid"))
                .await
                .expect_err("invalid metadata should fail");

            let ProjectDatabaseReadError::InvalidMetadata { reason, .. } = error else {
                panic!("expected invalid metadata error")
            };
            assert_eq!(ExpectedInvalidMetadata::from(&reason), expected);
        }
    }

    #[tokio::test]
    async fn rejects_metadata_that_cannot_reestablish_trusted_project_facts() {
        let cases = [
            (
                metadata_row(
                    SqliteValue::Text("../unsafe".to_owned()),
                    SqliteValue::Text("ja".to_owned()),
                    SqliteValue::Text("zh".to_owned()),
                    SqliteValue::Integer(24),
                    SqliteValue::Integer(30),
                    SqliteValue::Integer(18),
                ),
                ExpectedInvalidMetadata::InvalidProjectName,
            ),
            (
                metadata_row(
                    SqliteValue::Text("another".to_owned()),
                    SqliteValue::Text("ja".to_owned()),
                    SqliteValue::Text("zh".to_owned()),
                    SqliteValue::Integer(24),
                    SqliteValue::Integer(30),
                    SqliteValue::Integer(18),
                ),
                ExpectedInvalidMetadata::NameMismatch,
            ),
            (
                metadata_row(
                    SqliteValue::Text("demo".to_owned()),
                    SqliteValue::Text(" \t".to_owned()),
                    SqliteValue::Text("zh".to_owned()),
                    SqliteValue::Integer(24),
                    SqliteValue::Integer(30),
                    SqliteValue::Integer(18),
                ),
                ExpectedInvalidMetadata::BlankLanguage,
            ),
            (
                metadata_row(
                    SqliteValue::Text("demo".to_owned()),
                    SqliteValue::Text("ja".to_owned()),
                    SqliteValue::Text("\n".to_owned()),
                    SqliteValue::Integer(24),
                    SqliteValue::Integer(30),
                    SqliteValue::Integer(18),
                ),
                ExpectedInvalidMetadata::BlankLanguage,
            ),
            (
                metadata_row(
                    SqliteValue::Text("demo".to_owned()),
                    SqliteValue::Text("ja".to_owned()),
                    SqliteValue::Text("zh".to_owned()),
                    SqliteValue::Integer(0),
                    SqliteValue::Integer(30),
                    SqliteValue::Integer(18),
                ),
                ExpectedInvalidMetadata::InvalidLineWidth,
            ),
            (
                metadata_row(
                    SqliteValue::Text("demo".to_owned()),
                    SqliteValue::Text("ja".to_owned()),
                    SqliteValue::Text("zh".to_owned()),
                    SqliteValue::Integer(24),
                    SqliteValue::Integer(-1),
                    SqliteValue::Integer(18),
                ),
                ExpectedInvalidMetadata::InvalidLineWidth,
            ),
            (
                metadata_row(
                    SqliteValue::Text("demo".to_owned()),
                    SqliteValue::Text("ja".to_owned()),
                    SqliteValue::Text("zh".to_owned()),
                    SqliteValue::Integer(24),
                    SqliteValue::Integer(30),
                    SqliteValue::Integer(i64::from(u32::MAX) + 1),
                ),
                ExpectedInvalidMetadata::InvalidLineWidth,
            ),
        ];

        for (row, expected) in cases {
            let error = record_reading_service(Ok(vec![row]))
                .read(&"demo".parse().expect("test name should be valid"))
                .await
                .expect_err("untrusted metadata should fail");
            let ProjectDatabaseReadError::InvalidMetadata { reason, .. } = error else {
                panic!("expected invalid metadata error")
            };
            assert_eq!(ExpectedInvalidMetadata::from(&reason), expected);
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum ExpectedInvalidMetadata {
        MissingRow,
        MultipleRows,
        WrongColumnCount,
        WrongColumnType,
        InvalidProjectName,
        NameMismatch,
        BlankLanguage,
        InvalidLineWidth,
        InvalidSourceSnapshotFingerprintLength,
    }

    impl From<&InvalidProjectMetadata> for ExpectedInvalidMetadata {
        fn from(reason: &InvalidProjectMetadata) -> Self {
            match reason {
                InvalidProjectMetadata::MissingRow => Self::MissingRow,
                InvalidProjectMetadata::MultipleRows => Self::MultipleRows,
                InvalidProjectMetadata::WrongColumnCount { .. } => Self::WrongColumnCount,
                InvalidProjectMetadata::WrongColumnType { .. } => Self::WrongColumnType,
                InvalidProjectMetadata::InvalidProjectName { .. } => Self::InvalidProjectName,
                InvalidProjectMetadata::NameMismatch { .. } => Self::NameMismatch,
                InvalidProjectMetadata::BlankLanguage { .. } => Self::BlankLanguage,
                InvalidProjectMetadata::InvalidLineWidth { .. } => Self::InvalidLineWidth,
                InvalidProjectMetadata::InvalidSourceSnapshotFingerprintLength { .. } => {
                    Self::InvalidSourceSnapshotFingerprintLength
                }
            }
        }
    }

    #[test]
    fn maximum_fullwidth_chars_rejects_zero() {
        let error = MaxFullwidthChars::new(0).expect_err("zero width should be rejected");

        assert_eq!(error.to_string(), "每行最大全角字符数必须大于零");
        assert_eq!(width(1).get(), 1);
    }

    #[test]
    fn record_reading_future_is_send() {
        let service = record_reading_service(Ok(vec![valid_metadata_row()]));
        let name: ProjectName = "测试 游戏".parse().expect("test name should be valid");

        assert_send(service.read(&name));
    }

    fn assert_send(_: impl Send) {}
}
