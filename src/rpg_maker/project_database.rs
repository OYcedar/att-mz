//! RPG Maker 项目数据库的创建、读取与状态收敛职责。

mod run_plan;

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};

use crate::fingerprint::{InvalidSha256FingerprintLength, Sha256Fingerprint};
use crate::language::{LanguageId, LanguageIdError, LanguagePair};
use crate::rpg_maker::ProjectName;
use crate::rpg_maker::RpgMakerLayout;
use crate::rpg_maker::dialogue::{MvDialogueDefinition, MvDialogueDefinitionError};
use crate::rpg_maker::standard_asset::RpgMakerStandardAssetOwner;
use crate::storage::sqlite::{
    CreateDatabaseError, ExecuteTransactionError, QueryExistingDatabaseError, SqliteCommand,
    SqliteDatabaseCreator, SqliteQuery, SqliteQueryExecutor, SqliteRow, SqliteTransactionExecutor,
    SqliteTransactionPlan, SqliteTransactionStep, SqliteValue,
};

use run_plan::{
    CREATE_EXTRACT_RULES_DEFINITION_TABLE, CREATE_EXTRACT_RUN_PLAN_TABLE,
    CREATE_INIT_RUN_PLAN_TABLE, CREATE_LUA_PROGRAM_TABLE, CREATE_TRANSLATE_RUN_PLAN_TABLE,
    CREATE_WRITE_BACK_RUN_PLAN_TABLE, SELECT_LUA_PROGRAMS, SELECT_RUN_PLAN_SINGLETONS,
    decode_project_run_plans,
};
#[allow(
    unused_imports,
    reason = "运行方案组合根 API 在同次纵向重构完成接线后由 application 消费"
)]
pub(crate) use run_plan::{
    ExtractRulesCanonicalJson, ExtractRunPlan, FinalProjectRunPlanPersistenceService, InitRunPlan,
    InvalidProjectRunPlans, InvalidRunPlanValue, LuaProgramPhase, LuaProgramSnapshot,
    ProjectRunPlanFinalizer, ProjectRunPlanPersistenceService, ProjectRunPlanReadError,
    ProjectRunPlanReplaceError, ProjectRunPlanReplacement, ProjectRunPlanRepository,
    ProjectRunPlans, TranslateRunPlan, WriteBackRunPlan,
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
    ),
    asset_snapshot_fingerprint BLOB NOT NULL CHECK (
        typeof(asset_snapshot_fingerprint) = 'blob'
        AND length(asset_snapshot_fingerprint) = 32
    )
)"#;

pub(crate) const STANDARD_TEXT_GROUP_TABLE_NAME: &str = "standard_text_group";
pub(crate) const STANDARD_TEXT_UNIT_TABLE_NAME: &str = "standard_text_unit";
pub(crate) const STANDARD_MUTATION_CLAIM_TABLE_NAME: &str = "standard_mutation_claim";

const CREATE_STANDARD_TEXT_GROUP_TABLE: &str = r#"CREATE TABLE standard_text_group (
    owner                  TEXT NOT NULL CHECK (owner IN ('builtin', 'rules', 'lua')),
    group_location         TEXT NOT NULL CHECK (length(group_location) > 0),
    group_order            INTEGER NOT NULL CHECK (group_order >= 0),
    group_kind             TEXT NOT NULL CHECK (group_kind IN (
        'database_entry',
        'system',
        'map',
        'event_dialogue',
        'event_choices',
        'event_scrolling_text',
        'event_command',
        'plugin_parameter'
    )),
    projection_recipe_json TEXT NOT NULL CHECK (length(projection_recipe_json) > 0),
    PRIMARY KEY (owner, group_location),
    UNIQUE (owner, group_order),
    FOREIGN KEY (owner) REFERENCES standard_asset_owner_state(owner) ON DELETE CASCADE
)"#;

const CREATE_STANDARD_TEXT_UNIT_TABLE: &str = r#"CREATE TABLE standard_text_unit (
    owner                    TEXT NOT NULL CHECK (owner IN ('builtin', 'rules', 'lua')),
    group_location           TEXT NOT NULL CHECK (length(group_location) > 0),
    unit_role                TEXT NOT NULL CHECK (length(unit_role) > 0),
    unit_order               INTEGER NOT NULL CHECK (unit_order >= 0),
    source_content_json      TEXT NOT NULL CHECK (
        json_valid(source_content_json)
        AND json_type(source_content_json) IN ('text', 'array')
    ),
    source_context_json      TEXT NOT NULL CHECK (
        json_valid(source_context_json)
        AND json_type(source_context_json) = 'object'
    ),
    translation_content_json TEXT,
    translation_state        BLOB,
    PRIMARY KEY (owner, group_location, unit_role),
    UNIQUE (owner, group_location, unit_order),
    FOREIGN KEY (owner, group_location)
        REFERENCES standard_text_group(owner, group_location) ON DELETE CASCADE,
    CHECK (
        (translation_content_json IS NULL AND translation_state IS NULL)
        OR (
            translation_content_json IS NOT NULL
            AND json_valid(translation_content_json)
            AND json_type(translation_content_json) = json_type(source_content_json)
            AND typeof(translation_state) = 'blob'
            AND length(translation_state) = 32
        )
    )
)"#;

const CREATE_STANDARD_MUTATION_CLAIM_TABLE: &str = r#"CREATE TABLE standard_mutation_claim (
    owner          TEXT NOT NULL CHECK (owner IN ('builtin', 'rules', 'lua')),
    group_location TEXT NOT NULL CHECK (length(group_location) > 0),
    resource_key   TEXT NOT NULL CHECK (length(resource_key) > 0),
    access         TEXT NOT NULL CHECK (access IN ('intent', 'exclusive')),
    PRIMARY KEY (owner, group_location, resource_key),
    FOREIGN KEY (owner, group_location)
        REFERENCES standard_text_group(owner, group_location) ON DELETE CASCADE
)"#;

pub(crate) const CREATE_STANDARD_MUTATION_CLAIM_RESOURCE_INDEX: &str = "CREATE INDEX standard_mutation_claim_resource_idx ON standard_mutation_claim(resource_key, access, owner, group_location)";
pub(crate) const CREATE_STANDARD_MUTATION_CLAIM_OWNER_RESOURCE_INDEX: &str = "CREATE INDEX standard_mutation_claim_owner_resource_idx ON standard_mutation_claim(owner, resource_key, access, group_location)";
pub(crate) const DROP_STANDARD_MUTATION_CLAIM_RESOURCE_INDEX: &str =
    "DROP INDEX standard_mutation_claim_resource_idx";
pub(crate) const DROP_STANDARD_MUTATION_CLAIM_OWNER_RESOURCE_INDEX: &str =
    "DROP INDEX standard_mutation_claim_owner_resource_idx";

pub(crate) const STANDARD_TRANSLATION_RESOURCE_TABLE_NAME: &str = "standard_translation_resource";
pub(crate) const TERMINOLOGY_RESOURCE_KIND: &str = "terminology";
pub(crate) const PLACEHOLDER_RULES_RESOURCE_KIND: &str = "placeholder_rules";

const CREATE_STANDARD_TRANSLATION_RESOURCE_TABLE: &str = r#"CREATE TABLE standard_translation_resource (
    resource_kind  TEXT NOT NULL PRIMARY KEY CHECK (
        resource_kind IN ('terminology', 'placeholder_rules')
    ),
    canonical_json TEXT NOT NULL CHECK (length(canonical_json) > 0)
)"#;

pub(crate) const STANDARD_PROJECT_DEFINITION_TABLE_NAME: &str = "standard_project_definition";
pub(crate) const MV_DIALOGUE_RULES_DEFINITION_KIND: &str = "mv_dialogue_rules";

const CREATE_STANDARD_PROJECT_DEFINITION_TABLE: &str = r#"CREATE TABLE standard_project_definition (
    definition_kind TEXT NOT NULL PRIMARY KEY CHECK (definition_kind IN ('mv_dialogue_rules')),
    canonical_json  TEXT NOT NULL CHECK (length(canonical_json) > 0)
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
const INSERT_STANDARD_PROJECT_DEFINITION: &str = r#"INSERT INTO standard_project_definition (
    definition_kind,
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
const SELECT_PROJECT_RECORD: &str = r#"SELECT
    metadata.name,
    metadata.source_language,
    metadata.target_language,
    metadata.source_snapshot_fingerprint,
    metadata.dialogue_max_fullwidth_chars,
    metadata.scrolling_text_max_fullwidth_chars,
    metadata.help_description_max_fullwidth_chars,
    definition.canonical_json
FROM metadata
JOIN standard_project_definition AS definition
  ON definition.definition_kind = 'mv_dialogue_rules'"#;

const SELECT_SCHEMA_VERSION: &str = "SELECT schema_version FROM pragma_schema_version";
const SELECT_MANAGED_SCHEMA: &str = r#"SELECT type, name, tbl_name, sql
FROM sqlite_schema
WHERE sql IS NOT NULL
  AND (
    tbl_name IN (
      'metadata',
      'init_run_plan',
      'extract_run_plan',
      'extract_rules_definition',
      'translate_run_plan',
      'write_back_run_plan',
      'lua_program',
      'standard_asset_owner_state',
      'standard_text_group',
      'standard_text_unit',
      'standard_mutation_claim',
      'standard_translation_resource',
      'standard_project_definition'
    )
    OR name IN (
      'standard_mutation_claim_resource_idx',
      'standard_mutation_claim_owner_resource_idx'
    )
  )
ORDER BY type, name"#;
const SELECT_OWNER_STATES: &str = r#"SELECT owner, source_snapshot_fingerprint, asset_snapshot_fingerprint
FROM standard_asset_owner_state
ORDER BY owner"#;
const SELECT_TRANSLATION_RESOURCES: &str = r#"SELECT resource_kind, canonical_json
FROM standard_translation_resource
ORDER BY resource_kind"#;
const SELECT_PROJECT_DEFINITIONS: &str = r#"SELECT definition_kind, canonical_json
FROM standard_project_definition
ORDER BY definition_kind"#;
const SELECT_QUICK_CHECK: &str = "PRAGMA quick_check";
const SELECT_FOREIGN_KEY_CHECK: &str = "PRAGMA foreign_key_check";

const PROJECT_RECORD_QUERY_ID: &str = "project_database.project_record";
const PROJECT_RECORD_READ_STAGE: &str = "read_project_record";

const UPDATE_METADATA: &str = r#"UPDATE metadata
SET source_language = ?1,
    target_language = ?2,
    source_snapshot_fingerprint = ?3,
    dialogue_max_fullwidth_chars = ?4,
    scrolling_text_max_fullwidth_chars = ?5,
    help_description_max_fullwidth_chars = ?6
WHERE name = ?7"#;
const CLEAR_STANDARD_TEXT_TRANSLATIONS: &str = "UPDATE standard_text_unit SET translation_content_json = NULL, translation_state = NULL WHERE translation_content_json IS NOT NULL OR translation_state IS NOT NULL";
const RESET_TERMINOLOGY_RESOURCE: &str = r#"UPDATE standard_translation_resource
SET canonical_json = '[]'
WHERE resource_kind = 'terminology'"#;

/// 冻结布局所选 `data` 与 `js` 内容的精确身份。
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

/// 一个 owner 当前语义单元、自然顺序、物化配方与物理修改声明集合的精确身份。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct AssetSnapshotFingerprint(Sha256Fingerprint);

impl AssetSnapshotFingerprint {
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

/// 一个 RPG Maker 项目工作区中所有固定位置的唯一派生结果。
///
/// 工作区创建、数据库读取、项目开启与写回都从该值取得路径，避免各自重新解释
/// 工作区结构。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectWorkspaceLayout {
    rpg_maker_layout: RpgMakerLayout,
    workspace_root: PathBuf,
    database_path: PathBuf,
    source_root: PathBuf,
    source_data: PathBuf,
    source_js: PathBuf,
    write_back_root: PathBuf,
    write_back_data: PathBuf,
    write_back_js: PathBuf,
}

impl ProjectWorkspaceLayout {
    /// 从项目集合根和受信项目名定位工作区。
    pub(crate) fn for_project(
        projects_root: &Path,
        rpg_maker_layout: RpgMakerLayout,
        name: &ProjectName,
    ) -> Self {
        Self::from_workspace_root(
            projects_root
                .join(rpg_maker_layout.engine().storage_name())
                .join(name.as_str()),
            rpg_maker_layout,
        )
    }

    /// 从已经确定的工作区根建立全部固定位置。
    pub(crate) fn from_workspace_root(
        workspace_root: PathBuf,
        rpg_maker_layout: RpgMakerLayout,
    ) -> Self {
        let database_path = workspace_root.join(PROJECT_DATABASE_FILE_NAME);
        let source_root = workspace_root.join("source");
        let source_data = workspace_root.join(rpg_maker_layout.source_data_relative());
        let source_js = workspace_root.join(rpg_maker_layout.source_js_relative());
        let write_back_root = workspace_root.join("write_back");
        let write_back_data = workspace_root.join(rpg_maker_layout.write_back_data_relative());
        let write_back_js = workspace_root.join(rpg_maker_layout.write_back_js_relative());

        Self {
            rpg_maker_layout,
            workspace_root,
            database_path,
            source_root,
            source_data,
            source_js,
            write_back_root,
            write_back_data,
            write_back_js,
        }
    }

