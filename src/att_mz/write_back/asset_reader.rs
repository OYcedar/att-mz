//! 从五张 MZ 标准资产表建立不含术语数据的写回快照。

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use futures_util::stream::{self, StreamExt, TryStreamExt};

use crate::att_mz::location_codec::{MzLocationCodec, MzLocationCodecError};
use crate::att_mz::project::OpenedProject;
use crate::att_mz::standard_asset::{
    MzStandardAssetLocationError, MzStandardAssetOwner, MzStandardAssetReadingConfig,
    MzStandardAssetStorageKind, MzStandardAssetTable, MzTextBodyUnit,
};
use crate::att_mz::text::{MzLocation, TextGroupKind};
use crate::storage::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::storage::sqlite::{
    QueryExistingDatabaseError, SqliteQuery, SqliteQueryExecutor, SqliteRow, SqliteValue,
};

use super::standard::{
    StandardWriteBackAssetReader, StandardWriteBackFieldRole, StandardWriteBackGroup,
    StandardWriteBackLeaf, StandardWriteBackSnapshot, StandardWriteBackSnapshotError,
};

const READ_STANDARD_WRITE_BACK_ASSETS: &str = r#"SELECT
    asset_table,
    exact_location,
    owner,
    group_location,
    field_name,
    unit_type,
    original_text,
    translation
FROM (
    SELECT
        'entry' AS asset_table,
        exact_location,
        owner,
        group_location,
        field_name,
        NULL AS unit_type,
        original_text,
        translation
    FROM entry

    UNION ALL

    SELECT
        'system_text',
        exact_location,
        owner,
        group_location,
        field_name,
        NULL,
        original_text,
        translation
    FROM system_text

    UNION ALL

    SELECT
        'map_text',
        exact_location,
        owner,
        group_location,
        field_name,
        NULL,
        original_text,
        translation
    FROM map_text

    UNION ALL

    SELECT
        'text_body',
        exact_location,
        owner,
        group_location,
        field_name,
        unit_type,
        original_text,
        translation
    FROM text_body

    UNION ALL

    SELECT
        'plugin_param',
        exact_location,
        owner,
        group_location,
        field_name,
        NULL,
        original_text,
        translation
    FROM plugin_param
)
ORDER BY asset_table, exact_location"#;

/// 使用单次 SQLite 一致查询和受控 CPU 解码建立 Standard 写回快照。
pub(crate) struct MzStandardWriteBackAssetReadingService<Q, C> {
    sqlite: Q,
    cpu: C,
    config: MzStandardAssetReadingConfig,
}

impl<Q, C> MzStandardWriteBackAssetReadingService<Q, C> {
    pub(crate) fn new(sqlite: Q, cpu: C, config: MzStandardAssetReadingConfig) -> Self {
        Self {
            sqlite,
            cpu,
            config,
        }
    }
}

impl<Q, C> StandardWriteBackAssetReader for MzStandardWriteBackAssetReadingService<Q, C>
where
    Q: SqliteQueryExecutor,
    C: CpuTaskExecutor,
{
    type Error = MzStandardWriteBackAssetReadingError<Q::Error, C::Error>;

    async fn read(
        &self,
        project: &OpenedProject,
    ) -> Result<StandardWriteBackSnapshot, Self::Error> {
        let database_path = project.database_path().to_path_buf();
        let rows = self
            .sqlite
            .query_existing_database(
                database_path.clone(),
                SqliteQuery::new(READ_STANDARD_WRITE_BACK_ASSETS, Vec::new()),
            )
            .await
            .map_err(|error| map_query_error(database_path, error))?;

        if rows.is_empty() {
            return Ok(StandardWriteBackSnapshot::empty());
        }

        let leaves_per_job = self.config.leaves_per_decode_job().get();
        let batches = self
            .cpu
            .execute(move || partition_rows(rows, leaves_per_job))
            .await
            .map_err(MzStandardWriteBackAssetReadingError::SchedulePartition)?;

        let decoded_batches = stream::iter(batches.into_iter().map(|batch| async move {
            self.cpu
                .execute(move || decode_rows(batch))
                .await
                .map_err(MzStandardWriteBackAssetReadingError::ScheduleDecode)?
                .map_err(MzStandardWriteBackAssetReadingError::InvalidSnapshot)
        }))
        .buffered(self.config.decode_concurrency().get())
        .try_collect::<Vec<_>>()
        .await?;

        let decoded = decoded_batches.into_iter().flatten().collect::<Vec<_>>();
        self.cpu
            .execute(move || assemble_snapshot(decoded))
            .await
            .map_err(MzStandardWriteBackAssetReadingError::ScheduleAssembly)?
            .map_err(MzStandardWriteBackAssetReadingError::InvalidSnapshot)
    }
}

