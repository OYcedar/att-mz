//! RPG Maker 项目数据库的创建、读取与状态收敛职责。

mod run_plan;

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};

use crate::fingerprint::{InvalidSha256FingerprintLength, Sha256Fingerprint};
use crate::json_diagnostic::JsonErrorCategory;
use crate::language::{LanguageId, LanguageIdError, LanguagePair};
use crate::project_name::ProjectName;
use crate::rpg_maker::RpgMakerLayout;
use crate::rpg_maker::asset::RpgMakerAssetOwner;
use crate::rpg_maker::asset_storage::rpg_maker_asset_owner_order;
use crate::rpg_maker::dialogue::{MvDialogueDefinition, MvDialogueDefinitionError};
use crate::storage::sqlite::{
    CreateDatabaseError, ExecuteTransactionError, QueryExistingDatabaseError, SqliteCommand,
    SqliteDatabaseCreator, SqliteQuery, SqliteQueryExecutor, SqliteRow, SqliteTransactionExecutor,
    SqliteTransactionPlan, SqliteTransactionStep, SqliteValue,
};

use run_plan::{
    CREATE_EXTRACT_RULES_DEFINITION_TABLE, CREATE_EXTRACT_RUN_PLAN_TABLE,
    CREATE_INIT_RUN_PLAN_TABLE, CREATE_TRANSLATE_RUN_PLAN_TABLE,
};
#[allow(
    unused_imports,
    reason = "项目数据库集中重导出运行方案 API，调用方按具体命令选择所需成员"
)]
pub(crate) use run_plan::{
    ExtractRulesCanonicalJson, ExtractRunPlan, FinalProjectRunPlanPersistenceService, InitRunPlan,
    InvalidProjectRunPlans, InvalidRunPlanValue, ProjectRunPlanFinalizer,
    ProjectRunPlanPersistenceService, ProjectRunPlanReadError, ProjectRunPlanReplaceError,
    ProjectRunPlanReplacement, ProjectRunPlanRepository, ProjectRunPlans,
    SELECT_RUN_PLAN_SINGLETONS, TranslateRunPlan, decode_project_run_plans,
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

const CREATE_RPG_MAKER_ASSET_OWNER_STATE_TABLE: &str = r#"CREATE TABLE rpg_maker_asset_owner_state (
    owner                       TEXT NOT NULL PRIMARY KEY CHECK (owner IN ('builtin', 'rules')),
    source_snapshot_fingerprint BLOB NOT NULL CHECK (
        typeof(source_snapshot_fingerprint) = 'blob'
        AND length(source_snapshot_fingerprint) = 32
    ),
    asset_snapshot_fingerprint BLOB NOT NULL CHECK (
        typeof(asset_snapshot_fingerprint) = 'blob'
        AND length(asset_snapshot_fingerprint) = 32
    )
)"#;

pub(crate) const RPG_MAKER_TEXT_GROUP_TABLE_NAME: &str = "rpg_maker_text_group";
pub(crate) const RPG_MAKER_TEXT_UNIT_TABLE_NAME: &str = "rpg_maker_text_unit";
pub(crate) const RPG_MAKER_MUTATION_CLAIM_TABLE_NAME: &str = "rpg_maker_mutation_claim";

const CREATE_RPG_MAKER_TEXT_GROUP_TABLE: &str = r#"CREATE TABLE rpg_maker_text_group (
    owner                  TEXT NOT NULL CHECK (owner IN ('builtin', 'rules')),
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
    FOREIGN KEY (owner) REFERENCES rpg_maker_asset_owner_state(owner) ON DELETE CASCADE
)"#;

const CREATE_RPG_MAKER_TEXT_UNIT_TABLE: &str = r#"CREATE TABLE rpg_maker_text_unit (
    owner                    TEXT NOT NULL CHECK (owner IN ('builtin', 'rules')),
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
        REFERENCES rpg_maker_text_group(owner, group_location) ON DELETE CASCADE,
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

const CREATE_RPG_MAKER_MUTATION_CLAIM_TABLE: &str = r#"CREATE TABLE rpg_maker_mutation_claim (
    owner          TEXT NOT NULL CHECK (owner IN ('builtin', 'rules')),
    group_location TEXT NOT NULL CHECK (length(group_location) > 0),
    resource_key   TEXT NOT NULL CHECK (length(resource_key) > 0),
    access         TEXT NOT NULL CHECK (access IN ('intent', 'exclusive')),
    PRIMARY KEY (owner, group_location, resource_key),
    FOREIGN KEY (owner, group_location)
        REFERENCES rpg_maker_text_group(owner, group_location) ON DELETE CASCADE
)"#;

pub(crate) const CREATE_RPG_MAKER_MUTATION_CLAIM_RESOURCE_INDEX: &str = "CREATE INDEX rpg_maker_mutation_claim_resource_idx ON rpg_maker_mutation_claim(resource_key, access, owner, group_location)";
pub(crate) const CREATE_RPG_MAKER_MUTATION_CLAIM_OWNER_RESOURCE_INDEX: &str = "CREATE INDEX rpg_maker_mutation_claim_owner_resource_idx ON rpg_maker_mutation_claim(owner, resource_key, access, group_location)";
pub(crate) const DROP_RPG_MAKER_MUTATION_CLAIM_RESOURCE_INDEX: &str =
    "DROP INDEX rpg_maker_mutation_claim_resource_idx";