    pub(crate) fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub(crate) const fn rpg_maker_layout(&self) -> RpgMakerLayout {
        self.rpg_maker_layout
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

    #[cfg(test)]
    pub(crate) fn write_back_data(&self) -> &Path {
        &self.write_back_data
    }

    #[cfg(test)]
    pub(crate) fn write_back_js(&self) -> &Path {
        &self.write_back_js
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

/// RPG Maker 标准写回所使用的三个显示区域宽度。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RpgMakerWriteBackLayoutProfile {
    dialogue_body: MaxFullwidthChars,
    scrolling_text: MaxFullwidthChars,
    help_description: MaxFullwidthChars,
}

impl RpgMakerWriteBackLayoutProfile {
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
    language_pair: LanguagePair,
    source_snapshot_fingerprint: SourceSnapshotFingerprint,
    layout_profile: RpgMakerWriteBackLayoutProfile,
    mv_dialogue_definition: MvDialogueDefinition,
}

impl StoredProjectRecord {
    /// 建立一条已经由项目数据库读取器确认可信的记录。
    #[cfg(test)]
    pub(crate) fn new(
        name: ProjectName,
        workspace_root: PathBuf,
        database_path: PathBuf,
        rpg_maker_layout: RpgMakerLayout,
        language_pair: LanguagePair,
        layout_profile: RpgMakerWriteBackLayoutProfile,
    ) -> Self {
        let layout = ProjectWorkspaceLayout::from_workspace_root(workspace_root, rpg_maker_layout);
        assert_eq!(
            layout.database_path(),
            database_path,
            "受信项目记录的数据库路径必须属于同一工作区布局"
        );
        Self::from_layout(
            name,
            layout,
            language_pair,
            SourceSnapshotFingerprint::from_bytes([0xa5; 32]),
            layout_profile,
            MvDialogueDefinition::empty(),
        )
    }

    /// 直接复用已经建立的工作区布局。
    pub(crate) fn from_layout(
        name: ProjectName,
        layout: ProjectWorkspaceLayout,
        language_pair: LanguagePair,
        source_snapshot_fingerprint: SourceSnapshotFingerprint,
        layout_profile: RpgMakerWriteBackLayoutProfile,
        mv_dialogue_definition: MvDialogueDefinition,
    ) -> Self {
        Self {
            name,
            layout,
            language_pair,
            source_snapshot_fingerprint,
            layout_profile,
            mv_dialogue_definition,
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

    pub(crate) fn language_pair(&self) -> &LanguagePair {
        &self.language_pair
    }

    pub(crate) fn source_language(&self) -> &LanguageId {
        self.language_pair.source()
    }

    pub(crate) fn target_language(&self) -> &LanguageId {
        self.language_pair.target()
    }

    pub(crate) const fn source_snapshot_fingerprint(&self) -> SourceSnapshotFingerprint {
        self.source_snapshot_fingerprint
    }

    pub(crate) fn layout_profile(&self) -> &RpgMakerWriteBackLayoutProfile {
        &self.layout_profile
    }

    pub(crate) fn mv_dialogue_definition(&self) -> &MvDialogueDefinition {
        &self.mv_dialogue_definition
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
    rpg_maker_layout: RpgMakerLayout,
    sqlite: S,
}

impl<S> ProjectDatabaseRecordReadingService<S> {
    /// 创建服务；项目工作区根目录由外部配置边界明确注入。
    pub(crate) fn new(projects_root: PathBuf, rpg_maker_layout: RpgMakerLayout, sqlite: S) -> Self {
        Self {
            projects_root,
            rpg_maker_layout,
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
        let layout = ProjectWorkspaceLayout::for_project(
            &self.projects_root,
            self.rpg_maker_layout,
            requested_name,
        );
        let database_path = layout.database_path().to_path_buf();
        let query =
            SqliteQuery::new(SELECT_PROJECT_RECORD, Vec::new()).with_id(PROJECT_RECORD_QUERY_ID);
        let query_id = query.id().to_owned();
        let rows = self
            .sqlite
            .query_existing_database(database_path.clone(), query)
            .await
            .map_err(|error| {
                ProjectDatabaseReadError::from_executor(
                    database_path.clone(),
                    PROJECT_RECORD_READ_STAGE,
                    query_id,
                    error,
                )
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
    let mut rows = rows.into_iter();
    let row = rows
        .next()
        .ok_or_else(|| ProjectDatabaseReadError::InvalidMetadata {
            path: database_path.clone(),
            reason: InvalidProjectMetadata::MissingRow,
        })?;
    if rows.next().is_some() {
        return Err(ProjectDatabaseReadError::InvalidMetadata {
            path: database_path,
            reason: InvalidProjectMetadata::MultipleRows,
        });
    }
    let mut values = row.into_values();
    if values.len() != 8 {
        return Err(ProjectDatabaseReadError::InvalidMetadata {
            path: database_path,
            reason: InvalidProjectMetadata::WrongColumnCount {
                expected: 8,
                actual: values.len(),
            },
        });
    }
    let definition_json = text_column(
        values.pop().expect("已确认项目记录恰好有八列"),
        "mv_dialogue_rules",
    )
    .map_err(|reason| ProjectDatabaseReadError::InvalidMetadata {
        path: database_path.clone(),
        reason,
    })?;
    let mv_dialogue_definition = MvDialogueDefinition::from_canonical_json(&definition_json)
        .map_err(|source| ProjectDatabaseReadError::InvalidMetadata {
            path: database_path.clone(),
            reason: InvalidProjectMetadata::InvalidDialogueDefinition {
                stage: ProjectDefinitionStage::Decode,
                failure: project_definition_failure(source),
            },
        })?;
    let metadata = metadata_facts_from_rows(requested_name, vec![SqliteRow::new(values)]).map_err(
        |reason| ProjectDatabaseReadError::InvalidMetadata {
            path: database_path.clone(),
            reason,
        },
    )?;

    Ok(StoredProjectRecord::from_layout(
        metadata.name,
        layout,
        metadata.language_pair,
        metadata.source_snapshot_fingerprint,
        metadata.layout_profile,
        mv_dialogue_definition,
    ))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProjectMetadataFacts {
    name: ProjectName,
    language_pair: LanguagePair,
    source_snapshot_fingerprint: SourceSnapshotFingerprint,
    layout_profile: RpgMakerWriteBackLayoutProfile,
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
            expected: 7,
            actual: values.len(),
        });
    }

    let mut values = values.into_iter();
    let stored_name = text_column(values.next().expect("已确认 metadata 恰好有七列"), "name")?;
    let source_language = language_id_column(
        values.next().expect("已确认 metadata 恰好有七列"),
        "source_language",
    )?;
    let target_language = language_id_column(
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

    Ok(ProjectMetadataFacts {
        name: stored_name,
        language_pair: LanguagePair::new(source_language, target_language),
        source_snapshot_fingerprint,
        layout_profile: RpgMakerWriteBackLayoutProfile::new(
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

fn language_id_column(
    value: SqliteValue,
    column: &'static str,
) -> Result<LanguageId, InvalidProjectMetadata> {
    let stored = text_column(value, column)?;
    let language_id = LanguageId::parse(&stored)
        .map_err(|source| InvalidProjectMetadata::InvalidLanguage { column, source })?;
    if language_id.as_str() != stored {
        return Err(InvalidProjectMetadata::NonCanonicalLanguage {
            column,
            stored,
            canonical: language_id.as_str().to_owned(),
        });
    }
    Ok(language_id)
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
        stage: &'static str,
        query_id: String,
        source: E,
    },
    InvalidMetadata {
        path: PathBuf,
        reason: InvalidProjectMetadata,
    },
}

impl<E> ProjectDatabaseReadError<E> {
    fn from_executor(
        path: PathBuf,
        stage: &'static str,
        query_id: String,
        error: QueryExistingDatabaseError<E>,
    ) -> Self {
        match error {
            QueryExistingDatabaseError::NotFound => Self::DatabaseNotFound { path },
            QueryExistingDatabaseError::QueryFailed(source) => Self::ReadDatabase {
                path,
                stage,
                query_id,
                source,
            },
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
            Self::ReadDatabase {
                path,
                stage,
                query_id,
                source,
            } => write!(
                formatter,
                "无法读取项目数据库 {}（阶段 {stage}，查询 {query_id}）：{source}",
                path.display()
            ),
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
        expected: usize,
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
    InvalidLanguage {
        column: &'static str,
        source: LanguageIdError,
    },
    NonCanonicalLanguage {
        column: &'static str,
        stored: String,
        canonical: String,
    },
    InvalidLineWidth {
        column: &'static str,
        actual: i64,
    },
    InvalidSourceSnapshotFingerprintLength {
        actual: usize,
    },
    InvalidDialogueDefinition {
        stage: ProjectDefinitionStage,
        failure: ProjectDefinitionFailure,
    },
}

impl InvalidProjectMetadata {
    /// 只从 metadata 错误仍持有的类型化字段建立公开事实。
    ///
    /// 数据库中的定义正文、源文本以及底层错误 `Display` 均不会进入结果。
    pub(crate) fn safe_fact(&self) -> String {
        match self {
            Self::MissingRow => "metadata=missing_row".to_owned(),
            Self::MultipleRows => "metadata=multiple_rows".to_owned(),
            Self::WrongColumnCount { expected, actual } => {
                format!("metadata=wrong_column_count; expected={expected}; actual={actual}")
            }
            Self::WrongColumnType {
                column,
                expected,
                actual,
            } => format!(
                "metadata=wrong_column_type; column={column}; expected={expected}; actual={actual}"
            ),
            // ProjectName 当前只提供面向人的 String 错误；在它改为闭集类型前不能把该
            // 任意文本误当作结构化诊断。字段身份仍保留，内部 Display 继续用于因果链。
            Self::InvalidProjectName { .. } => {
                "metadata=invalid_project_name; field=name".to_owned()
            }
            Self::NameMismatch { requested, stored } => format!(
                "metadata=name_mismatch; requested={}; stored={}",
                crate::user_text::sanitize_user_text(requested),
                crate::user_text::sanitize_user_text(stored)
            ),
            Self::InvalidLanguage { column, source } => format!(
                "metadata=invalid_language; column={column}; {}",
                language_id_failure_safe_fact(source)
            ),
            Self::NonCanonicalLanguage {
                column,
                stored,
                canonical,
            } => format!(
                "metadata=noncanonical_language; column={column}; stored={}; canonical={}",
                crate::user_text::sanitize_user_text(stored),
                crate::user_text::sanitize_user_text(canonical)
            ),
            Self::InvalidLineWidth { column, actual } => {
                format!(
                    "metadata=invalid_line_width; column={column}; expected=positive_u32; actual={actual}"
                )
            }
            Self::InvalidSourceSnapshotFingerprintLength { actual } => format!(
                "metadata=invalid_source_fingerprint_length; field=source_snapshot_fingerprint; expected=32; actual={actual}"
            ),
            Self::InvalidDialogueDefinition { stage, failure } => format!(
                "metadata=invalid_dialogue_definition; definition={}; stage={}; {}",
                ProjectDefinitionKind::MvDialogueRules.storage_name(),
                stage.as_str(),
                project_definition_failure_safe_fact(failure)
            ),
        }
    }
}

impl fmt::Display for InvalidProjectMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRow => formatter.write_str("缺少项目记录"),
            Self::MultipleRows => formatter.write_str("包含多条项目记录"),
            Self::WrongColumnCount { expected, actual } => {
                write!(
                    formatter,
                    "查询结果列数不符合当前契约，应为 {expected} 列，实际为 {actual} 列"
                )
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
            Self::InvalidLanguage { column, source } => {
                write!(formatter, "字段 {column} 不是有效语言 ID：{source}")
            }
            Self::NonCanonicalLanguage {
                column,
                stored,
                canonical,
            } => write!(
                formatter,
                "字段 {column} 必须保存规范语言 ID，实际为 {stored:?}，规范形式为 {canonical:?}"
            ),
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
            Self::InvalidDialogueDefinition { stage, failure } => {
                write!(
                    formatter,
                    "MV 对话定义在 {} 阶段无效：{}",
                    stage.as_str(),
                    project_definition_failure_safe_fact(failure)
                )
            }
        }
    }
}

impl Error for InvalidProjectMetadata {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidLanguage { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ActiveOwnerState {
    owner: RpgMakerStandardAssetOwner,
    source_snapshot_fingerprint: SourceSnapshotFingerprint,
    asset_snapshot_fingerprint: AssetSnapshotFingerprint,
}

/// 一个 active owner 相对于当前项目冻结来源的精确新鲜度。
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StandardAssetOwnerFreshness {
    owner: RpgMakerStandardAssetOwner,
    fresh: bool,
}

/// 已经按当前完整 schema 验证的项目数据库事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectDatabaseState {
    metadata: ProjectMetadataFacts,
    owners: Vec<ActiveOwnerState>,
    terminology_json: String,
    placeholder_rules_json: String,
    mv_dialogue_rules_json: String,
    run_plans: ProjectRunPlans,
    schema_version: i64,
}

impl ProjectDatabaseState {
    #[cfg(test)]
    pub(crate) fn for_test(
        name: ProjectName,
        language_pair: LanguagePair,
        source_snapshot_fingerprint: SourceSnapshotFingerprint,
        layout_profile: RpgMakerWriteBackLayoutProfile,
        owners: Vec<(RpgMakerStandardAssetOwner, SourceSnapshotFingerprint)>,
    ) -> Self {
        Self {
            metadata: ProjectMetadataFacts {
                name,
                language_pair,
                source_snapshot_fingerprint,
                layout_profile,
            },
            owners: owners
                .into_iter()
                .map(|(owner, source_snapshot_fingerprint)| ActiveOwnerState {
                    owner,
                    source_snapshot_fingerprint,
                    asset_snapshot_fingerprint: AssetSnapshotFingerprint::from_bytes(
                        *source_snapshot_fingerprint.as_bytes(),
                    ),
                })
                .collect(),
            terminology_json: "[]".to_owned(),
            placeholder_rules_json: "[]".to_owned(),
            mv_dialogue_rules_json: r#"{"rules":[]}"#.to_owned(),
            run_plans: ProjectRunPlans::default(),
            schema_version: 13,
        }
    }

    pub(crate) fn language_pair(&self) -> &LanguagePair {
        &self.metadata.language_pair
    }

    pub(crate) fn source_language(&self) -> &LanguageId {
        self.metadata.language_pair.source()
    }

    pub(crate) fn target_language(&self) -> &LanguageId {
        self.metadata.language_pair.target()
    }

    pub(crate) fn layout_profile(&self) -> &RpgMakerWriteBackLayoutProfile {
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

    pub(crate) fn stale_owners(&self) -> Vec<RpgMakerStandardAssetOwner> {
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

    #[cfg(test)]
    pub(crate) fn mv_dialogue_rules_json(&self) -> &str {
        &self.mv_dialogue_rules_json
    }
}

fn owner_sort_key(owner: RpgMakerStandardAssetOwner) -> u8 {
    match owner {
        RpgMakerStandardAssetOwner::Builtin => 0,
        RpgMakerStandardAssetOwner::Rules => 1,
        RpgMakerStandardAssetOwner::Lua => 2,
    }
}

/// 项目数据库无法按当前唯一 schema 重建为受信事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum InvalidCurrentProjectDatabase {
    ManagedSchema(InvalidManagedSchema),
    Metadata(InvalidProjectMetadata),
    OwnerState(InvalidOwnerState),
    TranslationResources(InvalidTranslationResources),
    ProjectDefinitions(InvalidProjectDefinitions),
    RunPlans(InvalidProjectRunPlans),
    Integrity(InvalidProjectDatabaseIntegrity),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectDatabaseValueKind {
    Null,
    Integer,
    Real,
    Text,
    Blob,
}

impl ProjectDatabaseValueKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Null => "NULL",
            Self::Integer => "INTEGER",
            Self::Real => "REAL",
            Self::Text => "TEXT",
            Self::Blob => "BLOB",
        }
    }
}

impl From<&SqliteValue> for ProjectDatabaseValueKind {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectDatabaseField {
    SchemaType,
    SchemaName,
    SchemaTableName,
    SchemaSql,
    SchemaVersion,
    Owner,
    SourceSnapshotFingerprint,
    AssetSnapshotFingerprint,
    ResourceKind,
    CanonicalJson,
    DefinitionKind,
}

impl ProjectDatabaseField {
    const fn as_str(self) -> &'static str {
        match self {
            Self::SchemaType => "sqlite_schema.type",
            Self::SchemaName => "sqlite_schema.name",
            Self::SchemaTableName => "sqlite_schema.tbl_name",
            Self::SchemaSql => "sqlite_schema.sql",
            Self::SchemaVersion => "schema_version",
            Self::Owner => "owner",
            Self::SourceSnapshotFingerprint => "source_snapshot_fingerprint",
            Self::AssetSnapshotFingerprint => "asset_snapshot_fingerprint",
            Self::ResourceKind => "resource_kind",
            Self::CanonicalJson => "canonical_json",
            Self::DefinitionKind => "definition_kind",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedSchemaObject {
    Metadata,
    InitRunPlan,
    ExtractRunPlan,
    ExtractRulesDefinition,
    TranslateRunPlan,
    WriteBackRunPlan,
    LuaProgram,
    StandardAssetOwnerState,
    StandardTextGroup,
    StandardTextUnit,
    StandardMutationClaim,
    StandardTranslationResource,
    StandardProjectDefinition,
    StandardMutationClaimOwnerResourceIndex,
    StandardMutationClaimResourceIndex,
}

impl ManagedSchemaObject {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Metadata => "table:metadata",
            Self::InitRunPlan => "table:init_run_plan",
            Self::ExtractRunPlan => "table:extract_run_plan",
            Self::ExtractRulesDefinition => "table:extract_rules_definition",
            Self::TranslateRunPlan => "table:translate_run_plan",
            Self::WriteBackRunPlan => "table:write_back_run_plan",
            Self::LuaProgram => "table:lua_program",
            Self::StandardAssetOwnerState => "table:standard_asset_owner_state",
            Self::StandardTextGroup => "table:standard_text_group",
            Self::StandardTextUnit => "table:standard_text_unit",
            Self::StandardMutationClaim => "table:standard_mutation_claim",
            Self::StandardTranslationResource => "table:standard_translation_resource",
            Self::StandardProjectDefinition => "table:standard_project_definition",
            Self::StandardMutationClaimOwnerResourceIndex => {
                "index:standard_mutation_claim_owner_resource_idx"
            }
            Self::StandardMutationClaimResourceIndex => {
                "index:standard_mutation_claim_resource_idx"
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum InvalidManagedSchema {
    WrongColumnCount {
        query: &'static str,
        expected: usize,
        actual: usize,
    },
    WrongRowCount {
        query: &'static str,
        expected: usize,
        actual: usize,
    },
    WrongColumnType {
        field: ProjectDatabaseField,
        expected: ProjectDatabaseValueKind,
        actual: ProjectDatabaseValueKind,
    },
    NegativeSchemaVersion {
        actual: i64,
    },
    ObjectMismatch {
        expected_count: usize,
        actual_count: usize,
        missing: Vec<ManagedSchemaObject>,
        definition_mismatches: Vec<ManagedSchemaObject>,
        unexpected_count: usize,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum InvalidOwnerState {
    WrongColumnCount {
        expected: usize,
        actual: usize,
    },
    WrongColumnType {
        field: ProjectDatabaseField,
        expected: ProjectDatabaseValueKind,
        actual: ProjectDatabaseValueKind,
    },
    UnknownOwner,
    InvalidFingerprintLength {
        owner: RpgMakerStandardAssetOwner,
        field: ProjectDatabaseField,
        expected: usize,
        actual: usize,
    },
    DuplicateOwner {
        owner: RpgMakerStandardAssetOwner,
    },
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum TranslationResourceKind {
    Terminology,
    PlaceholderRules,
}

impl TranslationResourceKind {
    fn from_storage_name(value: &str) -> Option<Self> {
        if value == TERMINOLOGY_RESOURCE_KIND {
            Some(Self::Terminology)
        } else if value == PLACEHOLDER_RULES_RESOURCE_KIND {
            Some(Self::PlaceholderRules)
        } else {
            None
        }
    }

    const fn storage_name(self) -> &'static str {
        match self {
            Self::Terminology => TERMINOLOGY_RESOURCE_KIND,
            Self::PlaceholderRules => PLACEHOLDER_RULES_RESOURCE_KIND,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SafeJsonErrorCategory {
    Io,
    Syntax,
    Data,
    Eof,
}

impl SafeJsonErrorCategory {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Io => "io",
            Self::Syntax => "syntax",
            Self::Data => "data",
            Self::Eof => "eof",
        }
    }
}

impl From<&serde_json::Error> for SafeJsonErrorCategory {
    fn from(source: &serde_json::Error) -> Self {
        match source.classify() {
            serde_json::error::Category::Io => Self::Io,
            serde_json::error::Category::Syntax => Self::Syntax,
            serde_json::error::Category::Data => Self::Data,
            serde_json::error::Category::Eof => Self::Eof,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum InvalidTranslationResources {
    WrongColumnCount {
        expected: usize,
        actual: usize,
    },
    WrongColumnType {
        field: ProjectDatabaseField,
        expected: ProjectDatabaseValueKind,
        actual: ProjectDatabaseValueKind,
    },
    UnknownResourceKind,
    InvalidJson {
        resource: TranslationResourceKind,
        category: SafeJsonErrorCategory,
        line: usize,
        column: usize,
    },
    JsonMustBeArray {
        resource: TranslationResourceKind,
    },
    DuplicateResource {
        resource: TranslationResourceKind,
    },
    WrongResourceCount {
        expected: usize,
        actual: usize,
    },
    MissingResource {
        resource: TranslationResourceKind,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectDefinitionKind {
    MvDialogueRules,
}

impl ProjectDefinitionKind {
    const fn storage_name(self) -> &'static str {
        match self {
            Self::MvDialogueRules => MV_DIALOGUE_RULES_DEFINITION_KIND,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectDefinitionStage {
    Decode,
    Compile,
    Encode,
}

impl ProjectDefinitionStage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Decode => "decode",
            Self::Compile => "compile",
            Self::Encode => "encode",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Pcre2FailureKind {
    Compile,
    Jit,
    Match,
    Info,
    Option,
    Unknown,
}

impl Pcre2FailureKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Compile => "compile",
            Self::Jit => "jit",
            Self::Match => "match",
            Self::Info => "info",
            Self::Option => "option",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProjectDefinitionFailure {
    EmptyDocument,
    MissingRuleArray,
    InvalidToml {
        byte_start: Option<usize>,
        byte_end: Option<usize>,
    },
    InvalidJson {
        category: SafeJsonErrorCategory,
        line: usize,
        column: usize,
    },
    EncodeJson {
        category: SafeJsonErrorCategory,
        line: usize,
        column: usize,
    },
    EmptyPattern {
        rule_number: usize,
    },
    InvalidPattern {
        rule_number: usize,
        kind: Pcre2FailureKind,
        code: i32,
        offset: Option<usize>,
    },
    InvalidNamedCaptures {
        rule_number: usize,
        actual_count: usize,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum InvalidProjectDefinitions {
    WrongRowCount {
        expected: usize,
        actual: usize,
    },
    WrongColumnCount {
        expected: usize,
        actual: usize,
    },
    WrongColumnType {
        field: ProjectDatabaseField,
        expected: ProjectDatabaseValueKind,
        actual: ProjectDatabaseValueKind,
    },
    UnknownDefinitionKind,
    InvalidDefinition {
        definition: ProjectDefinitionKind,
        stage: ProjectDefinitionStage,
        failure: ProjectDefinitionFailure,
    },
    NonCanonicalJson {
        definition: ProjectDefinitionKind,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum InvalidProjectDatabaseIntegrity {
    QuickCheckWrongRowCount {
        expected: usize,
        actual: usize,
    },
    QuickCheckWrongColumnCount {
        expected: usize,
        actual: usize,
    },
    QuickCheckWrongColumnType {
        expected: ProjectDatabaseValueKind,
        actual: ProjectDatabaseValueKind,
    },
    QuickCheckFailed,
    ForeignKeyViolations {
        actual: usize,
    },
}

impl fmt::Display for InvalidCurrentProjectDatabase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.safe_fact())
    }
}

impl InvalidCurrentProjectDatabase {
    /// 只从闭集 reason 的类型化字段建立可公开事实，不读取任意错误文本或持久正文。
    pub(crate) fn safe_fact(&self) -> String {
        match self {
            Self::ManagedSchema(reason) => {
                format!("database_component=managed_schema; {}", reason.safe_fact())
            }
            Self::Metadata(reason) => {
                format!("database_component=metadata; {}", reason.safe_fact())
            }
            Self::OwnerState(reason) => {
                format!("database_component=owner_state; {}", reason.safe_fact())
            }
            Self::TranslationResources(reason) => format!(
                "database_component=translation_resources; {}",
                reason.safe_fact()
            ),
            Self::ProjectDefinitions(reason) => format!(
                "database_component=project_definitions; {}",
                reason.safe_fact()
            ),
            Self::RunPlans(reason) => format!(
                "database_component=run_plans; subject={}; detail={}",
                reason.safe_subject(),
                reason.safe_detail()
            ),
            Self::Integrity(reason) => {
                format!("database_component=integrity; {}", reason.safe_fact())
            }
        }
    }

    /// 返回领域错误已经筛选过的恢复事实；其他数据库状态错误没有额外恢复位置。
    pub(crate) const fn recovery_fact(&self) -> Option<&crate::diagnostic::RecoveryFact> {
        match self {
            Self::RunPlans(reason) => reason.recovery_fact(),
            _ => None,
        }
    }
}

impl InvalidManagedSchema {
    fn safe_fact(&self) -> String {
        match self {
            Self::WrongColumnCount {
                query,
                expected,
                actual,
            } => format!(
                "violation=wrong_column_count; query={query}; expected={expected}; actual={actual}"
            ),
            Self::WrongRowCount {
                query,
                expected,
                actual,
            } => format!(
                "violation=wrong_row_count; query={query}; expected={expected}; actual={actual}"
            ),
            Self::WrongColumnType {
                field,
                expected,
                actual,
            } => format!(
                "violation=wrong_column_type; field={}; expected={}; actual={}",
                field.as_str(),
                expected.as_str(),
                actual.as_str()
            ),
            Self::NegativeSchemaVersion { actual } => {
                format!("violation=negative_schema_version; actual={actual}")
            }
            Self::ObjectMismatch {
                expected_count,
                actual_count,
                missing,
                definition_mismatches,
                unexpected_count,
            } => format!(
                "violation=managed_object_mismatch; expected_count={expected_count}; actual_count={actual_count}; missing={}; definition_mismatches={}; unexpected_count={unexpected_count}",
                managed_schema_object_list(missing),
                managed_schema_object_list(definition_mismatches)
            ),
        }
    }
}

impl InvalidOwnerState {
    fn safe_fact(&self) -> String {
        match self {
            Self::WrongColumnCount { expected, actual } => {
                format!("violation=wrong_column_count; expected={expected}; actual={actual}")
            }
            Self::WrongColumnType {
                field,
                expected,
                actual,
            } => format!(
                "violation=wrong_column_type; field={}; expected={}; actual={}",
                field.as_str(),
                expected.as_str(),
                actual.as_str()
            ),
            Self::UnknownOwner => "violation=unknown_owner".to_owned(),
            Self::InvalidFingerprintLength {
                owner,
                field,
                expected,
                actual,
            } => format!(
                "violation=invalid_fingerprint_length; owner={}; field={}; expected={expected}; actual={actual}",
                owner.storage_name(),
                field.as_str()
            ),
            Self::DuplicateOwner { owner } => {
                format!("violation=duplicate_owner; owner={}", owner.storage_name())
            }
        }
    }
}

impl InvalidTranslationResources {
    fn safe_fact(&self) -> String {
        match self {
            Self::WrongColumnCount { expected, actual } => {
                format!("violation=wrong_column_count; expected={expected}; actual={actual}")
            }
            Self::WrongColumnType {
                field,
                expected,
                actual,
            } => format!(
                "violation=wrong_column_type; field={}; expected={}; actual={}",
                field.as_str(),
                expected.as_str(),
                actual.as_str()
            ),
            Self::UnknownResourceKind => "violation=unknown_resource_kind".to_owned(),
            Self::InvalidJson {
                resource,
                category,
                line,
                column,
            } => format!(
                "violation=invalid_json; resource={}; category={}; line={line}; column={column}",
                resource.storage_name(),
                category.as_str()
            ),
            Self::JsonMustBeArray { resource } => format!(
                "violation=json_shape; resource={}; expected=array",
                resource.storage_name()
            ),
            Self::DuplicateResource { resource } => format!(
                "violation=duplicate_resource; resource={}",
                resource.storage_name()
            ),
            Self::WrongResourceCount { expected, actual } => {
                format!("violation=wrong_resource_count; expected={expected}; actual={actual}")
            }
            Self::MissingResource { resource } => format!(
                "violation=missing_resource; resource={}",
                resource.storage_name()
            ),
        }
    }
}

impl InvalidProjectDefinitions {
    fn safe_fact(&self) -> String {
        match self {
            Self::WrongRowCount { expected, actual } => {
                format!("violation=wrong_row_count; expected={expected}; actual={actual}")
            }
            Self::WrongColumnCount { expected, actual } => {
                format!("violation=wrong_column_count; expected={expected}; actual={actual}")
            }
            Self::WrongColumnType {
                field,
                expected,
                actual,
            } => format!(
                "violation=wrong_column_type; field={}; expected={}; actual={}",
                field.as_str(),
                expected.as_str(),
                actual.as_str()
            ),
            Self::UnknownDefinitionKind => "violation=unknown_definition_kind".to_owned(),
            Self::InvalidDefinition {
                definition,
                stage,
                failure,
            } => format!(
                "violation=invalid_definition; definition={}; stage={}; {}",
                definition.storage_name(),
                stage.as_str(),
                project_definition_failure_safe_fact(failure)
            ),
            Self::NonCanonicalJson { definition } => format!(
                "violation=noncanonical_json; definition={}",
                definition.storage_name()
            ),
        }
    }
}

impl InvalidProjectDatabaseIntegrity {
    fn safe_fact(&self) -> String {
        match self {
            Self::QuickCheckWrongRowCount { expected, actual } => format!(
                "violation=quick_check_wrong_row_count; expected={expected}; actual={actual}"
            ),
            Self::QuickCheckWrongColumnCount { expected, actual } => format!(
                "violation=quick_check_wrong_column_count; expected={expected}; actual={actual}"
            ),
            Self::QuickCheckWrongColumnType { expected, actual } => format!(
                "violation=quick_check_wrong_column_type; expected={}; actual={}",
                expected.as_str(),
                actual.as_str()
            ),
            Self::QuickCheckFailed => "violation=quick_check_failed".to_owned(),
            Self::ForeignKeyViolations { actual } => {
                format!("violation=foreign_key_check; actual={actual}")
            }
        }
    }
}

fn project_definition_failure_safe_fact(failure: &ProjectDefinitionFailure) -> String {
    match failure {
        ProjectDefinitionFailure::EmptyDocument => "failure=empty_document".to_owned(),
        ProjectDefinitionFailure::MissingRuleArray => "failure=missing_rule_array".to_owned(),
        ProjectDefinitionFailure::InvalidToml {
            byte_start,
            byte_end,
        } => format!(
            "failure=invalid_toml; byte_start={}; byte_end={}",
            optional_usize(*byte_start),
            optional_usize(*byte_end)
        ),
        ProjectDefinitionFailure::InvalidJson {
            category,
            line,
            column,
        } => format!(
            "failure=invalid_json; category={}; line={line}; column={column}",
            category.as_str()
        ),
        ProjectDefinitionFailure::EncodeJson {
            category,
            line,
            column,
        } => format!(
            "failure=encode_json; category={}; line={line}; column={column}",
            category.as_str()
        ),
        ProjectDefinitionFailure::EmptyPattern { rule_number } => {
            format!("failure=empty_pattern; rule_number={rule_number}")
        }
        ProjectDefinitionFailure::InvalidPattern {
            rule_number,
            kind,
            code,
            offset,
        } => format!(
            "failure=invalid_pattern; rule_number={rule_number}; engine=pcre2; kind={}; code={code}; offset={}",
            kind.as_str(),
            optional_usize(*offset)
        ),
        ProjectDefinitionFailure::InvalidNamedCaptures {
            rule_number,
            actual_count,
        } => format!(
            "failure=invalid_named_captures; rule_number={rule_number}; actual_count={actual_count}"
        ),
    }
}

fn language_id_failure_safe_fact(source: &LanguageIdError) -> &'static str {
    match source {
        LanguageIdError::Blank => "language_failure=blank",
        LanguageIdError::SurroundingWhitespace { .. } => "language_failure=surrounding_whitespace",
        LanguageIdError::Underscore { .. } => "language_failure=underscore_separator",
        LanguageIdError::InvalidSyntax { .. } => "language_failure=invalid_rfc5646_syntax",
        LanguageIdError::InvalidRegistryTag { .. } => "language_failure=invalid_iana_registry_tag",
        LanguageIdError::CanonicalizationFailed { .. } => {
            "language_failure=canonicalization_failed"
        }
        LanguageIdError::UndefinedPrimaryLanguage { .. } => {
            "language_failure=undefined_primary_language"
        }
    }
}

fn managed_schema_object_list(objects: &[ManagedSchemaObject]) -> String {
    if objects.is_empty() {
        "none".to_owned()
    } else {
        objects
            .iter()
            .map(|object| object.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn optional_usize(value: Option<usize>) -> String {
    value.map_or_else(|| "none".to_owned(), |value| value.to_string())
}

impl Error for InvalidCurrentProjectDatabase {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Metadata(source) => Some(source),
            Self::RunPlans(source) => Some(source),
            Self::ManagedSchema(_)
            | Self::OwnerState(_)
            | Self::TranslationResources(_)
            | Self::ProjectDefinitions(_)
            | Self::Integrity(_) => None,
        }
    }
}

fn expected_managed_schema() -> Vec<(&'static str, &'static str, &'static str, &'static str)> {
    vec![
        ("table", "metadata", "metadata", CREATE_METADATA_TABLE),
        (
            "table",
            "init_run_plan",
            "init_run_plan",
            CREATE_INIT_RUN_PLAN_TABLE,
        ),
        (
            "table",
            "extract_run_plan",
            "extract_run_plan",
            CREATE_EXTRACT_RUN_PLAN_TABLE,
        ),
        (
            "table",
            "extract_rules_definition",
            "extract_rules_definition",
            CREATE_EXTRACT_RULES_DEFINITION_TABLE,
        ),
        (
            "table",
            "translate_run_plan",
            "translate_run_plan",
            CREATE_TRANSLATE_RUN_PLAN_TABLE,
        ),
        (
            "table",
            "write_back_run_plan",
            "write_back_run_plan",
            CREATE_WRITE_BACK_RUN_PLAN_TABLE,
        ),
        (
            "table",
            "lua_program",
            "lua_program",
            CREATE_LUA_PROGRAM_TABLE,
        ),
        (
            "table",
            "standard_asset_owner_state",
            "standard_asset_owner_state",
            CREATE_STANDARD_ASSET_OWNER_STATE_TABLE,
        ),
        (
            "table",
            STANDARD_TEXT_GROUP_TABLE_NAME,
            STANDARD_TEXT_GROUP_TABLE_NAME,
            CREATE_STANDARD_TEXT_GROUP_TABLE,
        ),
        (
            "table",
            STANDARD_TEXT_UNIT_TABLE_NAME,
            STANDARD_TEXT_UNIT_TABLE_NAME,
            CREATE_STANDARD_TEXT_UNIT_TABLE,
        ),
        (
            "table",
            STANDARD_MUTATION_CLAIM_TABLE_NAME,
            STANDARD_MUTATION_CLAIM_TABLE_NAME,
            CREATE_STANDARD_MUTATION_CLAIM_TABLE,
        ),
        (
            "table",
            STANDARD_TRANSLATION_RESOURCE_TABLE_NAME,
            STANDARD_TRANSLATION_RESOURCE_TABLE_NAME,
            CREATE_STANDARD_TRANSLATION_RESOURCE_TABLE,
        ),
        (
            "table",
            STANDARD_PROJECT_DEFINITION_TABLE_NAME,
            STANDARD_PROJECT_DEFINITION_TABLE_NAME,
            CREATE_STANDARD_PROJECT_DEFINITION_TABLE,
        ),
        (
            "index",
            "standard_mutation_claim_owner_resource_idx",
            STANDARD_MUTATION_CLAIM_TABLE_NAME,
            CREATE_STANDARD_MUTATION_CLAIM_OWNER_RESOURCE_INDEX,
        ),
        (
            "index",
            "standard_mutation_claim_resource_idx",
            STANDARD_MUTATION_CLAIM_TABLE_NAME,
            CREATE_STANDARD_MUTATION_CLAIM_RESOURCE_INDEX,
        ),
    ]
}

fn validate_managed_schema(rows: Vec<SqliteRow>) -> Result<(), InvalidCurrentProjectDatabase> {
    let mut actual = Vec::with_capacity(rows.len());
    for row in rows {
        let values = row.into_values();
        if values.len() != 4 {
            return Err(InvalidCurrentProjectDatabase::ManagedSchema(
                InvalidManagedSchema::WrongColumnCount {
                    query: "managed_schema",
                    expected: 4,
                    actual: values.len(),
                },
            ));
        }
        let mut values = values.into_iter();
        let kind = schema_text(
            values.next().expect("已确认有四列"),
            ProjectDatabaseField::SchemaType,
        )?;
        let name = schema_text(
            values.next().expect("已确认有四列"),
            ProjectDatabaseField::SchemaName,
        )?;
        let table = schema_text(
            values.next().expect("已确认有四列"),
            ProjectDatabaseField::SchemaTableName,
        )?;
        let sql = schema_text(
            values.next().expect("已确认有四列"),
            ProjectDatabaseField::SchemaSql,
        )?;
        actual.push((kind, name, table, sql));
    }
    let expected = expected_managed_schema();
    let missing = expected
        .iter()
        .filter_map(|(kind, name, _, _)| {
            (!actual
                .iter()
                .any(|(actual_kind, actual_name, _, _)| actual_kind == kind && actual_name == name))
            .then(|| managed_schema_object(kind, name))
            .flatten()
        })
        .collect::<Vec<_>>();
    let definition_mismatches = expected
        .iter()
        .filter_map(|(kind, name, table, sql)| {
            actual
                .iter()
                .find(|(actual_kind, actual_name, _, _)| actual_kind == kind && actual_name == name)
                .and_then(|(_, _, actual_table, actual_sql)| {
                    (actual_table != table || actual_sql != sql)
                        .then(|| managed_schema_object(kind, name))
                        .flatten()
                })
        })
        .collect::<Vec<_>>();
    let unexpected_count = actual
        .iter()
        .filter(|(actual_kind, actual_name, _, _)| {
            !expected
                .iter()
                .any(|(kind, name, _, _)| actual_kind == kind && actual_name == name)
        })
        .count();
    if actual.len() == expected.len()
        && missing.is_empty()
        && definition_mismatches.is_empty()
        && unexpected_count == 0
    {
        Ok(())
    } else {
        Err(InvalidCurrentProjectDatabase::ManagedSchema(
            InvalidManagedSchema::ObjectMismatch {
                expected_count: expected.len(),
                actual_count: actual.len(),
                missing,
                definition_mismatches,
                unexpected_count,
            },
        ))
    }
}

fn schema_text(
    value: SqliteValue,
    field: ProjectDatabaseField,
) -> Result<String, InvalidCurrentProjectDatabase> {
    match value {
        SqliteValue::Text(value) => Ok(value),
        value => Err(InvalidCurrentProjectDatabase::ManagedSchema(
            InvalidManagedSchema::WrongColumnType {
                field,
                expected: ProjectDatabaseValueKind::Text,
                actual: ProjectDatabaseValueKind::from(&value),
            },
        )),
    }
}

fn managed_schema_object(kind: &str, name: &str) -> Option<ManagedSchemaObject> {
    match (kind, name) {
        ("table", "metadata") => Some(ManagedSchemaObject::Metadata),
        ("table", "init_run_plan") => Some(ManagedSchemaObject::InitRunPlan),
        ("table", "extract_run_plan") => Some(ManagedSchemaObject::ExtractRunPlan),
        ("table", "extract_rules_definition") => Some(ManagedSchemaObject::ExtractRulesDefinition),
        ("table", "translate_run_plan") => Some(ManagedSchemaObject::TranslateRunPlan),
        ("table", "write_back_run_plan") => Some(ManagedSchemaObject::WriteBackRunPlan),
        ("table", "lua_program") => Some(ManagedSchemaObject::LuaProgram),
        ("table", "standard_asset_owner_state") => {
            Some(ManagedSchemaObject::StandardAssetOwnerState)
        }
        ("table", STANDARD_TEXT_GROUP_TABLE_NAME) => Some(ManagedSchemaObject::StandardTextGroup),
        ("table", STANDARD_TEXT_UNIT_TABLE_NAME) => Some(ManagedSchemaObject::StandardTextUnit),
        ("table", STANDARD_MUTATION_CLAIM_TABLE_NAME) => {
            Some(ManagedSchemaObject::StandardMutationClaim)
        }
        ("table", STANDARD_TRANSLATION_RESOURCE_TABLE_NAME) => {
            Some(ManagedSchemaObject::StandardTranslationResource)
        }
        ("table", STANDARD_PROJECT_DEFINITION_TABLE_NAME) => {
            Some(ManagedSchemaObject::StandardProjectDefinition)
        }
        ("index", "standard_mutation_claim_owner_resource_idx") => {
            Some(ManagedSchemaObject::StandardMutationClaimOwnerResourceIndex)
        }
        ("index", "standard_mutation_claim_resource_idx") => {
            Some(ManagedSchemaObject::StandardMutationClaimResourceIndex)
        }
        _ => None,
    }
}

fn decode_schema_version(rows: Vec<SqliteRow>) -> Result<i64, InvalidCurrentProjectDatabase> {
    let [row] = <[SqliteRow; 1]>::try_from(rows).map_err(|rows| {
        InvalidCurrentProjectDatabase::ManagedSchema(InvalidManagedSchema::WrongRowCount {
            query: "schema_version",
            expected: 1,
            actual: rows.len(),
        })
    })?;
    let [value] = <[SqliteValue; 1]>::try_from(row.into_values()).map_err(|values| {
        InvalidCurrentProjectDatabase::ManagedSchema(InvalidManagedSchema::WrongColumnCount {
            query: "schema_version",
            expected: 1,
            actual: values.len(),
        })
    })?;
    match value {
        SqliteValue::Integer(value) if value >= 0 => Ok(value),
        SqliteValue::Integer(actual) => Err(InvalidCurrentProjectDatabase::ManagedSchema(
            InvalidManagedSchema::NegativeSchemaVersion { actual },
        )),
        value => Err(InvalidCurrentProjectDatabase::ManagedSchema(
            InvalidManagedSchema::WrongColumnType {
                field: ProjectDatabaseField::SchemaVersion,
                expected: ProjectDatabaseValueKind::Integer,
                actual: ProjectDatabaseValueKind::from(&value),
            },
        )),
    }
}

fn decode_owner_states(
    rows: Vec<SqliteRow>,
) -> Result<Vec<ActiveOwnerState>, InvalidCurrentProjectDatabase> {
    let mut owners = Vec::with_capacity(rows.len());
    for row in rows {
        let values = row.into_values();
        if values.len() != 3 {
            return Err(InvalidCurrentProjectDatabase::OwnerState(
                InvalidOwnerState::WrongColumnCount {
                    expected: 3,
                    actual: values.len(),
                },
            ));
        }
        let mut values = values.into_iter();
        let owner = match values.next().expect("已确认有三列") {
            SqliteValue::Text(value) => RpgMakerStandardAssetOwner::from_storage_name(&value)
                .ok_or(InvalidCurrentProjectDatabase::OwnerState(
                    InvalidOwnerState::UnknownOwner,
                ))?,
            value => {
                return Err(InvalidCurrentProjectDatabase::OwnerState(
                    InvalidOwnerState::WrongColumnType {
                        field: ProjectDatabaseField::Owner,
                        expected: ProjectDatabaseValueKind::Text,
                        actual: ProjectDatabaseValueKind::from(&value),
                    },
                ));
            }
        };
        let fingerprint = match values.next().expect("已确认有三列") {
            SqliteValue::Blob(value) => {
                SourceSnapshotFingerprint::from_slice(&value).map_err(|_| {
                    InvalidCurrentProjectDatabase::OwnerState(
                        InvalidOwnerState::InvalidFingerprintLength {
                            owner,
                            field: ProjectDatabaseField::SourceSnapshotFingerprint,
                            expected: 32,
                            actual: value.len(),
                        },
                    )
                })?
            }
            value => {
                return Err(InvalidCurrentProjectDatabase::OwnerState(
                    InvalidOwnerState::WrongColumnType {
                        field: ProjectDatabaseField::SourceSnapshotFingerprint,
                        expected: ProjectDatabaseValueKind::Blob,
                        actual: ProjectDatabaseValueKind::from(&value),
                    },
                ));
            }
        };
        let asset_snapshot_fingerprint = match values.next().expect("已确认有三列") {
            SqliteValue::Blob(value) => {
                AssetSnapshotFingerprint::from_slice(&value).map_err(|_| {
                    InvalidCurrentProjectDatabase::OwnerState(
                        InvalidOwnerState::InvalidFingerprintLength {
                            owner,
                            field: ProjectDatabaseField::AssetSnapshotFingerprint,
                            expected: 32,
                            actual: value.len(),
                        },
                    )
                })?
            }
            value => {
                return Err(InvalidCurrentProjectDatabase::OwnerState(
                    InvalidOwnerState::WrongColumnType {
                        field: ProjectDatabaseField::AssetSnapshotFingerprint,
                        expected: ProjectDatabaseValueKind::Blob,
                        actual: ProjectDatabaseValueKind::from(&value),
                    },
                ));
            }
        };
        if owners
            .iter()
            .any(|state: &ActiveOwnerState| state.owner == owner)
        {
            return Err(InvalidCurrentProjectDatabase::OwnerState(
                InvalidOwnerState::DuplicateOwner { owner },
            ));
        }
        owners.push(ActiveOwnerState {
            owner,
            source_snapshot_fingerprint: fingerprint,
            asset_snapshot_fingerprint,
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
            return Err(InvalidCurrentProjectDatabase::TranslationResources(
                InvalidTranslationResources::WrongColumnCount {
                    expected: 2,
                    actual: values.len(),
                },
            ));
        }
        let mut values = values.into_iter();
        let raw_kind = resource_text(
            values.next().expect("已确认有两列"),
            ProjectDatabaseField::ResourceKind,
        )?;
        let kind = TranslationResourceKind::from_storage_name(&raw_kind).ok_or(
            InvalidCurrentProjectDatabase::TranslationResources(
                InvalidTranslationResources::UnknownResourceKind,
            ),
        )?;
        let canonical_json = resource_text(
            values.next().expect("已确认有两列"),
            ProjectDatabaseField::CanonicalJson,
        )?;
        let json: serde_json::Value = serde_json::from_str(&canonical_json).map_err(|source| {
            InvalidCurrentProjectDatabase::TranslationResources(
                InvalidTranslationResources::InvalidJson {
                    resource: kind,
                    category: SafeJsonErrorCategory::from(&source),
                    line: source.line(),
                    column: source.column(),
                },
            )
        })?;
        if !json.is_array() {
            return Err(InvalidCurrentProjectDatabase::TranslationResources(
                InvalidTranslationResources::JsonMustBeArray { resource: kind },
            ));
        }
        if resources.insert(kind, canonical_json).is_some() {
            return Err(InvalidCurrentProjectDatabase::TranslationResources(
                InvalidTranslationResources::DuplicateResource { resource: kind },
            ));
        }
    }
    if resources.len() != 2 {
        return Err(InvalidCurrentProjectDatabase::TranslationResources(
            InvalidTranslationResources::WrongResourceCount {
                expected: 2,
                actual: resources.len(),
            },
        ));
    }
    let terminology = resources
        .remove(&TranslationResourceKind::Terminology)
        .ok_or(InvalidCurrentProjectDatabase::TranslationResources(
            InvalidTranslationResources::MissingResource {
                resource: TranslationResourceKind::Terminology,
            },
        ))?;
    let placeholders = resources
        .remove(&TranslationResourceKind::PlaceholderRules)
        .ok_or(InvalidCurrentProjectDatabase::TranslationResources(
            InvalidTranslationResources::MissingResource {
                resource: TranslationResourceKind::PlaceholderRules,
            },
        ))?;
    Ok((terminology, placeholders))
}

fn decode_project_definitions(
    rows: Vec<SqliteRow>,
) -> Result<String, InvalidCurrentProjectDatabase> {
    let [row] = <[SqliteRow; 1]>::try_from(rows).map_err(|rows| {
        InvalidCurrentProjectDatabase::ProjectDefinitions(
            InvalidProjectDefinitions::WrongRowCount {
                expected: 1,
                actual: rows.len(),
            },
        )
    })?;
    let values = row.into_values();
    if values.len() != 2 {
        return Err(InvalidCurrentProjectDatabase::ProjectDefinitions(
            InvalidProjectDefinitions::WrongColumnCount {
                expected: 2,
                actual: values.len(),
            },
        ));
    }
    let mut values = values.into_iter();
    let raw_definition_kind = project_definition_text(
        values.next().expect("已确认有两列"),
        ProjectDatabaseField::DefinitionKind,
    )?;
    let definition_kind = if raw_definition_kind == MV_DIALOGUE_RULES_DEFINITION_KIND {
        ProjectDefinitionKind::MvDialogueRules
    } else {
        return Err(InvalidCurrentProjectDatabase::ProjectDefinitions(
            InvalidProjectDefinitions::UnknownDefinitionKind,
        ));
    };
    let canonical_json = project_definition_text(
        values.next().expect("已确认有两列"),
        ProjectDatabaseField::CanonicalJson,
    )?;
    let definition =
        MvDialogueDefinition::from_canonical_json(&canonical_json).map_err(|source| {
            invalid_project_definition(definition_kind, ProjectDefinitionStage::Decode, source)
        })?;
    definition.compile().map_err(|source| {
        invalid_project_definition(definition_kind, ProjectDefinitionStage::Compile, source)
    })?;
    let encoded = definition.to_canonical_json().map_err(|source| {
        invalid_project_definition(definition_kind, ProjectDefinitionStage::Encode, source)
    })?;
    if encoded != canonical_json {
        return Err(InvalidCurrentProjectDatabase::ProjectDefinitions(
            InvalidProjectDefinitions::NonCanonicalJson {
                definition: definition_kind,
            },
        ));
    }
    Ok(canonical_json)
}

fn resource_text(
    value: SqliteValue,
    field: ProjectDatabaseField,
) -> Result<String, InvalidCurrentProjectDatabase> {
    match value {
        SqliteValue::Text(value) => Ok(value),
        value => Err(InvalidCurrentProjectDatabase::TranslationResources(
            InvalidTranslationResources::WrongColumnType {
                field,
                expected: ProjectDatabaseValueKind::Text,
                actual: ProjectDatabaseValueKind::from(&value),
            },
        )),
    }
}

fn project_definition_text(
    value: SqliteValue,
    field: ProjectDatabaseField,
) -> Result<String, InvalidCurrentProjectDatabase> {
    match value {
        SqliteValue::Text(value) => Ok(value),
        value => Err(InvalidCurrentProjectDatabase::ProjectDefinitions(
            InvalidProjectDefinitions::WrongColumnType {
                field,
                expected: ProjectDatabaseValueKind::Text,
                actual: ProjectDatabaseValueKind::from(&value),
            },
        )),
    }
}

fn invalid_project_definition(
    definition: ProjectDefinitionKind,
    stage: ProjectDefinitionStage,
    source: MvDialogueDefinitionError,
) -> InvalidCurrentProjectDatabase {
    InvalidCurrentProjectDatabase::ProjectDefinitions(
        InvalidProjectDefinitions::InvalidDefinition {
            definition,
            stage,
            failure: project_definition_failure(source),
        },
    )
}

fn project_definition_failure(source: MvDialogueDefinitionError) -> ProjectDefinitionFailure {
    match source {
        MvDialogueDefinitionError::EmptyDocument => ProjectDefinitionFailure::EmptyDocument,
        MvDialogueDefinitionError::MissingRuleArray => ProjectDefinitionFailure::MissingRuleArray,
        MvDialogueDefinitionError::InvalidToml(_) => ProjectDefinitionFailure::InvalidToml {
            byte_start: None,
            byte_end: None,
        },
        MvDialogueDefinitionError::InvalidCanonicalJson(source) => {
            ProjectDefinitionFailure::InvalidJson {
                category: SafeJsonErrorCategory::from(&source),
                line: source.line(),
                column: source.column(),
            }
        }
        MvDialogueDefinitionError::EncodeCanonicalJson(source) => {
            ProjectDefinitionFailure::EncodeJson {
                category: SafeJsonErrorCategory::from(&source),
                line: source.line(),
                column: source.column(),
            }
        }
        MvDialogueDefinitionError::EmptyPattern { rule_number } => {
            ProjectDefinitionFailure::EmptyPattern { rule_number }
        }
        MvDialogueDefinitionError::InvalidPattern {
            rule_number,
            source,
        } => ProjectDefinitionFailure::InvalidPattern {
            rule_number,
            kind: pcre2_failure_kind(&source),
            code: source.code(),
            offset: source.offset(),
        },
        MvDialogueDefinitionError::InvalidNamedCaptures {
            rule_number,
            captures,
        } => ProjectDefinitionFailure::InvalidNamedCaptures {
            rule_number,
            actual_count: captures.len(),
        },
    }
}

fn pcre2_failure_kind(source: &pcre2::Error) -> Pcre2FailureKind {
    match source.kind() {
        pcre2::ErrorKind::Compile => Pcre2FailureKind::Compile,
        pcre2::ErrorKind::JIT => Pcre2FailureKind::Jit,
        pcre2::ErrorKind::Match => Pcre2FailureKind::Match,
        pcre2::ErrorKind::Info => Pcre2FailureKind::Info,
        pcre2::ErrorKind::Option => Pcre2FailureKind::Option,
        _ => Pcre2FailureKind::Unknown,
    }
}

fn validate_integrity(
    quick_check: Vec<SqliteRow>,
    foreign_key_check: Vec<SqliteRow>,
) -> Result<(), InvalidCurrentProjectDatabase> {
    let [quick_check_row] = <[SqliteRow; 1]>::try_from(quick_check).map_err(|rows| {
        InvalidCurrentProjectDatabase::Integrity(
            InvalidProjectDatabaseIntegrity::QuickCheckWrongRowCount {
                expected: 1,
                actual: rows.len(),
            },
        )
    })?;
    let [quick_check_value] =
        <[SqliteValue; 1]>::try_from(quick_check_row.into_values()).map_err(|values| {
            InvalidCurrentProjectDatabase::Integrity(
                InvalidProjectDatabaseIntegrity::QuickCheckWrongColumnCount {
                    expected: 1,
                    actual: values.len(),
                },
            )
        })?;
    match quick_check_value {
        SqliteValue::Text(value) if value == "ok" => {}
        SqliteValue::Text(_) => {
            return Err(InvalidCurrentProjectDatabase::Integrity(
                InvalidProjectDatabaseIntegrity::QuickCheckFailed,
            ));
        }
        value => {
            return Err(InvalidCurrentProjectDatabase::Integrity(
                InvalidProjectDatabaseIntegrity::QuickCheckWrongColumnType {
                    expected: ProjectDatabaseValueKind::Text,
                    actual: ProjectDatabaseValueKind::from(&value),
                },
            ));
        }
    }
    if !foreign_key_check.is_empty() {
        return Err(InvalidCurrentProjectDatabase::Integrity(
            InvalidProjectDatabaseIntegrity::ForeignKeyViolations {
                actual: foreign_key_check.len(),
            },
        ));
    }
    Ok(())
}

/// 已由初始化用例建立并可以在内部信任的新项目事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NewProject {
    name: ProjectName,
    language_pair: LanguagePair,
    source_snapshot_fingerprint: SourceSnapshotFingerprint,
    layout_profile: RpgMakerWriteBackLayoutProfile,
}

impl NewProject {
    /// 汇集创建项目数据库所需的全部受信事实。
    pub(crate) fn new(
        name: ProjectName,
        language_pair: LanguagePair,
        source_snapshot_fingerprint: SourceSnapshotFingerprint,
        layout_profile: RpgMakerWriteBackLayoutProfile,
    ) -> Self {
        Self {
            name,
            language_pair,
            source_snapshot_fingerprint,
            layout_profile,
        }
    }

    pub(crate) fn name(&self) -> &ProjectName {
        &self.name
    }

    pub(crate) fn source_language(&self) -> &LanguageId {
        self.language_pair.source()
    }

    pub(crate) fn target_language(&self) -> &LanguageId {
        self.language_pair.target()
    }

    pub(crate) const fn source_snapshot_fingerprint(&self) -> SourceSnapshotFingerprint {
        self.source_snapshot_fingerprint
    }

    pub(crate) fn layout_profile(&self) -> &RpgMakerWriteBackLayoutProfile {
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

    pub(crate) fn stale_owners(&self) -> Vec<RpgMakerStandardAssetOwner> {
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
        query_ids: Vec<String>,
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
                query_ids,
                source,
            } => write!(
                formatter,
                "检查项目数据库 {} 的{stage}失败（查询 {}）：{source}",
                path.display(),
                query_ids.join(",")
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
///
/// 调用方必须在检查前取得项目命令租约，并持有到检查或收敛返回明确终态，避免同一
/// 项目的 ATT 命令在多次严格读取与最终 CAS 之间改变权威状态。
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

async fn read_inspection_snapshot<Q>(
    queries: &Q,
    database_path: &Path,
    stage: &'static str,
    requested: Vec<SqliteQuery>,
) -> Result<Vec<Vec<SqliteRow>>, ProjectDatabaseInspectionError<Q::Error>>
where
    Q: SqliteQueryExecutor,
{
    let query_ids = requested
        .iter()
        .map(|query| query.id().to_owned())
        .collect();
    queries
        .query_existing_database_snapshot(database_path.to_path_buf(), requested)
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
                    query_ids,
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
    let snapshot = read_inspection_snapshot(
        queries,
        &database_path,
        "读取项目数据库一致快照",
        vec![
            SqliteQuery::new(SELECT_SCHEMA_VERSION, Vec::new())
                .with_id("project_database.inspect.schema_version"),
            SqliteQuery::new(SELECT_MANAGED_SCHEMA, Vec::new())
                .with_id("project_database.inspect.managed_schema"),
            SqliteQuery::new(SELECT_METADATA, Vec::new())
                .with_id("project_database.inspect.metadata"),
            SqliteQuery::new(SELECT_OWNER_STATES, Vec::new())
                .with_id("project_database.inspect.owner_states"),
            SqliteQuery::new(SELECT_TRANSLATION_RESOURCES, Vec::new())
                .with_id("project_database.inspect.translation_resources"),
            SqliteQuery::new(SELECT_PROJECT_DEFINITIONS, Vec::new())
                .with_id("project_database.inspect.project_definitions"),
            SqliteQuery::new(SELECT_RUN_PLAN_SINGLETONS, Vec::new())
                .with_id("project_database.inspect.run_plan_singletons"),
            SqliteQuery::new(SELECT_LUA_PROGRAMS, Vec::new())
                .with_id("project_database.inspect.lua_programs"),
            SqliteQuery::new(SELECT_QUICK_CHECK, Vec::new())
                .with_id("project_database.inspect.quick_check"),
            SqliteQuery::new(SELECT_FOREIGN_KEY_CHECK, Vec::new())
                .with_id("project_database.inspect.foreign_key_check"),
        ],
    )
    .await?;

    let [
        schema_version_rows,
        schema,
        metadata_rows,
        owner_rows,
        translation_resource_rows,
        project_definition_rows,
        run_plan_singletons,
        lua_programs,
        quick_check,
        foreign_key_check,
    ] = <[Vec<SqliteRow>; 10]>::try_from(snapshot).map_err(|results| {
        ProjectDatabaseInspectionError::InvalidDatabase {
            path: database_path.clone(),
            reason: InvalidCurrentProjectDatabase::ManagedSchema(
                InvalidManagedSchema::WrongRowCount {
                    query: "inspection_snapshot_result_sets",
                    expected: 10,
                    actual: results.len(),
                },
            ),
        }
    })?;

    let schema_version = decode_schema_version(schema_version_rows).map_err(|reason| {
        ProjectDatabaseInspectionError::InvalidDatabase {
            path: database_path.clone(),
            reason,
        }
    })?;
    validate_managed_schema(schema).map_err(|reason| {
        ProjectDatabaseInspectionError::InvalidDatabase {
            path: database_path.clone(),
            reason,
        }
    })?;

    let metadata = metadata_facts_from_rows(expected_name, metadata_rows).map_err(|reason| {
        ProjectDatabaseInspectionError::InvalidDatabase {
            path: database_path.clone(),
            reason: InvalidCurrentProjectDatabase::Metadata(reason),
        }
    })?;
    let owners = decode_owner_states(owner_rows).map_err(|reason| {
        ProjectDatabaseInspectionError::InvalidDatabase {
            path: database_path.clone(),
            reason,
        }
    })?;
    let (terminology_json, placeholder_rules_json) =
        decode_translation_resources(translation_resource_rows).map_err(|reason| {
            ProjectDatabaseInspectionError::InvalidDatabase {
                path: database_path.clone(),
                reason,
            }
        })?;
    let mv_dialogue_rules_json =
        decode_project_definitions(project_definition_rows).map_err(|reason| {
            ProjectDatabaseInspectionError::InvalidDatabase {
                path: database_path.clone(),
                reason,
            }
        })?;
    let run_plans =
        decode_project_run_plans(run_plan_singletons, lua_programs).map_err(|reason| {
            ProjectDatabaseInspectionError::InvalidDatabase {
                path: database_path.clone(),
                reason: InvalidCurrentProjectDatabase::RunPlans(reason),
            }
        })?;

    validate_integrity(quick_check, foreign_key_check).map_err(|reason| {
        ProjectDatabaseInspectionError::InvalidDatabase {
            path: database_path.clone(),
            reason,
        }
    })?;

    Ok(ProjectDatabaseState {
        metadata,
        owners,
        terminology_json,
        placeholder_rules_json,
        mv_dialogue_rules_json,
        run_plans,
        schema_version,
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
            SqliteValue::Text(state.metadata.language_pair.source().as_str().to_owned()),
            SqliteValue::Text(state.metadata.language_pair.target().as_str().to_owned()),
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
            let source_fingerprint_parameter = owner_parameter + 1;
            let asset_fingerprint_parameter = source_fingerprint_parameter + 1;
            statement.push_str(&format!(
                "(owner = ?{owner_parameter} AND source_snapshot_fingerprint = ?{source_fingerprint_parameter} AND asset_snapshot_fingerprint = ?{asset_fingerprint_parameter})"
            ));
            parameters.push(SqliteValue::Text(owner.owner.storage_name().to_owned()));
            parameters.push(SqliteValue::Blob(
                owner.source_snapshot_fingerprint.as_bytes().to_vec(),
            ));
            parameters.push(SqliteValue::Blob(
                owner.asset_snapshot_fingerprint.as_bytes().to_vec(),
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

fn project_definitions_cas(state: &ProjectDatabaseState) -> SqliteTransactionStep {
    SqliteTransactionStep::RequireNoRows(SqliteQuery::new(
        r#"SELECT 1
WHERE (SELECT COUNT(*) FROM standard_project_definition) <> 1
   OR NOT EXISTS (
     SELECT 1 FROM standard_project_definition
     WHERE definition_kind = 'mv_dialogue_rules' AND canonical_json = ?1
   )"#,
        vec![SqliteValue::Text(state.mv_dialogue_rules_json.clone())],
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
    let language_changed = current.metadata.language_pair != requested.language_pair;
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
        project_definitions_cas(&current),
    ];
    if language_changed {
        for statement in [CLEAR_STANDARD_TEXT_TRANSLATIONS, RESET_TERMINOLOGY_RESOURCE] {
            steps.push(SqliteTransactionStep::Execute(SqliteCommand::new(
                statement,
                Vec::new(),
            )));
        }
    }
    steps.push(SqliteTransactionStep::Execute(SqliteCommand::new(
        UPDATE_METADATA,
        vec![
            SqliteValue::Text(requested.language_pair.source().as_str().to_owned()),
            SqliteValue::Text(requested.language_pair.target().as_str().to_owned()),
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
            ExecuteTransactionError::RequirementFailed
            | ExecuteTransactionError::RequirementFailedWithRow { .. } => {
                ProjectDatabaseReconciliationError::ConcurrentModification {
                    path: database_path.clone(),
                }
            }
            ExecuteTransactionError::RequirementFailedWithRowOutcomeUnknown { source, .. } => {
                ProjectDatabaseReconciliationError::OutcomeUnknown {
                    path: database_path.clone(),
                    source: *source,
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
            language_pair: requested.language_pair,
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
        mv_dialogue_rules_json: current.mv_dialogue_rules_json,
        run_plans: current.run_plans,
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
        CREATE_INIT_RUN_PLAN_TABLE,
        CREATE_EXTRACT_RUN_PLAN_TABLE,
        CREATE_EXTRACT_RULES_DEFINITION_TABLE,
        CREATE_TRANSLATE_RUN_PLAN_TABLE,
        CREATE_WRITE_BACK_RUN_PLAN_TABLE,
        CREATE_LUA_PROGRAM_TABLE,
        CREATE_STANDARD_ASSET_OWNER_STATE_TABLE,
        CREATE_STANDARD_TEXT_GROUP_TABLE,
        CREATE_STANDARD_TEXT_UNIT_TABLE,
        CREATE_STANDARD_MUTATION_CLAIM_TABLE,
        CREATE_STANDARD_MUTATION_CLAIM_OWNER_RESOURCE_INDEX,
        CREATE_STANDARD_MUTATION_CLAIM_RESOURCE_INDEX,
        CREATE_STANDARD_TRANSLATION_RESOURCE_TABLE,
        CREATE_STANDARD_PROJECT_DEFINITION_TABLE,
    ]
    .into_iter()
    .map(|statement| SqliteCommand::new(statement, Vec::new()))
    .collect::<Vec<_>>();
    commands.push(SqliteCommand::new(
        INSERT_METADATA,
        vec![
            SqliteValue::Text(project.name().as_str().to_owned()),
            SqliteValue::Text(project.source_language().as_str().to_owned()),
            SqliteValue::Text(project.target_language().as_str().to_owned()),
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
    commands.push(SqliteCommand::new(
        INSERT_STANDARD_PROJECT_DEFINITION,
        vec![
            SqliteValue::Text(MV_DIALOGUE_RULES_DEFINITION_KIND.to_owned()),
            SqliteValue::Text(r#"{"rules":[]}"#.to_owned()),
        ],
    ));
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
        let layout = ProjectWorkspaceLayout::for_project(
            Path::new("C:/att/projects"),
            RpgMakerLayout::MZ,
            &name,
        );

        assert_eq!(
            layout.workspace_root(),
            Path::new("C:/att/projects/mz/测试 游戏")
        );
        assert_eq!(
            layout.database_path(),
            Path::new("C:/att/projects/mz/测试 游戏/project.db")
        );
        assert_eq!(
            layout.source_root(),
            Path::new("C:/att/projects/mz/测试 游戏/source")
        );
        assert_eq!(
            layout.source_data(),
            Path::new("C:/att/projects/mz/测试 游戏/source/data")
        );
        assert_eq!(
            layout.source_js(),
            Path::new("C:/att/projects/mz/测试 游戏/source/js")
        );
        assert_eq!(
            layout.write_back_root(),
            Path::new("C:/att/projects/mz/测试 游戏/write_back")
        );
        assert_eq!(
            layout.write_back_data(),
            Path::new("C:/att/projects/mz/测试 游戏/write_back/data")
        );
        assert_eq!(
            layout.write_back_js(),
            Path::new("C:/att/projects/mz/测试 游戏/write_back/js")
        );

        let mv = ProjectWorkspaceLayout::for_project(
            Path::new("C:/att/projects"),
            RpgMakerLayout::MV,
            &name,
        );
        assert_eq!(
            mv.source_data(),
            Path::new("C:/att/projects/mv/测试 游戏/source/www/data")
        );
        assert_eq!(
            mv.write_back_js(),
            Path::new("C:/att/projects/mv/测试 游戏/write_back/www/js")
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
            language_pair("JA", "zh-cn"),
            test_source_snapshot_fingerprint(),
            layout_profile(),
        )
    }

    fn language_id(value: &str) -> LanguageId {
        LanguageId::parse(value).expect("test language ID should be valid")
    }

    fn language_pair(source: &str, target: &str) -> LanguagePair {
        LanguagePair::new(language_id(source), language_id(target))
    }

    fn test_source_snapshot_fingerprint() -> SourceSnapshotFingerprint {
        SourceSnapshotFingerprint::from_bytes([0x5a; 32])
    }

    fn width(value: u32) -> MaxFullwidthChars {
        MaxFullwidthChars::new(value).expect("test width should be positive")
    }

    fn layout_profile() -> RpgMakerWriteBackLayoutProfile {
        RpgMakerWriteBackLayoutProfile::new(width(24), width(30), width(18))
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
        assert_eq!(invocation.commands.len(), 19);
        assert_eq!(invocation.commands[0].statement(), CREATE_METADATA_TABLE);
        assert!(invocation.commands[0].parameters().is_empty());
        assert_eq!(
            invocation.commands[1].statement(),
            CREATE_INIT_RUN_PLAN_TABLE
        );
        assert_eq!(
            invocation.commands[2].statement(),
            CREATE_EXTRACT_RUN_PLAN_TABLE
        );
        assert_eq!(
            invocation.commands[3].statement(),
            CREATE_EXTRACT_RULES_DEFINITION_TABLE
        );
        assert_eq!(
            invocation.commands[4].statement(),
            CREATE_TRANSLATE_RUN_PLAN_TABLE
        );
        assert_eq!(
            invocation.commands[5].statement(),
            CREATE_WRITE_BACK_RUN_PLAN_TABLE
        );
        assert_eq!(invocation.commands[6].statement(), CREATE_LUA_PROGRAM_TABLE);
        assert_eq!(
            invocation.commands[7].statement(),
            CREATE_STANDARD_ASSET_OWNER_STATE_TABLE
        );
        assert_eq!(
            invocation.commands[8].statement(),
            CREATE_STANDARD_TEXT_GROUP_TABLE
        );
        assert_eq!(
            invocation.commands[9].statement(),
            CREATE_STANDARD_TEXT_UNIT_TABLE
        );
        assert_eq!(
            invocation.commands[10].statement(),
            CREATE_STANDARD_MUTATION_CLAIM_TABLE
        );
        assert_eq!(invocation.commands[15].statement(), INSERT_METADATA);
        assert_eq!(
            invocation.commands[15].parameters(),
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
            invocation.commands[16].parameters(),
            &[
                SqliteValue::Text(TERMINOLOGY_RESOURCE_KIND.to_owned()),
                SqliteValue::Text("[]".to_owned()),
            ]
        );
        assert_eq!(
            invocation.commands[17].parameters(),
            &[
                SqliteValue::Text(PLACEHOLDER_RULES_RESOURCE_KIND.to_owned()),
                SqliteValue::Text("[]".to_owned()),
            ]
        );
        assert_eq!(
            invocation.commands[18].parameters(),
            &[
                SqliteValue::Text(MV_DIALOGUE_RULES_DEFINITION_KIND.to_owned()),
                SqliteValue::Text(r#"{"rules":[]}"#.to_owned()),
            ]
        );
        assert!(
            invocation.commands[8]
                .statement()
                .contains("PRIMARY KEY (owner, group_location)")
        );
        assert!(
            invocation.commands[9]
                .statement()
                .contains("source_context_json")
        );
        assert!(
            invocation.commands[9]
                .statement()
                .contains("translation_state")
        );
        assert!(
            invocation.commands[10]
                .statement()
                .contains("resource_key   TEXT NOT NULL")
        );
        assert!(
            invocation.commands[7]
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
            CREATE_INIT_RUN_PLAN_TABLE,
            CREATE_EXTRACT_RUN_PLAN_TABLE,
            CREATE_EXTRACT_RULES_DEFINITION_TABLE,
            CREATE_TRANSLATE_RUN_PLAN_TABLE,
            CREATE_WRITE_BACK_RUN_PLAN_TABLE,
            CREATE_LUA_PROGRAM_TABLE,
            CREATE_STANDARD_ASSET_OWNER_STATE_TABLE,
            CREATE_STANDARD_TEXT_GROUP_TABLE,
            CREATE_STANDARD_TEXT_UNIT_TABLE,
            CREATE_STANDARD_MUTATION_CLAIM_TABLE,
            CREATE_STANDARD_MUTATION_CLAIM_OWNER_RESOURCE_INDEX,
            CREATE_STANDARD_MUTATION_CLAIM_RESOURCE_INDEX,
            CREATE_STANDARD_TRANSLATION_RESOURCE_TABLE,
            CREATE_STANDARD_PROJECT_DEFINITION_TABLE,
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
                "CREATE TRIGGER injected_unit_trigger AFTER INSERT ON standard_text_unit BEGIN SELECT 1; END",
            )
            .expect("测试触发器应可创建");
        assert!(matches!(
            validate_managed_schema(read_managed_schema(&connection)),
            Err(InvalidCurrentProjectDatabase::ManagedSchema(_))
        ));

        connection
            .execute(
                "INSERT INTO extract_run_plan (singleton, builtin_enabled, rules_enabled, lua_enabled) VALUES (1, 0, 0, 0)",
                [],
            )
            .expect_err("空 Extract owner 集合不得持久化");
        connection
            .execute(
                "INSERT INTO extract_rules_definition (singleton, canonical_json) VALUES (1, '[]')",
                [],
            )
            .expect_err("空 Rules 集合表示停用，不得保存为 active 定义");
        connection
            .execute(
                "INSERT INTO extract_rules_definition (singleton, canonical_json) VALUES (1, ?1)",
                rusqlite::params![br#"[{"file":"Actors.json","path":"[].name"}]"#.to_vec()],
            )
            .expect_err("Rules canonical_json 的 BLOB 伪装不得通过 TEXT schema 约束");
        connection
            .execute(
                "INSERT INTO translate_run_plan (singleton, profile_id) VALUES (1, ?1)",
                rusqlite::params![b"quality".to_vec()],
            )
            .expect_err("Profile ID 的 BLOB 伪装不得通过 TEXT schema 约束");
        connection
            .execute(
                "INSERT INTO lua_program (phase, source, source_sha256, resolved_path_utf16) VALUES ('extract', ?1, ?2, ?3)",
                rusqlite::params![Vec::<u8>::new(), vec![0_u8; 32], vec![0x43_u8, 0_u8]],
            )
            .expect_err("零字节 Lua 应由命令语义清除，不能进入快照表");

        let insert_group = "INSERT INTO standard_text_group (owner, group_location, group_order, group_kind, projection_recipe_json) VALUES (?1, ?2, 0, 'database_entry', '[]')";
        let insert_unit = "INSERT INTO standard_text_unit (owner, group_location, unit_role, unit_order, source_content_json, source_context_json, translation_content_json, translation_state) VALUES (?1, ?2, ?3, ?4, ?5, '{}', ?6, ?7)";
        connection
            .execute(insert_group, rusqlite::params!["builtin", "group-a",])
            .expect_err("没有 owner state 的资产必须被外键拒绝");

        for owner in ["builtin", "rules", "lua"] {
            connection
                .execute(
                    "INSERT INTO standard_asset_owner_state (owner, source_snapshot_fingerprint, asset_snapshot_fingerprint) VALUES (?1, ?2, ?3)",
                    rusqlite::params![owner, vec![0x5a_u8; 32], vec![0x6b_u8; 32]],
                )
                .expect("三个当前 owner 编码都应合法");
        }

        connection
            .execute(insert_group, rusqlite::params!["builtin", "group-a",])
            .expect("owner 可以保存文本组");
        connection
            .execute(
                insert_unit,
                rusqlite::params![
                    "builtin",
                    "group-a",
                    "scalar:name",
                    0,
                    r#""original""#,
                    Option::<String>::None,
                    Option::<Vec<u8>>::None,
                ],
            )
            .expect("未翻译语义单元应可保存");
        connection
            .execute(
                insert_unit,
                rusqlite::params![
                    "builtin",
                    "group-a",
                    "scalar:description",
                    1,
                    r#""original""#,
                    r#""译文""#,
                    Option::<Vec<u8>>::None,
                ],
            )
            .expect_err("译文与语义单元状态必须成对保存");
        connection
            .execute(
                insert_unit,
                rusqlite::params![
                    "builtin",
                    "group-a",
                    "choices",
                    2,
                    r#"["是","否"]"#,
                    r#""合并译文""#,
                    vec![0x7c_u8; 32],
                ],
            )
            .expect_err("译文内容形状必须与源内容一致");
        connection
            .execute(
                insert_unit,
                rusqlite::params![
                    "builtin",
                    "group-a",
                    "choices",
                    2,
                    r#"["是","否"]"#,
                    r#"["Yes","No"]"#,
                    vec![0x7c_u8; 32],
                ],
            )
            .expect("有序行集合应以 JSON 数组保存");
        connection
            .execute(
                insert_unit,
                rusqlite::params![
                    "builtin",
                    "group-a",
                    "scalar:name",
                    3,
                    r#""original""#,
                    Option::<String>::None,
                    Option::<Vec<u8>>::None,
                ],
            )
            .expect_err("同一组不能重复保存同一逻辑角色");

        connection
            .execute(
                "INSERT INTO standard_mutation_claim (owner, group_location, resource_key, access) VALUES ('builtin', 'group-a', 'value:shared', 'intent')",
                [],
            )
            .expect("第一个物理资源锁应可保存");
        connection
            .execute(insert_group, rusqlite::params!["rules", "group-b"])
            .expect("第二个 owner 的组应可保存");
        connection
            .execute(
                "INSERT INTO standard_mutation_claim (owner, group_location, resource_key, access) VALUES ('rules', 'group-b', 'value:shared', 'intent')",
                [],
            )
            .expect("两个 owner 的 Intent 锁可以共存，跨 owner 冲突由 Store 事务判定");
    }

    #[test]
    fn owner_state_requires_distinct_source_and_asset_fingerprints() {
        let valid = decode_owner_states(vec![SqliteRow::new(vec![
            SqliteValue::Text("builtin".to_owned()),
            SqliteValue::Blob(vec![0x11; 32]),
            SqliteValue::Blob(vec![0x22; 32]),
        ])])
        .expect("两个合法指纹应重建 owner state");

        assert_eq!(valid.len(), 1);
        assert_eq!(valid[0].source_snapshot_fingerprint.as_bytes(), &[0x11; 32]);
        assert_eq!(valid[0].asset_snapshot_fingerprint.as_bytes(), &[0x22; 32]);
        assert!(matches!(
            decode_owner_states(vec![SqliteRow::new(vec![
                SqliteValue::Text("builtin".to_owned()),
                SqliteValue::Blob(vec![0x11; 32]),
                SqliteValue::Blob(vec![0x22; 31]),
            ])]),
            Err(InvalidCurrentProjectDatabase::OwnerState(_))
        ));
    }

    #[test]
    fn project_definition_requires_current_canonical_dialogue_definition() {
        let row = |json: &str| {
            vec![SqliteRow::new(vec![
                SqliteValue::Text(MV_DIALOGUE_RULES_DEFINITION_KIND.to_owned()),
                SqliteValue::Text(json.to_owned()),
            ])]
        };

        assert_eq!(
            decode_project_definitions(row(r#"{"rules":[]}"#)).expect("空对话定义应是有效当前定义"),
            r#"{"rules":[]}"#
        );
        for invalid in [
            "[]",
            r#"{ "rules": [] }"#,
            r#"{"rules":[{"pattern":""}]}"#,
            r#"{"rules":[{"pattern":"(?<text>.+)"}]}"#,
        ] {
            assert!(matches!(
                decode_project_definitions(row(invalid)),
                Err(InvalidCurrentProjectDatabase::ProjectDefinitions(_))
            ));
        }
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
        snapshots: Mutex<Vec<(PathBuf, Vec<SqliteQuery>)>>,
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
                snapshots: Mutex::new(Vec::new()),
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

        async fn query_existing_database_snapshot(
            &self,
            path: PathBuf,
            queries: Vec<SqliteQuery>,
        ) -> Result<Vec<Vec<SqliteRow>>, QueryExistingDatabaseError<Self::Error>> {
            self.snapshots
                .lock()
                .expect("query snapshots mutex should not be poisoned")
                .push((path.clone(), queries.clone()));
            let mut results = Vec::with_capacity(queries.len());
            for query in queries {
                results.push(self.query_existing_database(path.clone(), query).await?);
            }
            Ok(results)
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
                    SqliteValue::Blob(vec![0xa5; 32]),
                ]),
                SqliteRow::new(vec![
                    SqliteValue::Text("lua".to_owned()),
                    SqliteValue::Blob(vec![0x6b; 32]),
                    SqliteValue::Blob(vec![0xb6; 32]),
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
            Ok(vec![SqliteRow::new(vec![
                SqliteValue::Text(MV_DIALOGUE_RULES_DEFINITION_KIND.to_owned()),
                SqliteValue::Text(r#"{"rules":[]}"#.to_owned()),
            ])]),
            Ok(vec![SqliteRow::new(vec![
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
                SqliteValue::Null,
            ])]),
            Ok(Vec::new()),
            Ok(vec![SqliteRow::new(vec![SqliteValue::Text(
                "ok".to_owned(),
            )])]),
            Ok(Vec::new()),
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
        let response = response.map(|rows| {
            rows.into_iter()
                .map(|row| {
                    let mut values = row.into_values();
                    if values.len() == 7 {
                        values.push(SqliteValue::Text(r#"{"rules":[]}"#.to_owned()));
                    }
                    SqliteRow::new(values)
                })
                .collect()
        });
        ProjectDatabaseRecordReadingService::new(
            PathBuf::from("C:/att/projects"),
            RpgMakerLayout::MZ,
            RecordingQueryExecutor::responding_with(response),
        )
    }

    #[tokio::test]
    async fn strict_inspection_reads_all_project_facts_in_one_snapshot_and_domain_order() {
        let queries = RecordingQueryExecutor::responding_with_many(valid_inspection_responses());
        let transactions = RecordingTransactionExecutor::responding_with(Ok(()));
        let service = ProjectDatabaseStateReconciliationService::new(queries, transactions);
        let expected_name: ProjectName = "测试 游戏".parse().expect("项目名应合法");

        let state = service
            .inspect(PathBuf::from("C:/projects/demo/project.db"), expected_name)
            .await
            .expect("当前 schema 与项目事实应通过严格检查");

        assert_eq!(state.source_language().as_str(), "ja");
        assert_eq!(state.target_language().as_str(), "zh-Hans");
        assert_eq!(
            state.active_owner_freshness(),
            vec![
                StandardAssetOwnerFreshness {
                    owner: RpgMakerStandardAssetOwner::Builtin,
                    fresh: true,
                },
                StandardAssetOwnerFreshness {
                    owner: RpgMakerStandardAssetOwner::Lua,
                    fresh: false,
                },
            ]
        );
        assert_eq!(state.stale_owners(), vec![RpgMakerStandardAssetOwner::Lua]);
        let invocations = service
            .queries
            .invocations
            .lock()
            .expect("query invocations mutex should not be poisoned");
        assert_eq!(state.mv_dialogue_rules_json(), r#"{"rules":[]}"#);
        assert_eq!(invocations.len(), 10);
        let snapshots = service
            .queries
            .snapshots
            .lock()
            .expect("query snapshots mutex should not be poisoned");
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].0, PathBuf::from("C:/projects/demo/project.db"));
        assert_eq!(snapshots[0].1.len(), 10);
        assert_eq!(
            snapshots[0]
                .1
                .iter()
                .map(SqliteQuery::statement)
                .collect::<Vec<_>>(),
            vec![
                SELECT_SCHEMA_VERSION,
                SELECT_MANAGED_SCHEMA,
                SELECT_METADATA,
                SELECT_OWNER_STATES,
                SELECT_TRANSLATION_RESOURCES,
                SELECT_PROJECT_DEFINITIONS,
                SELECT_RUN_PLAN_SINGLETONS,
                SELECT_LUA_PROGRAMS,
                SELECT_QUICK_CHECK,
                SELECT_FOREIGN_KEY_CHECK,
            ]
        );
        assert_eq!(
            snapshots[0]
                .1
                .iter()
                .map(SqliteQuery::id)
                .collect::<Vec<_>>(),
            vec![
                "project_database.inspect.schema_version",
                "project_database.inspect.managed_schema",
                "project_database.inspect.metadata",
                "project_database.inspect.owner_states",
                "project_database.inspect.translation_resources",
                "project_database.inspect.project_definitions",
                "project_database.inspect.run_plan_singletons",
                "project_database.inspect.lua_programs",
                "project_database.inspect.quick_check",
                "project_database.inspect.foreign_key_check",
            ]
        );
    }

    #[tokio::test]
    async fn inspection_read_failure_preserves_database_path_stage_and_query_identity() {
        let queries = RecordingQueryExecutor::responding_with(Err(
            QueryExistingDatabaseError::QueryFailed(FakeDriverError("SQL_PARAMETER_SENTINEL")),
        ));
        let transactions = RecordingTransactionExecutor::responding_with(Ok(()));
        let service = ProjectDatabaseStateReconciliationService::new(queries, transactions);

        let error = service
            .inspect(
                PathBuf::from("C:/projects/demo/project.db"),
                "demo".parse().expect("项目名应合法"),
            )
            .await
            .expect_err("一致快照读取失败必须保留完整查询上下文");

        let ProjectDatabaseInspectionError::ReadDatabase {
            path,
            stage,
            query_ids,
            source,
        } = error
        else {
            panic!("应报告带上下文的数据库读取失败")
        };
        assert_eq!(path, PathBuf::from("C:/projects/demo/project.db"));
        assert_eq!(stage, "读取项目数据库一致快照");
        assert_eq!(
            query_ids,
            vec![
                "project_database.inspect.schema_version",
                "project_database.inspect.managed_schema",
                "project_database.inspect.metadata",
                "project_database.inspect.owner_states",
                "project_database.inspect.translation_resources",
                "project_database.inspect.project_definitions",
                "project_database.inspect.run_plan_singletons",
                "project_database.inspect.lua_programs",
                "project_database.inspect.quick_check",
                "project_database.inspect.foreign_key_check",
            ]
        );
        assert_eq!(source.0, "SQL_PARAMETER_SENTINEL");
    }

    #[tokio::test]
    async fn inspection_maps_quick_check_and_foreign_key_sets_in_snapshot_order() {
        let mut quick_check_responses = valid_inspection_responses();
        quick_check_responses[8] = Ok(vec![SqliteRow::new(vec![SqliteValue::Text(
            "corrupt".to_owned(),
        )])]);
        let service = ProjectDatabaseStateReconciliationService::new(
            RecordingQueryExecutor::responding_with_many(quick_check_responses),
            RecordingTransactionExecutor::responding_with(Ok(())),
        );
        let error = service
            .inspect(
                PathBuf::from("C:/projects/demo/project.db"),
                "测试 游戏".parse().expect("项目名应合法"),
            )
            .await
            .expect_err("quick_check 非 ok 必须按第九组结果报告");
        assert!(matches!(
            error,
            ProjectDatabaseInspectionError::InvalidDatabase {
                reason: InvalidCurrentProjectDatabase::Integrity(
                    InvalidProjectDatabaseIntegrity::QuickCheckFailed
                ),
                ..
            }
        ));

        let mut foreign_key_responses = valid_inspection_responses();
        foreign_key_responses[9] = Ok(vec![SqliteRow::new(vec![
            SqliteValue::Text("standard_text_unit".to_owned()),
            SqliteValue::Integer(1),
            SqliteValue::Text("standard_text_group".to_owned()),
            SqliteValue::Integer(0),
        ])]);
        let service = ProjectDatabaseStateReconciliationService::new(
            RecordingQueryExecutor::responding_with_many(foreign_key_responses),
            RecordingTransactionExecutor::responding_with(Ok(())),
        );
        let error = service
            .inspect(
                PathBuf::from("C:/projects/demo/project.db"),
                "测试 游戏".parse().expect("项目名应合法"),
            )
            .await
            .expect_err("foreign_key_check 非空必须按第十组结果报告");
        assert!(matches!(
            error,
            ProjectDatabaseInspectionError::InvalidDatabase {
                reason: InvalidCurrentProjectDatabase::Integrity(
                    InvalidProjectDatabaseIntegrity::ForeignKeyViolations { actual: 1 }
                ),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn reconciliation_rejects_noncanonical_language_metadata_without_writing() {
        let mut responses = valid_inspection_responses();
        responses[2] = Ok(vec![metadata_row(
            SqliteValue::Text("测试 游戏".to_owned()),
            SqliteValue::Text("en-us".to_owned()),
            SqliteValue::Text("zh-Hans".to_owned()),
            SqliteValue::Integer(24),
            SqliteValue::Integer(30),
            SqliteValue::Integer(18),
        )]);
        let queries = RecordingQueryExecutor::responding_with_many(responses);
        let transactions = RecordingTransactionExecutor::responding_with(Ok(()));
        let service = ProjectDatabaseStateReconciliationService::new(queries, transactions);
        let requested = NewProject::new(
            "测试 游戏".parse().expect("项目名应合法"),
            language_pair("en-US", "zh-Hans"),
            test_source_snapshot_fingerprint(),
            layout_profile(),
        );

        let error = service
            .reconcile(PathBuf::from("C:/projects/demo/project.db"), requested)
            .await
            .expect_err("非规范语言 metadata 必须在对账写入前失败");

        let ProjectDatabaseReconciliationError::Inspection(
            ProjectDatabaseInspectionError::InvalidDatabase {
                reason:
                    InvalidCurrentProjectDatabase::Metadata(
                        InvalidProjectMetadata::NonCanonicalLanguage {
                            column,
                            stored,
                            canonical,
                        },
                    ),
                ..
            },
        ) = error
        else {
            panic!("应报告项目 metadata 中的非规范语言")
        };
        assert_eq!(column, "source_language");
        assert_eq!(stored, "en-us");
        assert_eq!(canonical, "en-US");
        assert_eq!(
            service
                .queries
                .invocations
                .lock()
                .expect("查询记录锁不应中毒")
                .len(),
            10
        );
        assert!(
            service
                .transactions
                .plans
                .lock()
                .expect("事务记录锁不应中毒")
                .is_empty(),
            "非规范 metadata 不得触发修复或其他写事务"
        );
    }

    #[tokio::test]
    async fn reconciliation_rejects_invalid_language_metadata_without_writing() {
        let mut responses = valid_inspection_responses();
        responses[2] = Ok(vec![metadata_row(
            SqliteValue::Text("测试 游戏".to_owned()),
            SqliteValue::Text("en_US".to_owned()),
            SqliteValue::Text("zh-Hans".to_owned()),
            SqliteValue::Integer(24),
            SqliteValue::Integer(30),
            SqliteValue::Integer(18),
        )]);
        let queries = RecordingQueryExecutor::responding_with_many(responses);
        let transactions = RecordingTransactionExecutor::responding_with(Ok(()));
        let service = ProjectDatabaseStateReconciliationService::new(queries, transactions);
        let requested = NewProject::new(
            "测试 游戏".parse().expect("项目名应合法"),
            language_pair("en-US", "zh-Hans"),
            test_source_snapshot_fingerprint(),
            layout_profile(),
        );

        let error = service
            .reconcile(PathBuf::from("C:/projects/demo/project.db"), requested)
            .await
            .expect_err("非法语言 metadata 必须在对账写入前失败");

        assert!(matches!(
            error,
            ProjectDatabaseReconciliationError::Inspection(
                ProjectDatabaseInspectionError::InvalidDatabase {
                    reason: InvalidCurrentProjectDatabase::Metadata(
                        InvalidProjectMetadata::InvalidLanguage {
                            column: "source_language",
                            ..
                        }
                    ),
                    ..
                }
            )
        ));
        assert!(
            service
                .transactions
                .plans
                .lock()
                .expect("事务记录锁不应中毒")
                .is_empty(),
            "非法 metadata 不得触发修复或其他写事务"
        );
    }

    #[tokio::test]
    async fn reconciliation_clears_language_dependent_state_and_uses_cas_guards() {
        let queries = RecordingQueryExecutor::responding_with_many(valid_inspection_responses());
        let transactions = RecordingTransactionExecutor::responding_with(Ok(()));
        let service = ProjectDatabaseStateReconciliationService::new(queries, transactions);
        let requested = NewProject::new(
            "测试 游戏".parse().expect("项目名应合法"),
            language_pair("en", "zh-Hans"),
            SourceSnapshotFingerprint::from_bytes([0x7c; 32]),
            RpgMakerWriteBackLayoutProfile::new(width(26), width(32), width(20)),
        );

        let result = service
            .reconcile(PathBuf::from("C:/projects/demo/project.db"), requested)
            .await
            .expect("对账事务应提交");

        assert_eq!(
            result.stale_owners(),
            vec![
                RpgMakerStandardAssetOwner::Builtin,
                RpgMakerStandardAssetOwner::Lua
            ]
        );
        assert_eq!(result.state().source_language().as_str(), "en");
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
        let SqliteTransactionStep::RequireNoRows(schema_version_guard) = &steps[0] else {
            panic!("首个 CAS 必须复核一致快照读取到的 schema version")
        };
        assert_eq!(
            schema_version_guard.parameters(),
            &[SqliteValue::Integer(13)]
        );
        let check_count = steps
            .iter()
            .filter(|step| matches!(step, SqliteTransactionStep::RequireNoRows(_)))
            .count();
        assert_eq!(check_count, 5);
        let executed = steps
            .iter()
            .filter_map(|step| match step {
                SqliteTransactionStep::Execute(command) => Some(command.statement()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(executed.contains(&CLEAR_STANDARD_TEXT_TRANSLATIONS));
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
            language_pair("ja", "zh-Hans"),
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
            Path::new("C:/att/projects/mz/测试 游戏")
        );
        assert_eq!(
            record.database_path(),
            Path::new("C:/att/projects/mz/测试 游戏/project.db")
        );
        assert_eq!(
            record.layout().source_data(),
            Path::new("C:/att/projects/mz/测试 游戏/source/data")
        );
        assert_eq!(
            record.layout().source_js(),
            Path::new("C:/att/projects/mz/测试 游戏/source/js")
        );
        assert_eq!(record.source_language().as_str(), "ja");
        assert_eq!(record.target_language().as_str(), "zh-Hans");
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
            PathBuf::from("C:/att/projects/mz/测试 游戏/project.db")
        );
        assert_eq!(invocations[0].query.statement(), SELECT_PROJECT_RECORD);
        assert_eq!(invocations[0].query.id(), PROJECT_RECORD_QUERY_ID);
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
            Path::new("C:/att/projects/mz/demo/project.db")
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
            Path::new("C:/att/projects/mz/demo/project.db")
        );
        let ProjectDatabaseReadError::ReadDatabase {
            stage,
            query_id,
            source,
            ..
        } = &read_failure
        else {
            panic!("query failure should preserve read context")
        };
        assert_eq!(*stage, PROJECT_RECORD_READ_STAGE);
        assert_eq!(query_id, PROJECT_RECORD_QUERY_ID);
        assert_eq!(source.0, "query failed");
        assert_eq!(
            read_failure.source().map(ToString::to_string).as_deref(),
            Some("query failed")
        );
    }

    #[tokio::test]
    async fn dialogue_definition_failure_keeps_typed_position_without_definition_body() {
        const DEFINITION_BODY_SENTINEL: &str = "SECRET_DIALOGUE_DEFINITION_BODY";
        let mut values = valid_metadata_row().into_values();
        values.push(SqliteValue::Text(format!(
            "{{\n\"rules\": \"{DEFINITION_BODY_SENTINEL}\"\n}}"
        )));

        let error = record_reading_service(Ok(vec![SqliteRow::new(values)]))
            .read(&"测试 游戏".parse().expect("test name should be valid"))
            .await
            .expect_err("无效的对话定义必须被拒绝");

        let ProjectDatabaseReadError::InvalidMetadata { path, reason } = error else {
            panic!("应报告 metadata 中的定义错误")
        };
        assert_eq!(
            path,
            PathBuf::from("C:/att/projects/mz/测试 游戏/project.db")
        );
        let InvalidProjectMetadata::InvalidDialogueDefinition { stage, failure } = &reason else {
            panic!("应保留类型化的对话定义失败")
        };
        assert_eq!(*stage, ProjectDefinitionStage::Decode);
        let ProjectDefinitionFailure::InvalidJson {
            category,
            line,
            column,
        } = failure
        else {
            panic!("应保留 JSON 分类与行列")
        };
        assert_eq!(*category, SafeJsonErrorCategory::Data);
        assert_eq!(*line, 2);
        assert!(*column > 0);

        let fact = reason.safe_fact();
        assert!(fact.contains("metadata=invalid_dialogue_definition"));
        assert!(fact.contains("definition=mv_dialogue_rules"));
        assert!(fact.contains("stage=decode"));
        assert!(fact.contains("failure=invalid_json"));
        assert!(fact.contains("category=data"));
        assert!(fact.contains("line=2"));
        assert!(!fact.contains(DEFINITION_BODY_SENTINEL));
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
        assert_eq!(invocations[0].query.statement(), SELECT_PROJECT_RECORD);
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
                ExpectedInvalidMetadata::InvalidLanguage,
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
                ExpectedInvalidMetadata::InvalidLanguage,
            ),
            (
                metadata_row(
                    SqliteValue::Text("demo".to_owned()),
                    SqliteValue::Text("en-us".to_owned()),
                    SqliteValue::Text("zh".to_owned()),
                    SqliteValue::Integer(24),
                    SqliteValue::Integer(30),
                    SqliteValue::Integer(18),
                ),
                ExpectedInvalidMetadata::NonCanonicalLanguage,
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
        InvalidLanguage,
        NonCanonicalLanguage,
        InvalidLineWidth,
        InvalidSourceSnapshotFingerprintLength,
        InvalidDialogueDefinition,
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
                InvalidProjectMetadata::InvalidLanguage { .. } => Self::InvalidLanguage,
                InvalidProjectMetadata::NonCanonicalLanguage { .. } => Self::NonCanonicalLanguage,
                InvalidProjectMetadata::InvalidLineWidth { .. } => Self::InvalidLineWidth,
                InvalidProjectMetadata::InvalidSourceSnapshotFingerprintLength { .. } => {
                    Self::InvalidSourceSnapshotFingerprintLength
                }
                InvalidProjectMetadata::InvalidDialogueDefinition { .. } => {
                    Self::InvalidDialogueDefinition
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