/// 标准写回资产读取责任的阶段化错误。
#[derive(Debug)]
pub(crate) enum MzStandardWriteBackAssetReadingError<Q, C> {
    DatabaseNotFound { database_path: PathBuf },
    Query { database_path: PathBuf, source: Q },
    SchedulePartition(CpuTaskExecutionError<C>),
    ScheduleDecode(CpuTaskExecutionError<C>),
    ScheduleAssembly(CpuTaskExecutionError<C>),
    InvalidSnapshot(InvalidStandardWriteBackAssetSnapshot),
}

impl<Q, C> fmt::Display for MzStandardWriteBackAssetReadingError<Q, C>
where
    Q: fmt::Display,
    C: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DatabaseNotFound { database_path } => {
                write!(formatter, "项目数据库不存在：{}", database_path.display())
            }
            Self::Query {
                database_path,
                source,
            } => write!(
                formatter,
                "无法从 {} 读取标准写回资产：{source}",
                database_path.display()
            ),
            Self::SchedulePartition(source) => {
                write!(formatter, "写回资产解码分批任务执行失败：{source}")
            }
            Self::ScheduleDecode(source) => {
                write!(formatter, "写回资产解码任务执行失败：{source}")
            }
            Self::ScheduleAssembly(source) => {
                write!(formatter, "写回资产快照组装任务执行失败：{source}")
            }
            Self::InvalidSnapshot(source) => write!(formatter, "标准写回资产损坏：{source}"),
        }
    }
}

impl<Q, C> Error for MzStandardWriteBackAssetReadingError<Q, C>
where
    Q: Error + 'static,
    C: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Query { source, .. } => Some(source),
            Self::SchedulePartition(source)
            | Self::ScheduleDecode(source)
            | Self::ScheduleAssembly(source) => Some(source),
            Self::InvalidSnapshot(source) => Some(source),
            Self::DatabaseNotFound { .. } => None,
        }
    }
}

#[derive(Debug)]
struct DecodedRow {
    kind: TextGroupKind,
    exact_location: MzLocation,
    group_location: MzLocation,
    role: StandardWriteBackFieldRole,
    original_text: String,
    translation: Option<String>,
}

/// 数据库内容违反五表 schema、角色编码或写回快照不变量的原因。
#[derive(Debug)]
pub(crate) enum InvalidStandardWriteBackAssetSnapshot {
    WrongColumnCount {
        actual: usize,
    },
    WrongColumnType {
        column: &'static str,
        expected: &'static str,
        actual: &'static str,
    },
    UnknownAssetTable(String),
    UnknownOwner(String),
    UnknownUnitType(String),
    UnexpectedUnitType {
        table: String,
    },
    InvalidFieldRole {
        kind: TextGroupKind,
        field_name: String,
    },
    InvalidLocation {
        column: &'static str,
        source: MzLocationCodecError,
    },
    InvalidStorageLocation(MzStandardAssetLocationError),
    InvalidModel(StandardWriteBackSnapshotError),
}

impl fmt::Display for InvalidStandardWriteBackAssetSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongColumnCount { actual } => {
                write!(formatter, "写回资产查询行应包含 8 列，实际为 {actual} 列")
            }
            Self::WrongColumnType {
                column,
                expected,
                actual,
            } => write!(formatter, "列 {column} 应为 {expected}，实际为 {actual}"),
            Self::UnknownAssetTable(table) => write!(formatter, "未知标准资产表：{table}"),
            Self::UnknownOwner(owner) => write!(formatter, "未知资产所有者：{owner}"),
            Self::UnknownUnitType(unit_type) => {
                write!(formatter, "未知文本单元类型：{unit_type}")
            }
            Self::UnexpectedUnitType { table } => {
                write!(formatter, "资产表 {table} 的 unit_type 与表语义不一致")
            }
            Self::InvalidFieldRole { kind, field_name } => {
                write!(formatter, "组类型 {kind:?} 包含非规范字段角色 {field_name}")
            }
            Self::InvalidLocation { column, source } => {
                write!(formatter, "列 {column} 中的结构化位置无效：{source}")
            }
            Self::InvalidStorageLocation(source) => {
                write!(formatter, "结构化位置与标准资产存储语义不一致：{source}")
            }
            Self::InvalidModel(source) => source.fmt(formatter),
        }
    }
}

