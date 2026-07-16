#![allow(dead_code, reason = "标准翻译资产读取器尚未接入生产组合根")]

//! 从五张 MZ 标准资产表建立一致翻译语料。

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::path::PathBuf;

use futures_util::stream::{self, StreamExt, TryStreamExt};

use crate::att_mz::location_codec::{MzLocationCodec, MzLocationCodecError};
use crate::att_mz::text::{MzLocation, TextGroupKind};
use crate::project_database::StoredProjectRecord;
use crate::storage::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::storage::sqlite::{
    QueryExistingDatabaseError, SqliteQuery, SqliteQueryExecutor, SqliteRow, SqliteValue,
};

use super::standard::{
    StandardTranslationAsset, StandardTranslationAssetReader, StandardTranslationCorpus,
    StandardTranslationGroup, TerminologyDependency, TranslationLeafIdentity,
};

const READ_STANDARD_ASSETS: &str = r#"SELECT
    asset_table,
    exact_location,
    owner,
    group_location,
    field_name,
    unit_type,
    original_text,
    translation,
    term,
    term_translation
FROM (
    SELECT
        'entry' AS asset_table,
        asset.exact_location,
        asset.owner,
        asset.group_location,
        asset.field_name,
        NULL AS unit_type,
        asset.original_text,
        asset.translation,
        dependency.term,
        dependency.term_translation
    FROM entry AS asset
    LEFT JOIN translation_terminology_dependency AS dependency
      ON dependency.asset_table = 'entry'
     AND dependency.exact_location = asset.exact_location

    UNION ALL

    SELECT
        'system_text',
        asset.exact_location,
        asset.owner,
        asset.group_location,
        asset.field_name,
        NULL,
        asset.original_text,
        asset.translation,
        dependency.term,
        dependency.term_translation
    FROM system_text AS asset
    LEFT JOIN translation_terminology_dependency AS dependency
      ON dependency.asset_table = 'system_text'
     AND dependency.exact_location = asset.exact_location

    UNION ALL

    SELECT
        'map_text',
        asset.exact_location,
        asset.owner,
        asset.group_location,
        asset.field_name,
        NULL,
        asset.original_text,
        asset.translation,
        dependency.term,
        dependency.term_translation
    FROM map_text AS asset
    LEFT JOIN translation_terminology_dependency AS dependency
      ON dependency.asset_table = 'map_text'
     AND dependency.exact_location = asset.exact_location

    UNION ALL

    SELECT
        'text_body',
        asset.exact_location,
        asset.owner,
        asset.group_location,
        asset.field_name,
        asset.unit_type,
        asset.original_text,
        asset.translation,
        dependency.term,
        dependency.term_translation
    FROM text_body AS asset
    LEFT JOIN translation_terminology_dependency AS dependency
      ON dependency.asset_table = 'text_body'
     AND dependency.exact_location = asset.exact_location

    UNION ALL

    SELECT
        'plugin_param',
        asset.exact_location,
        asset.owner,
        asset.group_location,
        asset.field_name,
        NULL,
        asset.original_text,
        asset.translation,
        dependency.term,
        dependency.term_translation
    FROM plugin_param AS asset
    LEFT JOIN translation_terminology_dependency AS dependency
      ON dependency.asset_table = 'plugin_param'
     AND dependency.exact_location = asset.exact_location
)
ORDER BY asset_table, exact_location, term"#;

/// 标准翻译资产解码阶段的全部必填资源上限。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MzStandardTranslationAssetReadingConfig {
    decode_concurrency: NonZeroUsize,
    leaves_per_decode_job: NonZeroUsize,
}

impl MzStandardTranslationAssetReadingConfig {
    pub(crate) const fn new(
        decode_concurrency: NonZeroUsize,
        leaves_per_decode_job: NonZeroUsize,
    ) -> Self {
        Self {
            decode_concurrency,
            leaves_per_decode_job,
        }
    }

    pub(crate) const fn decode_concurrency(self) -> NonZeroUsize {
        self.decode_concurrency
    }

    pub(crate) const fn leaves_per_decode_job(self) -> NonZeroUsize {
        self.leaves_per_decode_job
    }
}

/// 使用单次 SQLite 一致查询与受控 CPU 解码建立标准翻译语料。
pub(crate) struct MzStandardTranslationAssetReadingService<Q, C> {
    sqlite: Q,
    cpu: C,
    config: MzStandardTranslationAssetReadingConfig,
}

