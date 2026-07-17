#![allow(dead_code, reason = "项目持久化能力尚未接入生产组合根")]

//! 项目数据库的创建职责。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};

use crate::att_mz::ProjectName;
use crate::storage::sqlite::{
    CreateDatabaseError, QueryExistingDatabaseError, SqliteCommand, SqliteDatabaseCreator,
    SqliteQuery, SqliteQueryExecutor, SqliteRow, SqliteValue,
};

const PROJECT_DATABASE_FILE_NAME: &str = "project.db";
const CREATE_METADATA_TABLE: &str = "CREATE TABLE metadata (\n    name                                TEXT NOT NULL PRIMARY KEY,\n    source_language                     TEXT NOT NULL,\n    target_language                     TEXT NOT NULL,\n    dialogue_max_fullwidth_chars        INTEGER NOT NULL CHECK (dialogue_max_fullwidth_chars > 0),\n    scrolling_text_max_fullwidth_chars  INTEGER NOT NULL CHECK (scrolling_text_max_fullwidth_chars > 0),\n    help_description_max_fullwidth_chars INTEGER NOT NULL CHECK (help_description_max_fullwidth_chars > 0)\n)";
const INSERT_METADATA: &str = "INSERT INTO metadata (name, source_language, target_language, dialogue_max_fullwidth_chars, scrolling_text_max_fullwidth_chars, help_description_max_fullwidth_chars) VALUES (?, ?, ?, ?, ?, ?)";
const SELECT_METADATA: &str = "SELECT name, source_language, target_language, dialogue_max_fullwidth_chars, scrolling_text_max_fullwidth_chars, help_description_max_fullwidth_chars FROM metadata";

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
    write_back_data: PathBuf,
    write_back_js: PathBuf,
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
        let write_back_data = write_back_root.join("data");
        let write_back_js = write_back_root.join("js");

        Self {
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

    pub(crate) fn write_back_data(&self) -> &Path {
        &self.write_back_data
    }

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
    layout_profile: MzWriteBackLayoutProfile,
}

impl StoredProjectRecord {
    /// 建立一条已经由项目数据库读取器确认可信的记录。
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
            layout_profile,
        )
    }

    /// 直接复用已经建立的工作区布局。
    pub(crate) fn from_layout(
        name: ProjectName,
        layout: ProjectWorkspaceLayout,
        source_language: String,
        target_language: String,
        layout_profile: MzWriteBackLayoutProfile,
    ) -> Self {
        Self {
            name,
            layout,
            source_language,
            target_language,
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

    let values = row.into_values();
    if values.len() != 6 {
        return Err(ProjectDatabaseReadError::InvalidMetadata {
            path: database_path,
            reason: InvalidProjectMetadata::WrongColumnCount {
                actual: values.len(),
            },
        });
    }

    let mut values = values.into_iter();
    let stored_name = text_column(values.next().expect("已确认 metadata 恰好有六列"), "name")
        .map_err(|reason| ProjectDatabaseReadError::InvalidMetadata {
            path: database_path.clone(),
            reason,
        })?;
    let source_language = text_column(
        values.next().expect("已确认 metadata 恰好有六列"),
        "source_language",
    )
    .map_err(|reason| ProjectDatabaseReadError::InvalidMetadata {
        path: database_path.clone(),
        reason,
    })?;
    let target_language = text_column(
        values.next().expect("已确认 metadata 恰好有六列"),
        "target_language",
    )
    .map_err(|reason| ProjectDatabaseReadError::InvalidMetadata {
        path: database_path.clone(),
        reason,
    })?;
    let dialogue_max_fullwidth_chars = max_fullwidth_chars_column(
        values.next().expect("已确认 metadata 恰好有六列"),
        "dialogue_max_fullwidth_chars",
    )
    .map_err(|reason| ProjectDatabaseReadError::InvalidMetadata {
        path: database_path.clone(),
        reason,
    })?;
    let scrolling_text_max_fullwidth_chars = max_fullwidth_chars_column(
        values.next().expect("已确认 metadata 恰好有六列"),
        "scrolling_text_max_fullwidth_chars",
    )
    .map_err(|reason| ProjectDatabaseReadError::InvalidMetadata {
        path: database_path.clone(),
        reason,
    })?;
    let help_description_max_fullwidth_chars = max_fullwidth_chars_column(
        values.next().expect("已确认 metadata 恰好有六列"),
        "help_description_max_fullwidth_chars",
    )
    .map_err(|reason| ProjectDatabaseReadError::InvalidMetadata {
        path: database_path.clone(),
        reason,
    })?;

    let stored_name = stored_name.parse::<ProjectName>().map_err(|message| {
        ProjectDatabaseReadError::InvalidMetadata {
            path: database_path.clone(),
            reason: InvalidProjectMetadata::InvalidProjectName { message },
        }
    })?;
    if &stored_name != requested_name {
        return Err(ProjectDatabaseReadError::InvalidMetadata {
            path: database_path,
            reason: InvalidProjectMetadata::NameMismatch {
                requested: requested_name.as_str().to_owned(),
                stored: stored_name.as_str().to_owned(),
            },
        });
    }

    if source_language.trim().is_empty() {
        return Err(ProjectDatabaseReadError::InvalidMetadata {
            path: database_path,
            reason: InvalidProjectMetadata::BlankLanguage {
                column: "source_language",
            },
        });
    }
    if target_language.trim().is_empty() {
        return Err(ProjectDatabaseReadError::InvalidMetadata {
            path: database_path,
            reason: InvalidProjectMetadata::BlankLanguage {
                column: "target_language",
            },
        });
    }

    Ok(StoredProjectRecord::from_layout(
        stored_name,
        layout,
        source_language,
        target_language,
        MzWriteBackLayoutProfile::new(
            dialogue_max_fullwidth_chars,
            scrolling_text_max_fullwidth_chars,
            help_description_max_fullwidth_chars,
        ),
    ))
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
}

impl fmt::Display for InvalidProjectMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRow => formatter.write_str("缺少项目记录"),
            Self::MultipleRows => formatter.write_str("包含多条项目记录"),
            Self::WrongColumnCount { actual } => {
                write!(formatter, "查询结果应有 6 列，实际为 {actual} 列")
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
        }
    }
}