impl Error for InvalidStandardWriteBackAssetSnapshot {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidLocation { source, .. } => Some(source),
            Self::InvalidStorageLocation(source) => Some(source),
            Self::InvalidModel(source) => Some(source),
            _ => None,
        }
    }
}

fn map_query_error<Q, C>(
    database_path: PathBuf,
    error: QueryExistingDatabaseError<Q>,
) -> MzStandardWriteBackAssetReadingError<Q, C> {
    match error {
        QueryExistingDatabaseError::NotFound => {
            MzStandardWriteBackAssetReadingError::DatabaseNotFound { database_path }
        }
        QueryExistingDatabaseError::QueryFailed(source) => {
            MzStandardWriteBackAssetReadingError::Query {
                database_path,
                source,
            }
        }
    }
}

fn partition_rows(rows: Vec<SqliteRow>, leaves_per_job: usize) -> Vec<Vec<SqliteRow>> {
    let mut rows = rows.into_iter();
    let mut batches = Vec::new();
    loop {
        let batch = rows.by_ref().take(leaves_per_job).collect::<Vec<_>>();
        if batch.is_empty() {
            return batches;
        }
        batches.push(batch);
    }
}

fn decode_rows(
    rows: Vec<SqliteRow>,
) -> Result<Vec<DecodedRow>, InvalidStandardWriteBackAssetSnapshot> {
    rows.into_iter().map(decode_row).collect()
}

fn decode_row(row: SqliteRow) -> Result<DecodedRow, InvalidStandardWriteBackAssetSnapshot> {
    let values = row.into_values();
    if values.len() != 8 {
        return Err(InvalidStandardWriteBackAssetSnapshot::WrongColumnCount {
            actual: values.len(),
        });
    }
    let mut values = values.into_iter();
    let table_name = required_text(next(&mut values), "asset_table")?;
    let exact_location = required_text(next(&mut values), "exact_location")?;
    let owner = required_text(next(&mut values), "owner")?;
    let group_location = required_text(next(&mut values), "group_location")?;
    let field_name = required_text(next(&mut values), "field_name")?;
    let unit_type = optional_text(next(&mut values), "unit_type")?;
    let original_text = required_text(next(&mut values), "original_text")?;
    let translation = optional_text(next(&mut values), "translation")?;

    let table = MzStandardAssetTable::from_storage_name(&table_name).ok_or_else(|| {
        InvalidStandardWriteBackAssetSnapshot::UnknownAssetTable(table_name.clone())
    })?;
    if MzStandardAssetOwner::from_storage_name(&owner).is_none() {
        return Err(InvalidStandardWriteBackAssetSnapshot::UnknownOwner(owner));
    }
    let unit_type = unit_type
        .as_deref()
        .map(|value| {
            MzTextBodyUnit::from_storage_name(value).ok_or_else(|| {
                InvalidStandardWriteBackAssetSnapshot::UnknownUnitType(value.to_owned())
            })
        })
        .transpose()?;
    let storage = MzStandardAssetStorageKind::from_parts(table, unit_type).ok_or_else(|| {
        InvalidStandardWriteBackAssetSnapshot::UnexpectedUnitType {
            table: table_name.clone(),
        }
    })?;
    let kind = storage.group_kind();
    let exact_location = MzLocationCodec::decode(&exact_location).map_err(|source| {
        InvalidStandardWriteBackAssetSnapshot::InvalidLocation {
            column: "exact_location",
            source,
        }
    })?;
    let group_location = MzLocationCodec::decode(&group_location).map_err(|source| {
        InvalidStandardWriteBackAssetSnapshot::InvalidLocation {
            column: "group_location",
            source,
        }
    })?;
    storage
        .validate_locations(&exact_location, &group_location)
        .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidStorageLocation)?;
    let role = role_for(kind, field_name)?;

    Ok(DecodedRow {
        kind,
        exact_location,
        group_location,
        role,
        original_text,
        translation,
    })
}