impl<Q, C> MzStandardTranslationAssetReadingService<Q, C> {
    pub(crate) fn new(sqlite: Q, cpu: C, config: MzStandardTranslationAssetReadingConfig) -> Self {
        Self {
            sqlite,
            cpu,
            config,
        }
    }
}

impl<Q, C> StandardTranslationAssetReader for MzStandardTranslationAssetReadingService<Q, C>
where
    Q: SqliteQueryExecutor,
    C: CpuTaskExecutor,
{
    type Error = MzStandardTranslationAssetReadingError<Q::Error, C::Error>;

    async fn read(
        &self,
        project: &StoredProjectRecord,
    ) -> Result<StandardTranslationCorpus, Self::Error> {
        let database_path = project.database_path().to_path_buf();
        let rows = self
            .sqlite
            .query_existing_database(
                database_path.clone(),
                SqliteQuery::new(READ_STANDARD_ASSETS, Vec::new()),
            )
            .await
            .map_err(|error| map_query_error(database_path, error))?;

        if rows.is_empty() {
            return Ok(StandardTranslationCorpus::new(Vec::new()));
        }

        let leaves_per_job = self.config.leaves_per_decode_job.get();
        let batches = self
            .cpu
            .execute(move || partition_rows(rows, leaves_per_job))
            .await
            .map_err(MzStandardTranslationAssetReadingError::SchedulePartition)?;

        let decoded_batches = stream::iter(batches.into_iter().map(|batch| async move {
            self.cpu
                .execute(move || decode_rows(batch))
                .await
                .map_err(MzStandardTranslationAssetReadingError::ScheduleDecode)?
                .map_err(MzStandardTranslationAssetReadingError::InvalidSnapshot)
        }))
        .buffered(self.config.decode_concurrency.get())
        .try_collect::<Vec<_>>()
        .await?;

        let decoded = decoded_batches.into_iter().flatten().collect::<Vec<_>>();
        self.cpu
            .execute(move || assemble_corpus(decoded))
            .await
            .map_err(MzStandardTranslationAssetReadingError::ScheduleAssembly)?
            .map_err(MzStandardTranslationAssetReadingError::InvalidSnapshot)
    }
}

/// 标准翻译资产读取职责产生的阶段化错误。
#[derive(Debug)]
pub(crate) enum MzStandardTranslationAssetReadingError<Q, C> {
    DatabaseNotFound { database_path: PathBuf },
    Query { database_path: PathBuf, source: Q },
    SchedulePartition(CpuTaskExecutionError<C>),
    ScheduleDecode(CpuTaskExecutionError<C>),
    ScheduleAssembly(CpuTaskExecutionError<C>),
    InvalidSnapshot(InvalidStandardTranslationAssetSnapshot),
}

impl<Q, C> fmt::Display for MzStandardTranslationAssetReadingError<Q, C>
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
                "无法从 {} 读取标准翻译资产：{source}",
                database_path.display()
            ),
            Self::SchedulePartition(source) => {
                write!(formatter, "资产解码分批任务执行失败：{source}")
            }
            Self::ScheduleDecode(source) => write!(formatter, "资产解码任务执行失败：{source}"),
            Self::ScheduleAssembly(source) => {
                write!(formatter, "资产语料组装任务执行失败：{source}")
            }
            Self::InvalidSnapshot(source) => write!(formatter, "标准翻译资产损坏：{source}"),
        }
    }
}

impl<Q, C> Error for MzStandardTranslationAssetReadingError<Q, C>
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

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum AssetTable {
    Entry,
    SystemText,
    MapText,
    TextBody,
    PluginParam,
}

impl AssetTable {
    const fn from_name(value: &str) -> Option<Self> {
        match value.as_bytes() {
            b"entry" => Some(Self::Entry),
            b"system_text" => Some(Self::SystemText),
            b"map_text" => Some(Self::MapText),
            b"text_body" => Some(Self::TextBody),
            b"plugin_param" => Some(Self::PluginParam),
            _ => None,
        }
    }