pub(crate) const DROP_RPG_MAKER_MUTATION_CLAIM_OWNER_RESOURCE_INDEX: &str =
    "DROP INDEX rpg_maker_mutation_claim_owner_resource_idx";

pub(crate) const RPG_MAKER_TRANSLATION_RESOURCE_TABLE_NAME: &str = "rpg_maker_translation_resource";
pub(crate) const TERMINOLOGY_RESOURCE_KIND: &str = "terminology";
pub(crate) const PLACEHOLDER_RULES_RESOURCE_KIND: &str = "placeholder_rules";

const CREATE_RPG_MAKER_TRANSLATION_RESOURCE_TABLE: &str = r#"CREATE TABLE rpg_maker_translation_resource (
    resource_kind  TEXT NOT NULL PRIMARY KEY CHECK (
        resource_kind IN ('terminology', 'placeholder_rules')
    ),
    canonical_json TEXT NOT NULL CHECK (length(canonical_json) > 0)
)"#;

pub(crate) const RPG_MAKER_PROJECT_DEFINITION_TABLE_NAME: &str = "rpg_maker_project_definition";
pub(crate) const MV_DIALOGUE_RULES_DEFINITION_KIND: &str = "mv_dialogue_rules";

const CREATE_RPG_MAKER_PROJECT_DEFINITION_TABLE: &str = r#"CREATE TABLE rpg_maker_project_definition (
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
const INSERT_RPG_MAKER_TRANSLATION_RESOURCE: &str = r#"INSERT INTO rpg_maker_translation_resource (
    resource_kind,
    canonical_json
) VALUES (?, ?)"#;
const INSERT_RPG_MAKER_PROJECT_DEFINITION: &str = r#"INSERT INTO rpg_maker_project_definition (
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
JOIN rpg_maker_project_definition AS definition
  ON definition.definition_kind = 'mv_dialogue_rules'"#;

const SELECT_SCHEMA_VERSION: &str = "SELECT schema_version FROM pragma_schema_version";
const SELECT_ATT_SCHEMA: &str = r#"SELECT type, name, tbl_name, sql
FROM sqlite_schema
WHERE sql IS NOT NULL
  AND (
    tbl_name IN (
      'metadata',
      'init_run_plan',
      'extract_run_plan',
      'extract_rules_definition',
      'translate_run_plan',
      'rpg_maker_asset_owner_state',
      'rpg_maker_text_group',
      'rpg_maker_text_unit',
      'rpg_maker_mutation_claim',
      'rpg_maker_translation_resource',
      'rpg_maker_project_definition'
    )
    OR name IN (
      'rpg_maker_mutation_claim_resource_idx',
      'rpg_maker_mutation_claim_owner_resource_idx'
    )
  )
ORDER BY type, name"#;
const SELECT_OWNER_STATES: &str = r#"SELECT owner, source_snapshot_fingerprint, asset_snapshot_fingerprint
FROM rpg_maker_asset_owner_state
ORDER BY owner"#;
const SELECT_TRANSLATION_RESOURCES: &str = r#"SELECT resource_kind, canonical_json
FROM rpg_maker_translation_resource
ORDER BY resource_kind"#;
const SELECT_PROJECT_DEFINITIONS: &str = r#"SELECT definition_kind, canonical_json
FROM rpg_maker_project_definition
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
const CLEAR_RPG_MAKER_TEXT_TRANSLATIONS: &str = "UPDATE rpg_maker_text_unit SET translation_content_json = NULL, translation_state = NULL WHERE translation_content_json IS NOT NULL OR translation_state IS NOT NULL";
const RESET_TERMINOLOGY_RESOURCE: &str = r#"UPDATE rpg_maker_translation_resource
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

    pub(crate) fn hex(&self) -> String {
        self.0.hex()
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

/// RPG Maker 写回所使用的三个显示区域宽度。
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

    #[cfg(test)]
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
    /// 创建服务；项目工作区根目录由发行布局边界明确注入。
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
    max_fullwidth_chars_integer(value, column)
}

fn max_fullwidth_chars_integer(
    value: i64,
    column: &'static str,
) -> Result<MaxFullwidthChars, InvalidProjectMetadata> {
    let value = u32::try_from(value).map_err(|_| InvalidProjectMetadata::InvalidLineWidth {
        column,
        actual: value,
    })?;
    MaxFullwidthChars::new(value).map_err(|_| InvalidProjectMetadata::InvalidLineWidth {
        column,
        actual: i64::from(value),
    })
}