fn role_for(
    kind: TextGroupKind,
    field_name: String,
) -> Result<StandardWriteBackFieldRole, InvalidStandardWriteBackAssetSnapshot> {
    match kind {
        TextGroupKind::EventDialogue if field_name == "speaker" => {
            Ok(StandardWriteBackFieldRole::dialogue_speaker())
        }
        TextGroupKind::EventDialogue => parse_body_index(&field_name)
            .map(StandardWriteBackFieldRole::dialogue_body)
            .ok_or(InvalidStandardWriteBackAssetSnapshot::InvalidFieldRole { kind, field_name }),
        TextGroupKind::EventScrollingText => parse_body_index(&field_name)
            .map(StandardWriteBackFieldRole::scrolling_text_body)
            .ok_or(InvalidStandardWriteBackAssetSnapshot::InvalidFieldRole { kind, field_name }),
        _ => Ok(StandardWriteBackFieldRole::scalar(field_name)),
    }
}

fn parse_body_index(field_name: &str) -> Option<usize> {
    let digits = field_name.strip_prefix("body[")?.strip_suffix(']')?;
    if digits.is_empty()
        || (digits.len() > 1 && digits.starts_with('0'))
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    digits.parse().ok()
}

fn next(values: &mut impl Iterator<Item = SqliteValue>) -> SqliteValue {
    values
        .next()
        .expect("列数已验证，写回资产查询行必须具有完整投影")
}

fn required_text(
    value: SqliteValue,
    column: &'static str,
) -> Result<String, InvalidStandardWriteBackAssetSnapshot> {
    match value {
        SqliteValue::Text(value) => Ok(value),
        actual => Err(InvalidStandardWriteBackAssetSnapshot::WrongColumnType {
            column,
            expected: "TEXT",
            actual: actual.kind_name(),
        }),
    }
}

fn optional_text(
    value: SqliteValue,
    column: &'static str,
) -> Result<Option<String>, InvalidStandardWriteBackAssetSnapshot> {
    match value {
        SqliteValue::Null => Ok(None),
        SqliteValue::Text(value) => Ok(Some(value)),
        actual => Err(InvalidStandardWriteBackAssetSnapshot::WrongColumnType {
            column,
            expected: "TEXT 或 NULL",
            actual: actual.kind_name(),
        }),
    }
}