    const fn kind(self, unit_type: Option<TextBodyUnit>) -> Option<TextGroupKind> {
        match (self, unit_type) {
            (Self::Entry, None) => Some(TextGroupKind::DatabaseEntry),
            (Self::SystemText, None) => Some(TextGroupKind::System),
            (Self::MapText, None) => Some(TextGroupKind::Map),
            (Self::PluginParam, None) => Some(TextGroupKind::PluginParameter),
            (Self::TextBody, Some(TextBodyUnit::Dialogue)) => Some(TextGroupKind::EventDialogue),
            (Self::TextBody, Some(TextBodyUnit::Choices)) => Some(TextGroupKind::EventChoices),
            (Self::TextBody, Some(TextBodyUnit::ScrollingText)) => {
                Some(TextGroupKind::EventScrollingText)
            }
            (Self::TextBody, Some(TextBodyUnit::EventCommand)) => Some(TextGroupKind::EventCommand),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TextBodyUnit {
    Dialogue,
    Choices,
    ScrollingText,
    EventCommand,
}

impl TextBodyUnit {
    const fn from_name(value: &str) -> Option<Self> {
        match value.as_bytes() {
            b"dialogue" => Some(Self::Dialogue),
            b"choices" => Some(Self::Choices),
            b"scrolling_text" => Some(Self::ScrollingText),
            b"event_command" => Some(Self::EventCommand),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct DecodedRow {
    table: AssetTable,
    kind: TextGroupKind,
    exact_location: MzLocation,
    group_location: MzLocation,
    field_name: String,
    original_text: String,
    translation: Option<String>,
    terminology_dependency: Option<TerminologyDependency>,
}

#[derive(Debug)]
struct LeafAccumulator {
    table: AssetTable,
    kind: TextGroupKind,
    group_location: MzLocation,
    field_name: String,
    original_text: String,
    translation: Option<String>,
    terminology_dependencies: BTreeMap<String, String>,
}

impl LeafAccumulator {
    fn from_row(row: DecodedRow) -> (Self, Option<TerminologyDependency>) {
        let terminology_dependency = row.terminology_dependency;
        let leaf = Self {
            table: row.table,
            kind: row.kind,
            group_location: row.group_location,
            field_name: row.field_name,
            original_text: row.original_text,
            translation: row.translation,
            terminology_dependencies: BTreeMap::new(),
        };
        (leaf, terminology_dependency)
    }

    fn accepts(&self, row: &DecodedRow) -> bool {
        self.table == row.table
            && self.kind == row.kind
            && self.group_location == row.group_location
            && self.field_name == row.field_name
            && self.original_text == row.original_text
            && self.translation == row.translation
    }
}

/// 数据库内容违反标准资产 schema 或跨行一致性时的明确原因。
#[derive(Debug)]
pub(crate) enum InvalidStandardTranslationAssetSnapshot {
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
    BlankFieldName,
    BlankOriginalText,
    BlankTranslation,
    PartialTerminologyDependency,
    BlankTerminologyDependency,
    DependencyWithoutTranslation,
    InvalidLocation {
        column: &'static str,
        source: MzLocationCodecError,
    },
    ContradictoryAssetRows {
        exact_location: Box<MzLocation>,
    },
    DuplicateTerminologyDependency {
        exact_location: Box<MzLocation>,
        term: String,
    },
    ContradictoryTerminologyDependency {
        exact_location: Box<MzLocation>,
        term: String,
    },
}

impl fmt::Display for InvalidStandardTranslationAssetSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongColumnCount { actual } => {
                write!(formatter, "资产查询行应包含 10 列，实际为 {actual} 列")
            }
            Self::WrongColumnType {
                column,
                expected,
                actual,
            } => write!(formatter, "列 {column} 应为 {expected}，实际为 {actual}"),
            Self::UnknownAssetTable(table) => write!(formatter, "未知标准资产表：{table}"),
            Self::UnknownOwner(owner) => write!(formatter, "未知资产所有者：{owner}"),
            Self::UnknownUnitType(unit_type) => write!(formatter, "未知文本单元类型：{unit_type}"),
            Self::UnexpectedUnitType { table } => {
                write!(formatter, "资产表 {table} 的 unit_type 与表语义不一致")
            }
            Self::BlankFieldName => formatter.write_str("标准资产字段名为空"),
            Self::BlankOriginalText => formatter.write_str("标准资产原文仅包含空白"),
            Self::BlankTranslation => formatter.write_str("标准资产译文仅包含空白"),
            Self::PartialTerminologyDependency => {
                formatter.write_str("术语依赖的原词与译词必须同时存在")
            }
            Self::BlankTerminologyDependency => formatter.write_str("术语依赖包含空值或首尾空白"),
            Self::DependencyWithoutTranslation => formatter.write_str("未翻译资产不应存在术语依赖"),
            Self::InvalidLocation { column, source } => {
                write!(formatter, "列 {column} 中的结构化位置无效：{source}")
            }
            Self::ContradictoryAssetRows { exact_location } => {
                write!(formatter, "同一资产位置存在矛盾行：{exact_location}")
            }
            Self::DuplicateTerminologyDependency {
                exact_location,
                term,
            } => write!(formatter, "资产 {exact_location} 重复记录术语依赖 {term}"),
            Self::ContradictoryTerminologyDependency {
                exact_location,
                term,
            } => write!(
                formatter,
                "资产 {exact_location} 对术语 {term} 记录了矛盾译词"
            ),
        }
    }
}

impl Error for InvalidStandardTranslationAssetSnapshot {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidLocation { source, .. } => Some(source),
            _ => None,
        }
    }
}

fn map_query_error<Q, C>(
    database_path: PathBuf,
    error: QueryExistingDatabaseError<Q>,
) -> MzStandardTranslationAssetReadingError<Q, C> {
    match error {
        QueryExistingDatabaseError::NotFound => {
            MzStandardTranslationAssetReadingError::DatabaseNotFound { database_path }
        }
        QueryExistingDatabaseError::QueryFailed(source) => {
            MzStandardTranslationAssetReadingError::Query {
                database_path,
                source,
            }
        }
    }
}

type RawLeafKey = (Option<SqliteValue>, Option<SqliteValue>);

fn partition_rows(rows: Vec<SqliteRow>, leaves_per_job: usize) -> Vec<Vec<SqliteRow>> {
    let mut batches = Vec::new();
    let mut batch = Vec::new();
    let mut leaves_in_batch = 0usize;
    let mut previous_key: Option<RawLeafKey> = None;

    for row in rows {
        let values = row.values();
        let key = (values.first().cloned(), values.get(1).cloned());
        let is_new_leaf = previous_key.as_ref() != Some(&key);
        if is_new_leaf && leaves_in_batch == leaves_per_job {
            batches.push(std::mem::take(&mut batch));
            leaves_in_batch = 0;
        }
        if is_new_leaf {
            leaves_in_batch += 1;
            previous_key = Some(key);
        }
        batch.push(row);
    }

    if !batch.is_empty() {
        batches.push(batch);
    }
    batches
}

fn decode_rows(
    rows: Vec<SqliteRow>,
) -> Result<Vec<DecodedRow>, InvalidStandardTranslationAssetSnapshot> {
    rows.into_iter().map(decode_row).collect()
}

fn decode_row(row: SqliteRow) -> Result<DecodedRow, InvalidStandardTranslationAssetSnapshot> {
    let values = row.into_values();
    if values.len() != 10 {
        return Err(InvalidStandardTranslationAssetSnapshot::WrongColumnCount {
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
    let term = optional_text(next(&mut values), "term")?;
    let term_translation = optional_text(next(&mut values), "term_translation")?;

    let table = AssetTable::from_name(&table_name).ok_or_else(|| {
        InvalidStandardTranslationAssetSnapshot::UnknownAssetTable(table_name.clone())
    })?;
    if owner != "builtin" && owner != "rules" {
        return Err(InvalidStandardTranslationAssetSnapshot::UnknownOwner(owner));
    }
    let unit = unit_type
        .as_deref()
        .map(|value| {
            TextBodyUnit::from_name(value).ok_or_else(|| {
                InvalidStandardTranslationAssetSnapshot::UnknownUnitType(value.to_owned())
            })
        })
        .transpose()?;
    let kind = table.kind(unit).ok_or_else(|| {
        InvalidStandardTranslationAssetSnapshot::UnexpectedUnitType {
            table: table_name.clone(),
        }
    })?;

    if field_name.is_empty() {
        return Err(InvalidStandardTranslationAssetSnapshot::BlankFieldName);
    }
    if original_text.trim().is_empty() {
        return Err(InvalidStandardTranslationAssetSnapshot::BlankOriginalText);
    }
    if translation
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err(InvalidStandardTranslationAssetSnapshot::BlankTranslation);
    }

    let terminology_dependency = match (term, term_translation) {
        (None, None) => None,
        (Some(_), None) | (None, Some(_)) => {
            return Err(InvalidStandardTranslationAssetSnapshot::PartialTerminologyDependency);
        }
        (Some(term), Some(term_translation)) => {
            if term.trim().is_empty()
                || term.trim() != term
                || term_translation.trim().is_empty()
                || term_translation.trim() != term_translation
            {
                return Err(InvalidStandardTranslationAssetSnapshot::BlankTerminologyDependency);
            }
            if translation.is_none() {
                return Err(InvalidStandardTranslationAssetSnapshot::DependencyWithoutTranslation);
            }
            Some(TerminologyDependency::new(term, term_translation))
        }
    };

    Ok(DecodedRow {
        table,
        kind,
        exact_location: MzLocationCodec::decode(&exact_location).map_err(|source| {
            InvalidStandardTranslationAssetSnapshot::InvalidLocation {
                column: "exact_location",
                source,
            }
        })?,
        group_location: MzLocationCodec::decode(&group_location).map_err(|source| {
            InvalidStandardTranslationAssetSnapshot::InvalidLocation {
                column: "group_location",
                source,
            }
        })?,
        field_name,
        original_text,
        translation,
        terminology_dependency,
    })
}

fn next(values: &mut impl Iterator<Item = SqliteValue>) -> SqliteValue {
    values
        .next()
        .expect("列数已验证，标准资产查询行必须具有完整投影")
}

fn required_text(
    value: SqliteValue,
    column: &'static str,
) -> Result<String, InvalidStandardTranslationAssetSnapshot> {
    match value {
        SqliteValue::Text(value) => Ok(value),
        actual => Err(InvalidStandardTranslationAssetSnapshot::WrongColumnType {
            column,
            expected: "TEXT",
            actual: actual.kind_name(),
        }),
    }
}

fn optional_text(
    value: SqliteValue,
    column: &'static str,
) -> Result<Option<String>, InvalidStandardTranslationAssetSnapshot> {
    match value {
        SqliteValue::Null => Ok(None),
        SqliteValue::Text(value) => Ok(Some(value)),
        actual => Err(InvalidStandardTranslationAssetSnapshot::WrongColumnType {
            column,
            expected: "TEXT 或 NULL",
            actual: actual.kind_name(),
        }),
    }
}

fn assemble_corpus(
    rows: Vec<DecodedRow>,
) -> Result<StandardTranslationCorpus, InvalidStandardTranslationAssetSnapshot> {
    let mut leaves = BTreeMap::<MzLocation, LeafAccumulator>::new();
    for row in rows {
        let exact_location = row.exact_location.clone();
        match leaves.entry(row.exact_location.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                let (mut leaf, dependency) = LeafAccumulator::from_row(row);
                insert_dependency(&mut leaf, &exact_location, dependency)?;
                entry.insert(leaf);
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                if !entry.get().accepts(&row) {
                    return Err(
                        InvalidStandardTranslationAssetSnapshot::ContradictoryAssetRows {
                            exact_location: Box::new(exact_location),
                        },
                    );
                }
                insert_dependency(entry.get_mut(), &exact_location, row.terminology_dependency)?;
            }
        }
    }

    let mut groups = BTreeMap::<(TextGroupKind, MzLocation), Vec<StandardTranslationAsset>>::new();
    for (exact_location, leaf) in leaves {
        let identity = TranslationLeafIdentity::new(
            leaf.kind,
            leaf.group_location.clone(),
            exact_location,
            leaf.original_text,
        );
        let dependencies = leaf
            .terminology_dependencies
            .into_iter()
            .map(|(term, translation)| TerminologyDependency::new(term, translation))
            .collect();
        groups
            .entry((leaf.kind, leaf.group_location))
            .or_default()
            .push(StandardTranslationAsset::new(
                identity,
                leaf.field_name,
                leaf.translation,
                dependencies,
            ));
    }

    Ok(StandardTranslationCorpus::new(
        groups
            .into_iter()
            .map(|((kind, group_location), assets)| {
                StandardTranslationGroup::new(kind, group_location, assets)
            })
            .collect(),
    ))
}

fn insert_dependency(
    leaf: &mut LeafAccumulator,
    exact_location: &MzLocation,
    dependency: Option<TerminologyDependency>,
) -> Result<(), InvalidStandardTranslationAssetSnapshot> {
    let Some(dependency) = dependency else {
        return Ok(());
    };
    match leaf
        .terminology_dependencies
        .entry(dependency.term().to_owned())
    {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(dependency.translation().to_owned());
            Ok(())
        }
        std::collections::btree_map::Entry::Occupied(entry)
            if entry.get() == dependency.translation() =>
        {
            Err(
                InvalidStandardTranslationAssetSnapshot::DuplicateTerminologyDependency {
                    exact_location: Box::new(exact_location.clone()),
                    term: dependency.term().to_owned(),
                },
            )
        }
        std::collections::btree_map::Entry::Occupied(_) => Err(
            InvalidStandardTranslationAssetSnapshot::ContradictoryTerminologyDependency {
                exact_location: Box::new(exact_location.clone()),
                term: dependency.term().to_owned(),
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use crate::att_mz::ProjectName;
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
        fail: bool,
    }

    impl CpuTaskExecutor for RecordingCpu {
        type Error = FakeError;

        async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
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

    #[test]
    fn config_preserves_explicit_non_zero_limits() {
        let config = MzStandardTranslationAssetReadingConfig::new(non_zero(3), non_zero(12));

        assert_eq!(config.decode_concurrency().get(), 3);
        assert_eq!(config.leaves_per_decode_job().get(), 12);
    }

    #[tokio::test]
    async fn one_union_query_folds_dependencies_and_compound_fields() {
        let item_group = location(vec![MzLocationStep::index(10)]);
        let name = location(vec![MzLocationStep::index(10), MzLocationStep::key("name")]);
        let description = location(vec![
            MzLocationStep::index(10),
            MzLocationStep::key("description"),
        ]);
        let rows = vec![
            row(
                "entry",
                &name,
                &item_group,
                "name",
                [
                    SqliteValue::Text("宝剑".to_owned()),
                    SqliteValue::Text("Sword".to_owned()),
                    SqliteValue::Text("宝剑".to_owned()),
                    SqliteValue::Text("Sword".to_owned()),
                ],
            ),
            row(
                "entry",
                &description,
                &item_group,
                "description",
                [
                    SqliteValue::Text("锋利的宝剑".to_owned()),
                    SqliteValue::Null,
                    SqliteValue::Null,
                    SqliteValue::Null,
                ],
            ),
        ];
        let harness = Harness::new(Ok(rows));
        let service = harness.service(2, 1);

        let corpus = service.read(&project()).await.expect("资产读取应该成功");

        let calls = harness.query_calls.lock().expect("查询调用锁不应中毒");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, PathBuf::from("C:/projects/demo/project.db"));
        assert_eq!(calls[0].1.statement(), READ_STANDARD_ASSETS);
        assert!(calls[0].1.statement().matches("UNION ALL").count() == 4);
        assert_eq!(corpus.groups().len(), 1);
        assert_eq!(corpus.groups()[0].assets().len(), 2);
        assert_eq!(
            corpus.groups()[0].assets()[1].terminology_dependencies(),
            &[TerminologyDependency::new("宝剑", "Sword")]
        );
        assert_eq!(harness.max_cpu_active.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn dependency_without_translation_is_rejected_before_returning_corpus() {
        let item_group = location(vec![MzLocationStep::index(10)]);
        let name = location(vec![MzLocationStep::index(10), MzLocationStep::key("name")]);
        let rows = vec![row(
            "entry",
            &name,
            &item_group,
            "name",
            [
                SqliteValue::Text("宝剑".to_owned()),
                SqliteValue::Null,
                SqliteValue::Text("宝剑".to_owned()),
                SqliteValue::Text("Sword".to_owned()),
            ],
        )];
        let harness = Harness::new(Ok(rows));

        let error = harness
            .service(1, 1)
            .read(&project())
            .await
            .expect_err("无译文依赖应该被拒绝");

        assert!(matches!(
            error,
            MzStandardTranslationAssetReadingError::InvalidSnapshot(
                InvalidStandardTranslationAssetSnapshot::DependencyWithoutTranslation
            )
        ));
    }

    #[tokio::test]
    async fn duplicate_and_contradictory_dependency_rows_are_rejected() {
        for (second_translation, contradiction) in [("Sword", false), ("Blade", true)] {
            let item_group = location(vec![MzLocationStep::index(10)]);
            let name = location(vec![MzLocationStep::index(10), MzLocationStep::key("name")]);
            let dependency_row = |term_translation: &str| {
                row(
                    "entry",
                    &name,
                    &item_group,
                    "name",
                    [
                        SqliteValue::Text("宝剑".to_owned()),
                        SqliteValue::Text("Sword".to_owned()),
                        SqliteValue::Text("宝剑".to_owned()),
                        SqliteValue::Text(term_translation.to_owned()),
                    ],
                )
            };
            let harness = Harness::new(Ok(vec![
                dependency_row("Sword"),
                dependency_row(second_translation),
            ]));

            let error = harness
                .service(1, 10)
                .read(&project())
                .await
                .expect_err("重复或矛盾依赖应该被拒绝");

            assert_eq!(
                matches!(
                    &error,
                    MzStandardTranslationAssetReadingError::InvalidSnapshot(
                        InvalidStandardTranslationAssetSnapshot::ContradictoryTerminologyDependency { .. }
                    )
                ),
                contradiction
            );
            assert_eq!(
                matches!(
                    &error,
                    MzStandardTranslationAssetReadingError::InvalidSnapshot(
                        InvalidStandardTranslationAssetSnapshot::DuplicateTerminologyDependency { .. }
                    )
                ),
                !contradiction
            );
        }
    }

    #[tokio::test]
    async fn query_terminal_states_keep_database_path_and_source() {
        for response in [
            Err(QueryExistingDatabaseError::NotFound),
            Err(QueryExistingDatabaseError::QueryFailed(FakeError("read"))),
        ] {
            let harness = Harness::new(response);
            let error = harness
                .service(1, 1)
                .read(&project())
                .await
                .expect_err("查询错误应该传播");
            match error {
                MzStandardTranslationAssetReadingError::DatabaseNotFound { database_path } => {
                    assert_eq!(database_path, PathBuf::from("C:/projects/demo/project.db"));
                }
                MzStandardTranslationAssetReadingError::Query {
                    database_path,
                    source,
                } => {
                    assert_eq!(database_path, PathBuf::from("C:/projects/demo/project.db"));
                    assert_eq!(source, FakeError("read"));
                }
                other => panic!("未预期的读取错误：{other}"),
            }
        }
    }

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
        ) -> MzStandardTranslationAssetReadingService<RecordingQuery, RecordingCpu> {
            MzStandardTranslationAssetReadingService::new(
                RecordingQuery {
                    calls: Arc::clone(&self.query_calls),
                    response: Arc::clone(&self.response),
                },
                RecordingCpu {
                    calls: Arc::clone(&self.cpu_calls),
                    active: Arc::new(AtomicUsize::new(0)),
                    max_active: Arc::clone(&self.max_cpu_active),
                    fail: false,
                },
                MzStandardTranslationAssetReadingConfig::new(
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
        payload: [SqliteValue; 4],
    ) -> SqliteRow {
        let [original_text, translation, term, term_translation] = payload;
        SqliteRow::new(vec![
            SqliteValue::Text(table.to_owned()),
            SqliteValue::Text(MzLocationCodec::encode(exact_location).expect("位置应可编码")),
            SqliteValue::Text("builtin".to_owned()),
            SqliteValue::Text(MzLocationCodec::encode(group_location).expect("位置应可编码")),
            SqliteValue::Text(field_name.to_owned()),
            SqliteValue::Null,
            original_text,
            translation,
            term,
            term_translation,
        ])
    }

    fn location(steps: Vec<MzLocationStep>) -> MzLocation {
        MzLocation::value(MzSource::data(StandardDataFile::Items), steps)
    }

    fn project() -> StoredProjectRecord {
        StoredProjectRecord::new(
            "demo".parse::<ProjectName>().expect("项目名称应该有效"),
            PathBuf::from("C:/projects/demo"),
            PathBuf::from("C:/projects/demo/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::att_mz::project::test_layout_profile(),
        )
    }

    fn non_zero(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).expect("测试配置必须非零")
    }
}