/// 用项目数据库读取器的同一规则校验直接 SQLite 连接返回的显示宽度。
///
/// Lua 适配器在事务提交前持有 `rusqlite` 行，必须保留 SQLite 实际存储类型，
/// 不能通过 `get::<i64>` 把 REAL 或 TEXT 隐式转换成看似合法的整数。
pub(crate) fn max_fullwidth_chars_from_rusqlite_value(
    value: rusqlite::types::ValueRef<'_>,
    column: &'static str,
) -> Result<MaxFullwidthChars, InvalidProjectMetadata> {
    let value = match value {
        rusqlite::types::ValueRef::Integer(value) => {
            return max_fullwidth_chars_integer(value, column);
        }
        rusqlite::types::ValueRef::Null => "NULL",
        rusqlite::types::ValueRef::Real(_) => "REAL",
        rusqlite::types::ValueRef::Text(_) => "TEXT",
        rusqlite::types::ValueRef::Blob(_) => "BLOB",
    };
    Err(InvalidProjectMetadata::WrongColumnType {
        column,
        expected: "INTEGER",
        actual: value,
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
    owner: RpgMakerAssetOwner,
    source_snapshot_fingerprint: SourceSnapshotFingerprint,
    asset_snapshot_fingerprint: AssetSnapshotFingerprint,
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
        owners: Vec<(RpgMakerAssetOwner, SourceSnapshotFingerprint)>,
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

    pub(crate) fn stale_owners(&self) -> Vec<RpgMakerAssetOwner> {
        let mut owners = self
            .owners
            .iter()
            .filter_map(|state| {
                (state.source_snapshot_fingerprint != self.metadata.source_snapshot_fingerprint)
                    .then_some(state.owner)
            })
            .collect::<Vec<_>>();
        owners.sort_by_key(|owner| rpg_maker_asset_owner_order(*owner));
        owners
    }
}

/// 项目数据库无法按当前唯一 schema 重建为受信事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum InvalidCurrentProjectDatabase {
    Schema(InvalidAttSchema),
    Metadata(InvalidProjectMetadata),
    OwnerState(InvalidOwnerState),
    TranslationResources(InvalidTranslationResources),
    ProjectDefinitions(InvalidProjectDefinitions),
    RunPlans(InvalidProjectRunPlans),
    Integrity(InvalidProjectDatabaseIntegrity),
}

/// 直接连接无法证明 RPG Maker 数据库采用当前唯一 ATT schema。
#[derive(Debug)]
pub(crate) enum CurrentAttSchemaValidationError {
    Cancelled,
    Read(rusqlite::Error),
    Invalid(InvalidCurrentProjectDatabase),
}

impl fmt::Display for CurrentAttSchemaValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("当前 ATT schema 校验已取消"),
            Self::Read(source) => write!(formatter, "读取当前 ATT schema 失败：{source}"),
            Self::Invalid(reason) => reason.fmt(formatter),
        }
    }
}

impl Error for CurrentAttSchemaValidationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Cancelled => None,
            Self::Read(source) => Some(source),
            Self::Invalid(reason) => Some(reason),
        }
    }
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
pub(crate) enum AttSchemaObject {
    Metadata,
    InitRunPlan,
    ExtractRunPlan,
    ExtractRulesDefinition,
    TranslateRunPlan,
    RpgMakerAssetOwnerState,
    RpgMakerTextGroup,
    RpgMakerTextUnit,
    RpgMakerMutationClaim,
    RpgMakerTranslationResource,
    RpgMakerProjectDefinition,
    RpgMakerMutationClaimOwnerResourceIndex,
    RpgMakerMutationClaimResourceIndex,
}