fn assemble_snapshot(
    rows: Vec<DecodedRow>,
) -> Result<StandardWriteBackSnapshot, InvalidStandardWriteBackAssetSnapshot> {
    let mut groups = BTreeMap::<(TextGroupKind, MzLocation), Vec<StandardWriteBackLeaf>>::new();
    for row in rows {
        let leaf = StandardWriteBackLeaf::new(
            row.role,
            row.exact_location,
            row.original_text,
            row.translation,
        )
        .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidModel)?;
        groups
            .entry((row.kind, row.group_location))
            .or_default()
            .push(leaf);
    }

    let groups = groups
        .into_iter()
        .map(|((kind, group_location), leaves)| {
            StandardWriteBackGroup::new(kind, group_location, leaves)
                .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidModel)
        })
        .collect::<Result<Vec<_>, _>>()?;
    StandardWriteBackSnapshot::new(groups)
        .map_err(InvalidStandardWriteBackAssetSnapshot::InvalidModel)
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use crate::att_mz::ProjectName;
    use crate::att_mz::project::{MaxFullwidthChars, MzWriteBackLayoutProfile};
    use crate::att_mz::text::{MzLocationStep, MzSource, StandardDataFile};

    use super::*;

    type QueryResponse = Result<Vec<SqliteRow>, QueryExistingDatabaseError<FakeError>>;
    type SharedQueryResponse = Arc<Mutex<Option<QueryResponse>>>;

    #[derive(Clone)]
    struct RecordingQuery {
        calls: Arc<Mutex<Vec<(PathBuf, SqliteQuery)>>>,
        response: SharedQueryResponse,
    }

    impl SqliteQueryExecutor for RecordingQuery {
        type Error = FakeError;

        fn query_existing_database(
            &self,
            path: PathBuf,
            query: SqliteQuery,
        ) -> impl Future<Output = Result<Vec<SqliteRow>, QueryExistingDatabaseError<Self::Error>>> + Send
        {
            self.calls
                .lock()
                .expect("查询调用锁不应中毒")
                .push((path, query));
            let response = self
                .response
                .lock()
                .expect("查询响应锁不应中毒")
                .take()
                .expect("测试查询只应调用一次");
            async move { response }
        }
    }

    #[derive(Clone)]
    struct RecordingCpu {
        calls: Arc<AtomicUsize>,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
        fail_at: Option<usize>,
    }

    impl CpuTaskExecutor for RecordingCpu {
        type Error = FakeError;

        async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.fail_at == Some(call) {
                return Err(CpuTaskExecutionError::Unavailable(FakeError("cpu")));
            }
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            tokio::task::yield_now().await;
            let output = task();
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(output)
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeError {}

    #[tokio::test]
    async fn one_eight_column_query_builds_all_storage_kinds_without_terminology() {
        let dialogue_group = command_location(2, 10);
        let scrolling_group = command_location(4, 20);
        let rows = vec![
            replace_column(
                row(
                    "plugin_param",
                    &plugin_location(),
                    &plugin_group_location(),
                    "Name",
                    None,
                    "插件",
                    Some("Plugin"),
                ),
                2,
                SqliteValue::Text("rules".to_owned()),
            ),
            row(
                "text_body",
                &command_parameter_location(5, 31, 0),
                &command_location(5, 30),
                "parameter[0]",
                Some("event_command"),
                "事件原文",
                Some("Event"),
            ),
            row(
                "text_body",
                &command_parameter_location(4, 21, 0),
                &scrolling_group,
                "body[0]",
                Some("scrolling_text"),
                "滚动原文",
                Some("Scroll"),
            ),
            row(
                "text_body",
                &command_parameter_location(3, 11, 0),
                &command_location(3, 10),
                "choice[0]",
                Some("choices"),
                "选项原文",
                None,
            ),
            row(
                "text_body",
                &command_parameter_location(2, 11, 0),
                &dialogue_group,
                "body[0]",
                Some("dialogue"),
                "对话原文",
                Some("Dialogue"),
            ),
            row(
                "text_body",
                &command_parameter_location(2, 10, 4),
                &dialogue_group,
                "speaker",
                Some("dialogue"),
                "角色",
                Some("Actor"),
            ),
            scalar_row(
                "map_text",
                MzSource::map(1),
                "displayName",
                "地图",
                Some("Map"),
            ),
            scalar_row(
                "system_text",
                MzSource::data(StandardDataFile::System),
                "gameTitle",
                "标题",
                Some("Title"),
            ),
            scalar_row(
                "entry",
                MzSource::data(StandardDataFile::Items),
                "name",
                "药水",
                Some("Potion"),
            ),
        ];
        let harness = Harness::new(Ok(rows));

        let snapshot = harness
            .service(3, 2, None)
            .read(&project())
            .await
            .expect("写回快照应读取成功");

        let calls = harness.query_calls.lock().expect("查询调用锁不应中毒");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, PathBuf::from("C:/projects/demo/project.db"));
        assert_eq!(calls[0].1.statement(), READ_STANDARD_WRITE_BACK_ASSETS);
        assert!(calls[0].1.parameters().is_empty());
        assert_eq!(calls[0].1.statement().matches("UNION ALL").count(), 4);
        assert!(!calls[0].1.statement().contains("terminology"));
        assert_eq!(snapshot.groups().len(), 8);
        assert_eq!(
            snapshot
                .groups()
                .iter()
                .map(StandardWriteBackGroup::kind)
                .collect::<Vec<_>>(),
            vec![
                TextGroupKind::DatabaseEntry,
                TextGroupKind::System,
                TextGroupKind::Map,
                TextGroupKind::EventDialogue,
                TextGroupKind::EventChoices,
                TextGroupKind::EventScrollingText,
                TextGroupKind::EventCommand,
                TextGroupKind::PluginParameter,
            ]
        );
        let dialogue = snapshot
            .groups()
            .iter()
            .find(|group| group.kind() == TextGroupKind::EventDialogue)
            .expect("对话组应存在");
        assert!(matches!(
            dialogue.leaves()[0].role(),
            StandardWriteBackFieldRole::DialogueSpeaker
        ));
        assert!(matches!(
            dialogue.leaves()[1].role(),
            StandardWriteBackFieldRole::DialogueBody { index: 0 }
        ));
        assert_eq!(
            snapshot
                .groups()
                .iter()
                .flat_map(StandardWriteBackGroup::leaves)
                .filter(|leaf| leaf.translation().is_none())
                .count(),
            1
        );
        assert_eq!(harness.max_cpu_active.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn empty_query_returns_empty_snapshot_without_cpu_work() {
        let harness = Harness::new(Ok(Vec::new()));

        let snapshot = harness
            .service(1, 1, None)
            .read(&project())
            .await
            .expect("空写回快照合法");

        assert!(snapshot.groups().is_empty());
        assert_eq!(harness.cpu_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn event_roles_accept_only_canonical_database_field_names() {
        assert_eq!(
            role_for(TextGroupKind::EventDialogue, "speaker".to_owned()).expect("speaker 应合法"),
            StandardWriteBackFieldRole::dialogue_speaker()
        );
        assert_eq!(
            role_for(TextGroupKind::EventDialogue, "body[0]".to_owned()).expect("body[0] 应合法"),
            StandardWriteBackFieldRole::dialogue_body(0)
        );
        assert_eq!(
            role_for(TextGroupKind::EventScrollingText, "body[12]".to_owned())
                .expect("body[12] 应合法"),
            StandardWriteBackFieldRole::scrolling_text_body(12)
        );
        for field_name in ["body[]", "body[01]", "body[-1]", "body[1]tail", "speaker "] {
            assert!(
                role_for(TextGroupKind::EventDialogue, field_name.to_owned()).is_err(),
                "{field_name} 不应成为对话角色"
            );
        }
        assert!(role_for(TextGroupKind::EventScrollingText, "speaker".to_owned()).is_err());
    }

    #[test]
    fn malformed_rows_are_rejected_at_the_database_boundary() {
        let location = data_location(StandardDataFile::Items, 1, "name");
        let group = data_group_location(StandardDataFile::Items, 1);
        let valid = row(
            "entry",
            &location,
            &group,
            "name",
            None,
            "剑",
            Some("Sword"),
        );
        let cases = [
            (
                SqliteRow::new(vec![SqliteValue::Text("entry".to_owned())]),
                "column-count",
            ),
            (
                replace_column(valid.clone(), 0, SqliteValue::Text("terms".to_owned())),
                "table",
            ),
            (
                replace_column(valid.clone(), 2, SqliteValue::Text("lua".to_owned())),
                "owner",
            ),
            (
                replace_column(valid.clone(), 5, SqliteValue::Text("dialogue".to_owned())),
                "unit",
            ),
            (replace_column(valid, 6, SqliteValue::Integer(1)), "type"),
        ];

        for (row, label) in cases {
            assert!(decode_row(row).is_err(), "{label} 损坏应被拒绝");
        }
    }

    #[test]
    fn invalid_locations_and_unknown_text_body_units_are_rejected() {
        let group = command_location(1, 10);
        let body = command_parameter_location(1, 11, 0);
        let valid = row(
            "text_body",
            &body,
            &group,
            "body[0]",
            Some("dialogue"),
            "原文",
            Some("译文"),
        );

        assert!(matches!(
            decode_row(replace_column(
                valid.clone(),
                1,
                SqliteValue::Text("not-json".to_owned())
            )),
            Err(InvalidStandardWriteBackAssetSnapshot::InvalidLocation {
                column: "exact_location",
                ..
            })
        ));
        assert!(matches!(
            decode_row(replace_column(
                valid,
                5,
                SqliteValue::Text("dialog".to_owned())
            )),
            Err(InvalidStandardWriteBackAssetSnapshot::UnknownUnitType(unit)) if unit == "dialog"
        ));
    }

    #[test]
    fn decoded_locations_incompatible_with_the_asset_table_are_rejected() {
        let group = data_group_location(StandardDataFile::Items, 1);
        let exact = data_location(StandardDataFile::Items, 1, "Name");

        let error = decode_row(row(
            "plugin_param",
            &exact,
            &group,
            "Name",
            None,
            "插件参数",
            Some("Plugin parameter"),
        ))
        .expect_err("PluginParam 不应接受 Data 来源");

        assert!(matches!(
            error,
            InvalidStandardWriteBackAssetSnapshot::InvalidStorageLocation(
                MzStandardAssetLocationError::SourceDoesNotMatchStorage {
                    storage: MzStandardAssetStorageKind::PluginParam,
                    source: MzSource::Data(StandardDataFile::Items),
                }
            )
        ));
    }

    #[test]
    fn rules_entry_on_map_with_custom_field_and_path_is_accepted() {
        let source = MzSource::map(18);
        let group = MzLocation::value(
            source.clone(),
            vec![MzLocationStep::key("rules_group"), MzLocationStep::index(6)],
        );
        let exact = MzLocation::value(
            source,
            vec![
                MzLocationStep::key("custom_rules_path"),
                MzLocationStep::DecodeJsonString,
                MzLocationStep::key("actual_name"),
            ],
        );
        let mut values = row(
            "entry",
            &exact,
            &group,
            "custom_field_name",
            None,
            "原文",
            Some("Translation"),
        )
        .into_values();
        values[2] = SqliteValue::Text("rules".to_owned());

        let decoded = decode_row(SqliteRow::new(values)).expect("合法 Rules Entry→Map 应被接受");

        assert_eq!(decoded.kind, TextGroupKind::DatabaseEntry);
        assert_eq!(decoded.exact_location, exact);
        assert_eq!(decoded.group_location, group);
        assert_eq!(
            decoded.role,
            StandardWriteBackFieldRole::scalar("custom_field_name")
        );
    }

    #[tokio::test]
    async fn snapshot_model_rejects_non_contiguous_body_indices() {
        let group = command_location(1, 10);
        let rows = vec![
            row(
                "text_body",
                &command_parameter_location(1, 11, 0),
                &group,
                "body[0]",
                Some("dialogue"),
                "第一行",
                Some("First"),
            ),
            row(
                "text_body",
                &command_parameter_location(1, 12, 0),
                &group,
                "body[2]",
                Some("dialogue"),
                "第三行",
                Some("Third"),
            ),
        ];
        let harness = Harness::new(Ok(rows));

        let error = harness
            .service(1, 10, None)
            .read(&project())
            .await
            .expect_err("不连续正文应被拒绝");

        assert!(matches!(
            error,
            MzStandardWriteBackAssetReadingError::InvalidSnapshot(
                InvalidStandardWriteBackAssetSnapshot::InvalidModel(
                    StandardWriteBackSnapshotError::NonContiguousBodyIndex {
                        expected: 1,
                        actual: 2,
                        ..
                    }
                )
            )
        ));
    }

    #[tokio::test]
    async fn query_terminal_states_keep_database_path_and_source() {
        for response in [
            Err(QueryExistingDatabaseError::NotFound),
            Err(QueryExistingDatabaseError::QueryFailed(FakeError("read"))),
        ] {
            let harness = Harness::new(response);
            let error = harness
                .service(1, 1, None)
                .read(&project())
                .await
                .expect_err("查询错误应传播");
            match error {
                MzStandardWriteBackAssetReadingError::DatabaseNotFound { database_path } => {
                    assert_eq!(database_path, PathBuf::from("C:/projects/demo/project.db"));
                }
                MzStandardWriteBackAssetReadingError::Query {
                    database_path,
                    source,
                } => {
                    assert_eq!(database_path, PathBuf::from("C:/projects/demo/project.db"));
                    assert_eq!(source, FakeError("read"));
                }
                other => panic!("未预期的写回资产读取错误：{other}"),
            }
        }
    }

    #[tokio::test]
    async fn cpu_failures_report_the_exact_processing_stage() {
        for (fail_at, expected_stage) in [(1, "partition"), (2, "decode"), (3, "assembly")] {
            let harness = Harness::new(Ok(vec![scalar_row(
                "entry",
                MzSource::data(StandardDataFile::Items),
                "name",
                "剑",
                Some("Sword"),
            )]));
            let error = harness
                .service(1, 10, Some(fail_at))
                .read(&project())
                .await
                .expect_err("CPU 失败应传播");
            let actual_stage = match error {
                MzStandardWriteBackAssetReadingError::SchedulePartition(_) => "partition",
                MzStandardWriteBackAssetReadingError::ScheduleDecode(_) => "decode",
                MzStandardWriteBackAssetReadingError::ScheduleAssembly(_) => "assembly",
                other => panic!("未预期的 CPU 阶段错误：{other}"),
            };
            assert_eq!(actual_stage, expected_stage);
        }
    }

    #[test]
    fn reading_future_is_send() {
        let harness = Harness::new(Ok(Vec::new()));
        let service = harness.service(1, 1, None);
        let project = project();

        assert_send(service.read(&project));
    }

    fn assert_send(_: impl Send) {}

    struct Harness {
        query_calls: Arc<Mutex<Vec<(PathBuf, SqliteQuery)>>>,
        response: SharedQueryResponse,
        cpu_calls: Arc<AtomicUsize>,
        max_cpu_active: Arc<AtomicUsize>,
    }

    impl Harness {
        fn new(response: QueryResponse) -> Self {
            Self {
                query_calls: Arc::new(Mutex::new(Vec::new())),
                response: Arc::new(Mutex::new(Some(response))),
                cpu_calls: Arc::new(AtomicUsize::new(0)),
                max_cpu_active: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn service(
            &self,
            decode_concurrency: usize,
            leaves_per_job: usize,
            fail_at: Option<usize>,
        ) -> MzStandardWriteBackAssetReadingService<RecordingQuery, RecordingCpu> {
            MzStandardWriteBackAssetReadingService::new(
                RecordingQuery {
                    calls: Arc::clone(&self.query_calls),
                    response: Arc::clone(&self.response),
                },
                RecordingCpu {
                    calls: Arc::clone(&self.cpu_calls),
                    active: Arc::new(AtomicUsize::new(0)),
                    max_active: Arc::clone(&self.max_cpu_active),
                    fail_at,
                },
                MzStandardAssetReadingConfig::new(
                    non_zero(decode_concurrency),
                    non_zero(leaves_per_job),
                ),
            )
        }
    }

    fn row(
        table: &str,
        exact_location: &MzLocation,
        group_location: &MzLocation,
        field_name: &str,
        unit_type: Option<&str>,
        original_text: &str,
        translation: Option<&str>,
    ) -> SqliteRow {
        SqliteRow::new(vec![
            SqliteValue::Text(table.to_owned()),
            SqliteValue::Text(
                MzLocationCodec::encode(exact_location).expect("测试精确位置应可编码"),
            ),
            SqliteValue::Text("builtin".to_owned()),
            SqliteValue::Text(MzLocationCodec::encode(group_location).expect("测试组位置应可编码")),
            SqliteValue::Text(field_name.to_owned()),
            unit_type.map_or(SqliteValue::Null, |value| {
                SqliteValue::Text(value.to_owned())
            }),
            SqliteValue::Text(original_text.to_owned()),
            translation.map_or(SqliteValue::Null, |value| {
                SqliteValue::Text(value.to_owned())
            }),
        ])
    }

    fn scalar_row(
        table: &str,
        source: MzSource,
        field_name: &str,
        original_text: &str,
        translation: Option<&str>,
    ) -> SqliteRow {
        let group_location = MzLocation::value(source.clone(), vec![MzLocationStep::index(1)]);
        let exact_location = MzLocation::value(
            source,
            vec![MzLocationStep::index(1), MzLocationStep::key(field_name)],
        );
        row(
            table,
            &exact_location,
            &group_location,
            field_name,
            None,
            original_text,
            translation,
        )
    }

    fn data_group_location(file: StandardDataFile, index: usize) -> MzLocation {
        MzLocation::value(MzSource::data(file), vec![MzLocationStep::index(index)])
    }

    fn data_location(file: StandardDataFile, index: usize, field_name: &str) -> MzLocation {
        MzLocation::value(
            MzSource::data(file),
            vec![
                MzLocationStep::index(index),
                MzLocationStep::key(field_name),
            ],
        )
    }

    fn command_location(map_id: u32, command_index: usize) -> MzLocation {
        MzLocation::value(
            MzSource::map(map_id),
            vec![
                MzLocationStep::key("events"),
                MzLocationStep::index(1),
                MzLocationStep::key("pages"),
                MzLocationStep::index(0),
                MzLocationStep::key("list"),
                MzLocationStep::index(command_index),
            ],
        )
    }

    fn command_parameter_location(
        map_id: u32,
        command_index: usize,
        parameter_index: usize,
    ) -> MzLocation {
        let MzLocation::Value { source, mut steps } = command_location(map_id, command_index)
        else {
            unreachable!("命令位置始终是 Value")
        };
        steps.push(MzLocationStep::key("parameters"));
        steps.push(MzLocationStep::index(parameter_index));
        MzLocation::value(source, steps)
    }

    fn plugin_group_location() -> MzLocation {
        MzLocation::value(MzSource::plugin_parameter(0, "Demo", "Config"), Vec::new())
    }

    fn plugin_location() -> MzLocation {
        MzLocation::value(
            MzSource::plugin_parameter(0, "Demo", "Config"),
            vec![
                MzLocationStep::DecodeJsonString,
                MzLocationStep::key("Name"),
            ],
        )
    }

    fn replace_column(row: SqliteRow, index: usize, value: SqliteValue) -> SqliteRow {
        let mut values = row.into_values();
        values[index] = value;
        SqliteRow::new(values)
    }

    fn project() -> OpenedProject {
        let width = MaxFullwidthChars::new(20).expect("测试行宽应合法");
        OpenedProject::new(
            "demo".parse::<ProjectName>().expect("项目名称应合法"),
            PathBuf::from("C:/projects/demo"),
            PathBuf::from("C:/projects/demo/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            MzWriteBackLayoutProfile::new(width, width, width),
        )
    }

    fn non_zero(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).expect("测试配置必须非零")
    }
}