/// 已由初始化用例建立并可以在内部信任的新项目事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NewProject {
    name: ProjectName,
    source_language: String,
    target_language: String,
    layout_profile: MzWriteBackLayoutProfile,
}

impl NewProject {
    /// 汇集创建项目数据库所需的全部受信事实。
    pub(crate) fn new(
        name: ProjectName,
        source_language: String,
        target_language: String,
        layout_profile: MzWriteBackLayoutProfile,
    ) -> Self {
        Self {
            name,
            source_language,
            target_language,
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

    pub(crate) fn layout_profile(&self) -> &MzWriteBackLayoutProfile {
        &self.layout_profile
    }
}

/// 已创建项目数据库的定位信息。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CreatedProject {
    database_path: PathBuf,
}

impl CreatedProject {
    /// 记录一个已经成功创建的数据库路径。
    pub(crate) fn new(database_path: PathBuf) -> Self {
        Self { database_path }
    }

    /// 返回成功创建的数据库路径。
    pub(crate) fn database_path(&self) -> &Path {
        &self.database_path
    }
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
    ) -> impl Future<Output = Result<CreatedProject, Self::Error>> + Send;
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

    async fn create(
        &self,
        database_path: PathBuf,
        project: NewProject,
    ) -> Result<CreatedProject, Self::Error> {
        let commands = metadata_commands(&project);

        self.sqlite
            .create_new_database(database_path.clone(), commands)
            .await
            .map_err(|error| {
                ProjectDatabaseCreateError::from_driver(database_path.clone(), error)
            })?;

        Ok(CreatedProject::new(database_path))
    }
}

fn metadata_commands(project: &NewProject) -> Vec<SqliteCommand> {
    vec![
        SqliteCommand::new(CREATE_METADATA_TABLE, Vec::new()),
        SqliteCommand::new(
            INSERT_METADATA,
            vec![
                SqliteValue::Text(project.name().as_str().to_owned()),
                SqliteValue::Text(project.source_language().to_owned()),
                SqliteValue::Text(project.target_language().to_owned()),
                SqliteValue::Integer(i64::from(project.layout_profile().dialogue_body().get())),
                SqliteValue::Integer(i64::from(project.layout_profile().scrolling_text().get())),
                SqliteValue::Integer(i64::from(project.layout_profile().help_description().get())),
            ],
        ),
    ]
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
        assert_eq!(
            layout.write_back_data(),
            Path::new("C:/att/projects/测试 游戏/write_back/data")
        );
        assert_eq!(
            layout.write_back_js(),
            Path::new("C:/att/projects/测试 游戏/write_back/js")
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
            layout_profile(),
        )
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

        let created = service
            .create(
                PathBuf::from("C:/projects/测试 游戏/project.db"),
                project("测试 游戏"),
            )
            .await
            .expect("database creation should succeed");

        assert_eq!(
            created.database_path(),
            Path::new("C:/projects/测试 游戏/project.db")
        );

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
        assert_eq!(invocation.commands.len(), 2);
        assert_eq!(invocation.commands[0].statement(), CREATE_METADATA_TABLE);
        assert!(invocation.commands[0].parameters().is_empty());
        assert_eq!(invocation.commands[1].statement(), INSERT_METADATA);
        assert_eq!(
            invocation.commands[1].parameters(),
            &[
                SqliteValue::Text("测试 游戏".to_owned()),
                SqliteValue::Text("ja".to_owned()),
                SqliteValue::Text("zh-CN".to_owned()),
                SqliteValue::Integer(24),
                SqliteValue::Integer(30),
                SqliteValue::Integer(18),
            ]
        );
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
            Self {
                invocations: Mutex::new(Vec::new()),
                responses: Mutex::new(VecDeque::from([response])),
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

    fn record_reading_service(
        response: Result<Vec<SqliteRow>, QueryExistingDatabaseError<FakeDriverError>>,
    ) -> ProjectDatabaseRecordReadingService<RecordingQueryExecutor> {
        ProjectDatabaseRecordReadingService::new(
            PathBuf::from("C:/att/projects"),
            RecordingQueryExecutor::responding_with(response),
        )
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