impl AttSchemaObject {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Metadata => "table:metadata",
            Self::InitRunPlan => "table:init_run_plan",
            Self::ExtractRunPlan => "table:extract_run_plan",
            Self::ExtractRulesDefinition => "table:extract_rules_definition",
            Self::TranslateRunPlan => "table:translate_run_plan",
            Self::RpgMakerAssetOwnerState => "table:rpg_maker_asset_owner_state",
            Self::RpgMakerTextGroup => "table:rpg_maker_text_group",
            Self::RpgMakerTextUnit => "table:rpg_maker_text_unit",
            Self::RpgMakerMutationClaim => "table:rpg_maker_mutation_claim",
            Self::RpgMakerTranslationResource => "table:rpg_maker_translation_resource",
            Self::RpgMakerProjectDefinition => "table:rpg_maker_project_definition",
            Self::RpgMakerMutationClaimOwnerResourceIndex => {
                "index:rpg_maker_mutation_claim_owner_resource_idx"
            }
            Self::RpgMakerMutationClaimResourceIndex => {
                "index:rpg_maker_mutation_claim_resource_idx"
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum InvalidAttSchema {
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
        missing: Vec<AttSchemaObject>,
        definition_mismatches: Vec<AttSchemaObject>,
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
        owner: RpgMakerAssetOwner,
        field: ProjectDatabaseField,
        expected: usize,
        actual: usize,
    },
    DuplicateOwner {
        owner: RpgMakerAssetOwner,
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

pub(crate) type SafeJsonErrorCategory = JsonErrorCategory;

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
            Self::Schema(reason) => {
                format!("database_component=att_schema; {}", reason.safe_fact())
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

impl InvalidAttSchema {
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
                "violation=att_schema_object_mismatch; expected_count={expected_count}; actual_count={actual_count}; missing={}; definition_mismatches={}; unexpected_count={unexpected_count}",
                att_schema_object_list(missing),
                att_schema_object_list(definition_mismatches)
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
                "violation=invalid_json; resource={}; json_category={}; line={line}; column={column}",
                resource.storage_name(),
                category.storage_name()
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
    pub(crate) fn safe_fact(&self) -> String {
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
            "failure=invalid_json; json_category={}; line={line}; column={column}",
            category.storage_name()
        ),
        ProjectDefinitionFailure::EncodeJson {
            category,
            line,
            column,
        } => format!(
            "failure=encode_json; json_category={}; line={line}; column={column}",
            category.storage_name()
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

fn att_schema_object_list(objects: &[AttSchemaObject]) -> String {
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
            Self::Schema(_)
            | Self::OwnerState(_)
            | Self::TranslationResources(_)
            | Self::ProjectDefinitions(_)
            | Self::Integrity(_) => None,
        }
    }
}

fn expected_att_schema() -> Vec<(&'static str, &'static str, &'static str, &'static str)> {
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
            "rpg_maker_asset_owner_state",
            "rpg_maker_asset_owner_state",
            CREATE_RPG_MAKER_ASSET_OWNER_STATE_TABLE,
        ),
        (
            "table",
            RPG_MAKER_TEXT_GROUP_TABLE_NAME,
            RPG_MAKER_TEXT_GROUP_TABLE_NAME,
            CREATE_RPG_MAKER_TEXT_GROUP_TABLE,
        ),
        (
            "table",
            RPG_MAKER_TEXT_UNIT_TABLE_NAME,
            RPG_MAKER_TEXT_UNIT_TABLE_NAME,
            CREATE_RPG_MAKER_TEXT_UNIT_TABLE,
        ),
        (
            "table",
            RPG_MAKER_MUTATION_CLAIM_TABLE_NAME,
            RPG_MAKER_MUTATION_CLAIM_TABLE_NAME,
            CREATE_RPG_MAKER_MUTATION_CLAIM_TABLE,
        ),
        (
            "table",
            RPG_MAKER_TRANSLATION_RESOURCE_TABLE_NAME,
            RPG_MAKER_TRANSLATION_RESOURCE_TABLE_NAME,
            CREATE_RPG_MAKER_TRANSLATION_RESOURCE_TABLE,
        ),
        (
            "table",
            RPG_MAKER_PROJECT_DEFINITION_TABLE_NAME,
            RPG_MAKER_PROJECT_DEFINITION_TABLE_NAME,
            CREATE_RPG_MAKER_PROJECT_DEFINITION_TABLE,
        ),
        (
            "index",
            "rpg_maker_mutation_claim_owner_resource_idx",
            RPG_MAKER_MUTATION_CLAIM_TABLE_NAME,
            CREATE_RPG_MAKER_MUTATION_CLAIM_OWNER_RESOURCE_INDEX,
        ),
        (
            "index",
            "rpg_maker_mutation_claim_resource_idx",
            RPG_MAKER_MUTATION_CLAIM_TABLE_NAME,
            CREATE_RPG_MAKER_MUTATION_CLAIM_RESOURCE_INDEX,
        ),
    ]
}

fn validate_att_schema(rows: Vec<SqliteRow>) -> Result<(), InvalidCurrentProjectDatabase> {
    match validate_att_schema_with_check(rows, &mut || false) {
        Ok(()) => Ok(()),
        Err(AttSchemaCheckError::Invalid(source)) => Err(source),
        Err(AttSchemaCheckError::Cancelled) => {
            unreachable!("永不取消的 schema 校验闭包不得返回取消")
        }
    }
}

enum AttSchemaCheckError {
    Cancelled,
    Invalid(InvalidCurrentProjectDatabase),
}

fn validate_att_schema_with_check(
    rows: Vec<SqliteRow>,
    is_cancelled: &mut impl FnMut() -> bool,
) -> Result<(), AttSchemaCheckError> {
    ensure_att_schema_running(is_cancelled)?;
    let mut actual = Vec::with_capacity(rows.len());
    for row in rows {
        ensure_att_schema_running(is_cancelled)?;
        let values = row.into_values();
        if values.len() != 4 {
            return Err(AttSchemaCheckError::Invalid(
                InvalidCurrentProjectDatabase::Schema(InvalidAttSchema::WrongColumnCount {
                    query: "att_schema",
                    expected: 4,
                    actual: values.len(),
                }),
            ));
        }
        let mut values = values.into_iter();
        let kind = schema_text(
            values.next().expect("已确认有四列"),
            ProjectDatabaseField::SchemaType,
        )
        .map_err(AttSchemaCheckError::Invalid)?;
        let name = schema_text(
            values.next().expect("已确认有四列"),
            ProjectDatabaseField::SchemaName,
        )
        .map_err(AttSchemaCheckError::Invalid)?;
        let table = schema_text(
            values.next().expect("已确认有四列"),
            ProjectDatabaseField::SchemaTableName,
        )
        .map_err(AttSchemaCheckError::Invalid)?;
        let sql = schema_text(
            values.next().expect("已确认有四列"),
            ProjectDatabaseField::SchemaSql,
        )
        .map_err(AttSchemaCheckError::Invalid)?;
        actual.push((kind, name, table, sql));
    }
    ensure_att_schema_running(is_cancelled)?;
    let expected = expected_att_schema();
    let mut missing = Vec::new();
    for (kind, name, _, _) in &expected {
        ensure_att_schema_running(is_cancelled)?;
        let mut found = false;
        for (actual_kind, actual_name, _, _) in &actual {
            ensure_att_schema_running(is_cancelled)?;
            if schema_text_eq(actual_kind, kind, is_cancelled)?
                && schema_text_eq(actual_name, name, is_cancelled)?
            {
                found = true;
                break;
            }
        }
        if !found && let Some(object) = att_schema_object(kind, name) {
            missing.push(object);
        }
    }
    let mut definition_mismatches = Vec::new();
    for (kind, name, table, sql) in &expected {
        ensure_att_schema_running(is_cancelled)?;
        for (actual_kind, actual_name, actual_table, actual_sql) in &actual {
            ensure_att_schema_running(is_cancelled)?;
            if !schema_text_eq(actual_kind, kind, is_cancelled)?
                || !schema_text_eq(actual_name, name, is_cancelled)?
            {
                continue;
            }
            if (!schema_text_eq(actual_table, table, is_cancelled)?
                || !schema_text_eq(actual_sql, sql, is_cancelled)?)
                && let Some(object) = att_schema_object(kind, name)
            {
                definition_mismatches.push(object);
            }
            break;
        }
    }
    let mut unexpected_count = 0;
    for (actual_kind, actual_name, _, _) in &actual {
        ensure_att_schema_running(is_cancelled)?;
        let mut found = false;
        for (kind, name, _, _) in &expected {
            ensure_att_schema_running(is_cancelled)?;
            if schema_text_eq(actual_kind, kind, is_cancelled)?
                && schema_text_eq(actual_name, name, is_cancelled)?
            {
                found = true;
                break;
            }
        }
        if !found {
            unexpected_count += 1;
        }
    }
    ensure_att_schema_running(is_cancelled)?;
    if actual.len() == expected.len()
        && missing.is_empty()
        && definition_mismatches.is_empty()
        && unexpected_count == 0
    {
        Ok(())
    } else {
        Err(AttSchemaCheckError::Invalid(
            InvalidCurrentProjectDatabase::Schema(InvalidAttSchema::ObjectMismatch {
                expected_count: expected.len(),
                actual_count: actual.len(),
                missing,
                definition_mismatches,
                unexpected_count,
            }),
        ))
    }
}

fn ensure_att_schema_running(
    is_cancelled: &mut impl FnMut() -> bool,
) -> Result<(), AttSchemaCheckError> {
    if is_cancelled() {
        Err(AttSchemaCheckError::Cancelled)
    } else {
        Ok(())
    }
}

fn schema_text_eq(
    left: &str,
    right: &str,
    is_cancelled: &mut impl FnMut() -> bool,
) -> Result<bool, AttSchemaCheckError> {
    ensure_att_schema_running(is_cancelled)?;
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left
        .as_bytes()
        .chunks(64 * 1024)
        .zip(right.as_bytes().chunks(64 * 1024))
    {
        ensure_att_schema_running(is_cancelled)?;
        if left != right {
            return Ok(false);
        }
    }
    ensure_att_schema_running(is_cancelled)?;
    Ok(true)
}

/// 在调用方已经打开的连接上校验当前唯一 RPG Maker ATT schema。
///
/// 查询只选择 ATT 管理的对象，因此脚本私有表、索引和触发器不属于本契约。
#[cfg(test)]
pub(crate) fn validate_current_att_schema(
    connection: &rusqlite::Connection,
) -> Result<(), CurrentAttSchemaValidationError> {
    validate_current_att_schema_with_cancellation(connection, || false)
}

pub(crate) fn validate_current_att_schema_with_cancellation(
    connection: &rusqlite::Connection,
    mut is_cancelled: impl FnMut() -> bool,
) -> Result<(), CurrentAttSchemaValidationError> {
    if is_cancelled() {
        return Err(CurrentAttSchemaValidationError::Cancelled);
    }
    let mut statement = connection
        .prepare(SELECT_ATT_SCHEMA)
        .map_err(|source| current_att_schema_read_error(source, &mut is_cancelled))?;
    if is_cancelled() {
        return Err(CurrentAttSchemaValidationError::Cancelled);
    }
    let mut query = statement
        .query([])
        .map_err(|source| current_att_schema_read_error(source, &mut is_cancelled))?;
    let mut rows = Vec::new();
    loop {
        if is_cancelled() {
            return Err(CurrentAttSchemaValidationError::Cancelled);
        }
        let Some(row) = query
            .next()
            .map_err(|source| current_att_schema_read_error(source, &mut is_cancelled))?
        else {
            break;
        };
        rows.push(SqliteRow::new(vec![
            SqliteValue::Text(clone_current_att_schema_text(
                row,
                0,
                "type",
                &mut is_cancelled,
            )?),
            SqliteValue::Text(clone_current_att_schema_text(
                row,
                1,
                "name",
                &mut is_cancelled,
            )?),
            SqliteValue::Text(clone_current_att_schema_text(
                row,
                2,
                "tbl_name",
                &mut is_cancelled,
            )?),
            SqliteValue::Text(clone_current_att_schema_text(
                row,
                3,
                "sql",
                &mut is_cancelled,
            )?),
        ]));
    }
    drop(query);
    drop(statement);
    match validate_att_schema_with_check(rows, &mut is_cancelled) {
        Ok(()) => Ok(()),
        Err(AttSchemaCheckError::Cancelled) => Err(CurrentAttSchemaValidationError::Cancelled),
        Err(AttSchemaCheckError::Invalid(source)) => {
            Err(CurrentAttSchemaValidationError::Invalid(source))
        }
    }
}

fn clone_current_att_schema_text(
    row: &rusqlite::Row<'_>,
    index: usize,
    column: &'static str,
    is_cancelled: &mut impl FnMut() -> bool,
) -> Result<String, CurrentAttSchemaValidationError> {
    if is_cancelled() {
        return Err(CurrentAttSchemaValidationError::Cancelled);
    }
    let bytes = match row
        .get_ref(index)
        .map_err(|source| current_att_schema_read_error(source, is_cancelled))?
    {
        rusqlite::types::ValueRef::Text(bytes) => bytes,
        value => {
            let source =
                rusqlite::Error::InvalidColumnType(index, column.to_owned(), value.data_type());
            return Err(current_att_schema_read_error(source, is_cancelled));
        }
    };
    let mut text = String::with_capacity(bytes.len());
    let mut pending = Vec::with_capacity(64 * 1024 + 3);
    for chunk in bytes.chunks(64 * 1024) {
        if is_cancelled() {
            return Err(CurrentAttSchemaValidationError::Cancelled);
        }
        pending.extend_from_slice(chunk);
        match std::str::from_utf8(&pending) {
            Ok(valid) => {
                text.push_str(valid);
                pending.clear();
            }
            Err(source) if source.error_len().is_none() => {
                let valid_up_to = source.valid_up_to();
                let valid = std::str::from_utf8(&pending[..valid_up_to])
                    .expect("Utf8Error::valid_up_to 指向有效 UTF-8 前缀");
                text.push_str(valid);
                pending.copy_within(valid_up_to.., 0);
                pending.truncate(pending.len() - valid_up_to);
            }
            Err(source) => {
                let source = rusqlite::Error::FromSqlConversionFailure(
                    index,
                    rusqlite::types::Type::Text,
                    Box::new(source),
                );
                return Err(current_att_schema_read_error(source, is_cancelled));
            }
        }
    }
    if !pending.is_empty() {
        let source = std::str::from_utf8(&pending).expect_err("pending 只保留不完整 UTF-8 后缀");
        let source = rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(source),
        );
        return Err(current_att_schema_read_error(source, is_cancelled));
    }
    if is_cancelled() {
        Err(CurrentAttSchemaValidationError::Cancelled)
    } else {
        Ok(text)
    }
}

fn current_att_schema_read_error(
    source: rusqlite::Error,
    is_cancelled: &mut impl FnMut() -> bool,
) -> CurrentAttSchemaValidationError {
    if matches!(
        source.sqlite_error_code(),
        Some(rusqlite::ErrorCode::OperationInterrupted)
    ) || is_cancelled()
    {
        CurrentAttSchemaValidationError::Cancelled
    } else {
        CurrentAttSchemaValidationError::Read(source)
    }
}

fn schema_text(
    value: SqliteValue,
    field: ProjectDatabaseField,
) -> Result<String, InvalidCurrentProjectDatabase> {
    match value {
        SqliteValue::Text(value) => Ok(value),
        value => Err(InvalidCurrentProjectDatabase::Schema(
            InvalidAttSchema::WrongColumnType {
                field,
                expected: ProjectDatabaseValueKind::Text,
                actual: ProjectDatabaseValueKind::from(&value),
            },
        )),
    }
}

fn att_schema_object(kind: &str, name: &str) -> Option<AttSchemaObject> {
    match (kind, name) {
        ("table", "metadata") => Some(AttSchemaObject::Metadata),
        ("table", "init_run_plan") => Some(AttSchemaObject::InitRunPlan),
        ("table", "extract_run_plan") => Some(AttSchemaObject::ExtractRunPlan),
        ("table", "extract_rules_definition") => Some(AttSchemaObject::ExtractRulesDefinition),
        ("table", "translate_run_plan") => Some(AttSchemaObject::TranslateRunPlan),
        ("table", "rpg_maker_asset_owner_state") => Some(AttSchemaObject::RpgMakerAssetOwnerState),
        ("table", RPG_MAKER_TEXT_GROUP_TABLE_NAME) => Some(AttSchemaObject::RpgMakerTextGroup),
        ("table", RPG_MAKER_TEXT_UNIT_TABLE_NAME) => Some(AttSchemaObject::RpgMakerTextUnit),
        ("table", RPG_MAKER_MUTATION_CLAIM_TABLE_NAME) => {
            Some(AttSchemaObject::RpgMakerMutationClaim)
        }
        ("table", RPG_MAKER_TRANSLATION_RESOURCE_TABLE_NAME) => {
            Some(AttSchemaObject::RpgMakerTranslationResource)
        }
        ("table", RPG_MAKER_PROJECT_DEFINITION_TABLE_NAME) => {
            Some(AttSchemaObject::RpgMakerProjectDefinition)
        }
        ("index", "rpg_maker_mutation_claim_owner_resource_idx") => {
            Some(AttSchemaObject::RpgMakerMutationClaimOwnerResourceIndex)
        }
        ("index", "rpg_maker_mutation_claim_resource_idx") => {
            Some(AttSchemaObject::RpgMakerMutationClaimResourceIndex)
        }
        _ => None,
    }
}

fn decode_schema_version(rows: Vec<SqliteRow>) -> Result<i64, InvalidCurrentProjectDatabase> {
    let [row] = <[SqliteRow; 1]>::try_from(rows).map_err(|rows| {
        InvalidCurrentProjectDatabase::Schema(InvalidAttSchema::WrongRowCount {
            query: "schema_version",
            expected: 1,
            actual: rows.len(),
        })
    })?;
    let [value] = <[SqliteValue; 1]>::try_from(row.into_values()).map_err(|values| {
        InvalidCurrentProjectDatabase::Schema(InvalidAttSchema::WrongColumnCount {
            query: "schema_version",
            expected: 1,
            actual: values.len(),
        })
    })?;
    match value {
        SqliteValue::Integer(value) if value >= 0 => Ok(value),
        SqliteValue::Integer(actual) => Err(InvalidCurrentProjectDatabase::Schema(
            InvalidAttSchema::NegativeSchemaVersion { actual },
        )),
        value => Err(InvalidCurrentProjectDatabase::Schema(
            InvalidAttSchema::WrongColumnType {
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
            SqliteValue::Text(value) => RpgMakerAssetOwner::from_storage_name(&value).ok_or(
                InvalidCurrentProjectDatabase::OwnerState(InvalidOwnerState::UnknownOwner),
            )?,
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
    owners.sort_by_key(|state| rpg_maker_asset_owner_order(state.owner));
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
    if raw_definition_kind != MV_DIALOGUE_RULES_DEFINITION_KIND {
        return Err(InvalidCurrentProjectDatabase::ProjectDefinitions(
            InvalidProjectDefinitions::UnknownDefinitionKind,
        ));
    }
    let canonical_json = project_definition_text(
        values.next().expect("已确认有两列"),
        ProjectDatabaseField::CanonicalJson,
    )?;
    validate_mv_dialogue_definition_canonical_json(&canonical_json)
        .map_err(InvalidCurrentProjectDatabase::ProjectDefinitions)?;
    Ok(canonical_json)
}

/// 按当前唯一项目定义契约解析、编译并确认 MV 对话规则的规范 JSON。
///
/// 项目状态读取与 Lua 最终事务校验共用这个语义边界，避免脚本提交一个 schema
/// 合法、但 PCRE2 或命名捕获无效而无法再次打开的项目。
pub(crate) fn validate_mv_dialogue_definition_canonical_json(
    canonical_json: &str,
) -> Result<(), InvalidProjectDefinitions> {
    let definition_kind = ProjectDefinitionKind::MvDialogueRules;
    let definition =
        MvDialogueDefinition::from_canonical_json(canonical_json).map_err(|source| {
            invalid_project_definition(definition_kind, ProjectDefinitionStage::Decode, source)
        })?;
    definition.compile().map_err(|source| {
        invalid_project_definition(definition_kind, ProjectDefinitionStage::Compile, source)
    })?;
    let encoded = definition.to_canonical_json().map_err(|source| {
        invalid_project_definition(definition_kind, ProjectDefinitionStage::Encode, source)
    })?;
    if encoded != canonical_json {
        return Err(InvalidProjectDefinitions::NonCanonicalJson {
            definition: definition_kind,
        });
    }
    Ok(())
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
) -> InvalidProjectDefinitions {
    InvalidProjectDefinitions::InvalidDefinition {
        definition,
        stage,
        failure: project_definition_failure(source),
    }
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

    pub(crate) fn stale_owners(&self) -> Vec<RpgMakerAssetOwner> {
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
            SqliteQuery::new(SELECT_ATT_SCHEMA, Vec::new())
                .with_id("project_database.inspect.att_schema"),
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
        quick_check,
        foreign_key_check,
    ] = <[Vec<SqliteRow>; 9]>::try_from(snapshot).map_err(|results| {
        ProjectDatabaseInspectionError::InvalidDatabase {
            path: database_path.clone(),
            reason: InvalidCurrentProjectDatabase::Schema(InvalidAttSchema::WrongRowCount {
                query: "inspection_snapshot_result_sets",
                expected: 9,
                actual: results.len(),
            }),
        }
    })?;

    let schema_version = decode_schema_version(schema_version_rows).map_err(|reason| {
        ProjectDatabaseInspectionError::InvalidDatabase {
            path: database_path.clone(),
            reason,
        }
    })?;
    validate_att_schema(schema).map_err(|reason| {
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
    let run_plans = decode_project_run_plans(run_plan_singletons).map_err(|reason| {
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
        "SELECT 1 WHERE (SELECT COUNT(*) FROM rpg_maker_asset_owner_state) <> ?1".to_owned();
    let mut parameters = vec![SqliteValue::Integer(
        i64::try_from(state.owners.len()).expect("owner 数量固定小于 i64 上限"),
    )];
    if !state.owners.is_empty() {
        statement.push_str(" OR EXISTS (SELECT 1 FROM rpg_maker_asset_owner_state WHERE NOT (");
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
WHERE (SELECT COUNT(*) FROM rpg_maker_translation_resource) <> 2
   OR NOT EXISTS (
     SELECT 1 FROM rpg_maker_translation_resource
     WHERE resource_kind = 'terminology' AND canonical_json = ?1
   )
   OR NOT EXISTS (
     SELECT 1 FROM rpg_maker_translation_resource
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
WHERE (SELECT COUNT(*) FROM rpg_maker_project_definition) <> 1
   OR NOT EXISTS (
     SELECT 1 FROM rpg_maker_project_definition
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
        for statement in [
            CLEAR_RPG_MAKER_TEXT_TRANSLATIONS,
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
        CREATE_RPG_MAKER_ASSET_OWNER_STATE_TABLE,
        CREATE_RPG_MAKER_TEXT_GROUP_TABLE,
        CREATE_RPG_MAKER_TEXT_UNIT_TABLE,
        CREATE_RPG_MAKER_MUTATION_CLAIM_TABLE,
        CREATE_RPG_MAKER_MUTATION_CLAIM_OWNER_RESOURCE_INDEX,
        CREATE_RPG_MAKER_MUTATION_CLAIM_RESOURCE_INDEX,
        CREATE_RPG_MAKER_TRANSLATION_RESOURCE_TABLE,
        CREATE_RPG_MAKER_PROJECT_DEFINITION_TABLE,
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
            INSERT_RPG_MAKER_TRANSLATION_RESOURCE,
            vec![
                SqliteValue::Text(resource_kind.to_owned()),
                SqliteValue::Text("[]".to_owned()),
            ],
        ));
    }
    commands.push(SqliteCommand::new(
        INSERT_RPG_MAKER_PROJECT_DEFINITION,
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
    use super::*;

    fn language_pair(source: &str, target: &str) -> LanguagePair {
        LanguagePair::new(
            LanguageId::parse(source).expect("源语言应合法"),
            LanguageId::parse(target).expect("目标语言应合法"),
        )
    }

    fn layout_profile() -> RpgMakerWriteBackLayoutProfile {
        let width = |value| MaxFullwidthChars::new(value).expect("测试宽度应合法");
        RpgMakerWriteBackLayoutProfile::new(width(24), width(30), width(18))
    }

    fn project() -> NewProject {
        NewProject::new(
            "测试 游戏".parse().expect("项目名应合法"),
            language_pair("ja", "zh-Hans"),
            SourceSnapshotFingerprint::from_bytes([0x5a; 32]),
            layout_profile(),
        )
    }

    #[test]
    fn workspace_layout_derives_every_fixed_location_from_one_root() {
        let name: ProjectName = "测试 游戏".parse().expect("项目名应合法");
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
            layout.write_back_root(),
            Path::new("C:/att/projects/mz/测试 游戏/write_back")
        );
    }

    #[test]
    fn creation_plan_contains_the_complete_current_rpg_maker_schema_and_resources() {
        let commands = project_database_commands(&project());
        assert_eq!(commands.len(), 17);
        let statements = commands
            .iter()
            .map(SqliteCommand::statement)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(statements.contains("rpg_maker_text_group"));
        assert!(statements.contains("rpg_maker_translation_resource"));
        assert_eq!(
            expected_att_schema()
                .iter()
                .map(|(_, name, _, _)| *name)
                .collect::<Vec<_>>(),
            [
                "metadata",
                "init_run_plan",
                "extract_run_plan",
                "extract_rules_definition",
                "translate_run_plan",
                "rpg_maker_asset_owner_state",
                "rpg_maker_text_group",
                "rpg_maker_text_unit",
                "rpg_maker_mutation_claim",
                "rpg_maker_translation_resource",
                "rpg_maker_project_definition",
                "rpg_maker_mutation_claim_owner_resource_idx",
                "rpg_maker_mutation_claim_resource_idx",
            ]
        );
    }

    #[test]
    fn current_schema_matches_the_protected_att_objects_and_allows_private_tables() {
        let connection = rusqlite::Connection::open_in_memory().expect("应可打开内存数据库");
        connection
            .pragma_update(None, "foreign_keys", true)
            .expect("应可启用外键");
        for (_, _, _, sql) in expected_att_schema() {
            connection
                .execute_batch(sql)
                .unwrap_or_else(|error| panic!("当前 schema 应可执行：{error}"));
        }
        connection
            .execute_batch("CREATE TABLE script_private_state (value TEXT)")
            .expect("脚本私有表应可建立");

        validate_current_att_schema(&connection).expect("生产 DDL 应与当前唯一 schema 一致");
    }
}
