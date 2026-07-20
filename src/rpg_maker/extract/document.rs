//! RPG Maker 项目文档的无损读取契约。
//!
//! 读取器只负责按选择加载标准文档并保留完整 JSON；哪些字段属于可翻译文本由
//! Builtin 或 Rules 服务决定。

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::str::Utf8Error;

use futures_util::stream::{self, StreamExt};
use serde_json::{Map, Value};

use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::text::DataFileName;
pub(crate) use crate::rpg_maker::text::StandardDataFile;
use crate::storage::file_system::{
    DirectoryEntryKind, DirectoryLister, FileReader, ListDirectoryError, ReadFileError,
};

/// 一个已加载文档的稳定身份。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum RpgMakerDocumentId {
    Data(StandardDataFile),
    DataFile(DataFileName),
    Map(u32),
}

/// 调用方本次真正需要的 RPG Maker 文档集合。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerDocumentSelection {
    standard_files: BTreeSet<StandardDataFile>,
    data_files: BTreeSet<DataFileName>,
    map_ids: BTreeSet<u32>,
    all_maps: bool,
    plugins: bool,
}

impl RpgMakerDocumentSelection {
    pub(crate) fn new(
        standard_files: impl IntoIterator<Item = StandardDataFile>,
        all_maps: bool,
        plugins: bool,
    ) -> Self {
        Self {
            standard_files: standard_files.into_iter().collect(),
            data_files: BTreeSet::new(),
            map_ids: BTreeSet::new(),
            all_maps,
            plugins,
        }
    }

    pub(crate) fn empty() -> Self {
        Self::new([], false, false)
    }

    pub(crate) fn insert_standard_file(&mut self, file: StandardDataFile) {
        self.standard_files.insert(file);
    }

    /// 请求一个已通过安全基名校验的非标准 `data/*.json` 文档。
    pub(crate) fn insert_data_file(&mut self, file: DataFileName) {
        self.data_files.insert(file);
    }

    pub(crate) fn request_all_maps(&mut self) {
        self.all_maps = true;
    }

    /// 请求一个已由结构化位置确定的精确 Map 文档，不触发目录枚举。
    pub(crate) fn insert_map(&mut self, map_id: u32) {
        self.map_ids.insert(map_id);
    }

    pub(crate) fn request_plugins(&mut self) {
        self.plugins = true;
    }

    pub(crate) fn standard_files(&self) -> &BTreeSet<StandardDataFile> {
        &self.standard_files
    }

    pub(crate) fn data_files(&self) -> &BTreeSet<DataFileName> {
        &self.data_files
    }

    pub(crate) fn includes_all_maps(&self) -> bool {
        self.all_maps
    }

    pub(crate) fn map_ids(&self) -> &BTreeSet<u32> {
        &self.map_ids
    }

    pub(crate) fn includes_plugins(&self) -> bool {
        self.plugins
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.standard_files.is_empty()
            && self.data_files.is_empty()
            && self.map_ids.is_empty()
            && !self.all_maps
            && !self.plugins
    }
}

/// `plugins.js` 中一个插件的无损记录。
///
/// 这里保留完整对象，插件名称、启用状态和参数结构由真正消费它们的 Rules 模块
/// 解释；写回会修改参数叶后重新序列化整条记录，因此读取边界必须无损保留当前
/// 不参与 Rules 判断的其余字段。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PluginConfiguration {
    index: usize,
    fields: Map<String, Value>,
}

impl PluginConfiguration {
    pub(crate) fn new(index: usize, fields: Map<String, Value>) -> Self {
        Self { index, fields }
    }

    pub(crate) fn index(&self) -> usize {
        self.index
    }

    #[cfg(test)]
    pub(crate) fn fields(&self) -> &Map<String, Value> {
        &self.fields
    }

    pub(crate) fn into_parts(self) -> (usize, Map<String, Value>) {
        (self.index, self.fields)
    }
}

/// 一次读取所得的完整、无损 RPG Maker 文档集合。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerProjectDocuments {
    documents: BTreeMap<RpgMakerDocumentId, Value>,
    plugins: Vec<PluginConfiguration>,
}

impl RpgMakerProjectDocuments {
    pub(crate) fn new(
        documents: BTreeMap<RpgMakerDocumentId, Value>,
        mut plugins: Vec<PluginConfiguration>,
    ) -> Self {
        plugins.sort_by_key(PluginConfiguration::index);
        Self { documents, plugins }
    }

    pub(crate) fn empty() -> Self {
        Self::new(BTreeMap::new(), Vec::new())
    }

    pub(crate) fn document(&self, id: RpgMakerDocumentId) -> Option<&Value> {
        self.documents.get(&id)
    }

    #[cfg(test)]
    pub(crate) fn insert_document(
        &mut self,
        id: RpgMakerDocumentId,
        value: Value,
    ) -> Option<Value> {
        self.documents.insert(id, value)
    }

    pub(crate) fn documents(&self) -> &BTreeMap<RpgMakerDocumentId, Value> {
        &self.documents
    }

    #[cfg(test)]
    pub(crate) fn plugins(&self) -> &[PluginConfiguration] {
        &self.plugins
    }

    /// 将完整文档集拆成可移动到独立 CPU 工作单元的拥有型部分。
    pub(crate) fn into_parts(
        self,
    ) -> (
        BTreeMap<RpgMakerDocumentId, Value>,
        Vec<PluginConfiguration>,
    ) {
        (self.documents, self.plugins)
    }
}

/// 按调用方选择无损读取 RPG Maker JSON 和 `plugins.js`。
///
/// 实现负责稳定定位、读取与解析所选文件，并返回完整 `serde_json::Value`。未选择
/// 的文件不应被读取；未知字段不得丢弃；非标准 `data/*.json` 只能通过已校验的
/// 精确基名读取，不列举或猜测来源。
pub(crate) trait RpgMakerProjectDocumentReader: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn read(
        &self,
        project: &OpenedProject,
        selection: RpgMakerDocumentSelection,
    ) -> impl Future<Output = Result<RpgMakerProjectDocuments, Self::Error>> + Send;
}

/// RPG Maker 文档读取阶段的外部配置。
///
/// 读取上限必须由组合根显式提供；CPU 解析并行度统一由 CPU 根执行器管理。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerDocumentReadingConfig {
    read_concurrency: NonZeroUsize,
}

impl RpgMakerDocumentReadingConfig {
    pub(crate) const fn new(read_concurrency: NonZeroUsize) -> Self {
        Self { read_concurrency }
    }

    pub(crate) const fn read_concurrency(self) -> NonZeroUsize {
        self.read_concurrency
    }
}

/// 通过异步文件根能力与 CPU 根执行器无损读取标准 RPG Maker 文档。
pub(crate) struct RpgMakerProjectDocumentReadingService<F, L, C> {
    file_reader: F,
    directory_lister: L,
    cpu_executor: C,
    config: RpgMakerDocumentReadingConfig,
}

impl<F, L, C> RpgMakerProjectDocumentReadingService<F, L, C> {
    pub(crate) fn new(
        file_reader: F,
        directory_lister: L,
        cpu_executor: C,
        config: RpgMakerDocumentReadingConfig,
    ) -> Self {
        Self {
            file_reader,
            directory_lister,
            cpu_executor,
            config,
        }
    }
}

impl<F, L, C> RpgMakerProjectDocumentReader for RpgMakerProjectDocumentReadingService<F, L, C>
where
    F: FileReader,
    L: DirectoryLister,
    C: CpuTaskExecutor,
{
    type Error = RpgMakerProjectDocumentReadingError<F::Error, L::Error, C::Error>;

    async fn read(
        &self,
        project: &OpenedProject,
        selection: RpgMakerDocumentSelection,
    ) -> Result<RpgMakerProjectDocuments, Self::Error> {
        if selection.is_empty() {
            return Ok(RpgMakerProjectDocuments::empty());
        }

        let requests = self
            .build_requests(
                project.layout().source_data(),
                project.layout().source_js(),
                selection,
            )
            .await
            .map_err(RpgMakerProjectDocumentReadingError::ListMaps)?;
        let read_concurrency = self.config.read_concurrency().get();
        // 第二层只覆盖本次有限请求集合，使读取完成后立即进入 CPU 根的准入等待；
        // 实际执行与已接管队列仍由 CPU 根的统一预算约束。
        let parse_submission_capacity = requests.len().max(1);
        let reads = stream::iter(requests.into_iter().enumerate().map(
            |(request_index, request)| async move {
                let requested_path = request.path.clone();
                let result = self
                    .file_reader
                    .read_file(requested_path.clone())
                    .await
                    .map(|file| (request.kind, file))
                    .map_err(|source| RpgMakerProjectDocumentReadingError::ReadDocument {
                        path: requested_path,
                        source,
                    });
                (request_index, result)
            },
        ))
        .buffer_unordered(read_concurrency);

        let work = reads
            .map(|(request_index, read_result)| async move {
                let result = async {
                    let (kind, file) = read_result?;
                    let resolved_path = file.resolved_path().to_path_buf();
                    let schedule_path = resolved_path.clone();
                    let bytes = file.into_bytes();
                    let parsed = self
                        .cpu_executor
                        .execute(move || parse_document(kind, resolved_path, bytes))
                        .await
                        .map_err(
                            |source| RpgMakerProjectDocumentReadingError::ScheduleParse {
                                path: schedule_path,
                                source,
                            },
                        )?;
                    parsed.map_err(RpgMakerProjectDocumentReadingError::from_parse_failure)
                }
                .await;
                (request_index, result)
            })
            .buffer_unordered(parse_submission_capacity);

        let mut completed = work.collect::<Vec<_>>().await;
        completed.sort_by_key(|(request_index, _)| *request_index);
        let mut documents = BTreeMap::new();
        let mut plugins = Vec::new();
        for (_, result) in completed {
            match result? {
                ParsedDocument::Json { id, value } => {
                    documents.insert(id, value);
                }
                ParsedDocument::Plugins(records) => plugins = records,
            }
        }

        Ok(RpgMakerProjectDocuments::new(documents, plugins))
    }
}

impl<F, L, C> RpgMakerProjectDocumentReadingService<F, L, C>
where
    L: DirectoryLister,
{
    async fn build_requests(
        &self,
        data_root: &Path,
        js_root: &Path,
        selection: RpgMakerDocumentSelection,
    ) -> Result<Vec<DocumentRequest>, ListDirectoryError<L::Error>> {
        let mut documents = BTreeMap::new();
        for file in selection.standard_files() {
            documents.insert(
                RpgMakerDocumentId::Data(*file),
                data_root.join(file.file_name()),
            );
        }
        for file in selection.data_files() {
            documents.insert(
                RpgMakerDocumentId::DataFile(file.clone()),
                data_root.join(file.as_str()),
            );
        }
        for map_id in selection.map_ids() {
            documents.insert(
                RpgMakerDocumentId::Map(*map_id),
                data_root.join(format!("Map{map_id:03}.json")),
            );
        }

        if selection.includes_all_maps() {
            let entries = self
                .directory_lister
                .list_directory(data_root.to_path_buf())
                .await?;
            for entry in entries {
                if entry.kind() != DirectoryEntryKind::RegularFile {
                    continue;
                }
                let path = entry.into_path();
                if let Some(map_id) = canonical_map_id(&path) {
                    documents.insert(RpgMakerDocumentId::Map(map_id), path);
                }
            }
        }

        let mut requests: Vec<_> = documents
            .into_iter()
            .map(|(id, path)| DocumentRequest {
                kind: DocumentRequestKind::Json(id),
                path,
            })
            .collect();
        if selection.includes_plugins() {
            requests.push(DocumentRequest {
                kind: DocumentRequestKind::Plugins,
                path: js_root.join("plugins.js"),
            });
        }
        Ok(requests)
    }
}

/// 无损读取 RPG Maker 文档时的阶段错误。
#[derive(Debug)]
pub(crate) enum RpgMakerProjectDocumentReadingError<F, L, C> {
    ListMaps(ListDirectoryError<L>),
    ReadDocument {
        path: PathBuf,
        source: ReadFileError<F>,
    },
    ScheduleParse {
        path: PathBuf,
        source: CpuTaskExecutionError<C>,
    },
    InvalidUtf8 {
        path: PathBuf,
        source: Utf8Error,
    },
    InvalidJson {
        path: PathBuf,
        source: serde_json::Error,
    },
    InvalidPluginsEnvelope {
        path: PathBuf,
    },
    InvalidPluginRecord {
        path: PathBuf,
        index: usize,
    },
}

impl<F, L, C> RpgMakerProjectDocumentReadingError<F, L, C> {
    fn from_parse_failure(error: ParseFailure) -> Self {
        match error {
            ParseFailure::InvalidUtf8 { path, source } => Self::InvalidUtf8 { path, source },
            ParseFailure::InvalidJson { path, source } => Self::InvalidJson { path, source },
            ParseFailure::InvalidPluginsEnvelope { path } => Self::InvalidPluginsEnvelope { path },
            ParseFailure::InvalidPluginRecord { path, index } => {
                Self::InvalidPluginRecord { path, index }
            }
        }
    }
}

impl<F, L, C> fmt::Display for RpgMakerProjectDocumentReadingError<F, L, C>
where
    F: fmt::Display,
    L: fmt::Display,
    C: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ListMaps(error) => write!(formatter, "无法发现 RPG Maker 地图文件：{error}"),
            Self::ReadDocument { path, source } => {
                write!(
                    formatter,
                    "无法读取 RPG Maker 文档 {}：{source}",
                    path.display()
                )
            }
            Self::ScheduleParse { path, source } => write!(
                formatter,
                "无法调度 RPG Maker 文档 {} 的解析任务：{source}",
                path.display()
            ),
            Self::InvalidUtf8 { path, source } => {
                write!(
                    formatter,
                    "RPG Maker 文档 {} 不是有效 UTF-8：{source}",
                    path.display()
                )
            }
            Self::InvalidJson { path, source } => {
                write!(
                    formatter,
                    "RPG Maker 文档 {} 不是有效 JSON：{source}",
                    path.display()
                )
            }
            Self::InvalidPluginsEnvelope { path } => write!(
                formatter,
                "{} 不是 RPG Maker 生成的 plugins.js 格式",
                path.display()
            ),
            Self::InvalidPluginRecord { path, index } => write!(
                formatter,
                "{} 中索引 {index} 的插件记录不是对象",
                path.display()
            ),
        }
    }
}

impl<F, L, C> Error for RpgMakerProjectDocumentReadingError<F, L, C>
where
    F: Error + 'static,
    L: Error + 'static,
    C: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ListMaps(error) => Some(error),
            Self::ReadDocument { source, .. } => Some(source),
            Self::ScheduleParse { source, .. } => Some(source),
            Self::InvalidUtf8 { source, .. } => Some(source),
            Self::InvalidJson { source, .. } => Some(source),
            Self::InvalidPluginsEnvelope { .. } | Self::InvalidPluginRecord { .. } => None,
        }
    }
}

struct DocumentRequest {
    kind: DocumentRequestKind,
    path: PathBuf,
}

#[derive(Clone)]
enum DocumentRequestKind {
    Json(RpgMakerDocumentId),
    Plugins,
}

enum ParsedDocument {
    Json {
        id: RpgMakerDocumentId,
        value: Value,
    },
    Plugins(Vec<PluginConfiguration>),
}

enum ParseFailure {
    InvalidUtf8 {
        path: PathBuf,
        source: Utf8Error,
    },
    InvalidJson {
        path: PathBuf,
        source: serde_json::Error,
    },
    InvalidPluginsEnvelope {
        path: PathBuf,
    },
    InvalidPluginRecord {
        path: PathBuf,
        index: usize,
    },
}

fn parse_document(
    kind: DocumentRequestKind,
    path: PathBuf,
    bytes: Vec<u8>,
) -> Result<ParsedDocument, ParseFailure> {
    let text = std::str::from_utf8(&bytes).map_err(|source| ParseFailure::InvalidUtf8 {
        path: path.clone(),
        source,
    })?;

    match kind {
        DocumentRequestKind::Json(id) => {
            let value = serde_json::from_str(text)
                .map_err(|source| ParseFailure::InvalidJson { path, source })?;
            Ok(ParsedDocument::Json { id, value })
        }
        DocumentRequestKind::Plugins => parse_plugins(path, text),
    }
}

fn parse_plugins(path: PathBuf, text: &str) -> Result<ParsedDocument, ParseFailure> {
    let Some((prefix, assignment)) = text.split_once("var $plugins") else {
        return Err(ParseFailure::InvalidPluginsEnvelope { path });
    };
    if !prefix
        .lines()
        .all(|line| line.trim().is_empty() || line.trim_start().starts_with("//"))
    {
        return Err(ParseFailure::InvalidPluginsEnvelope { path });
    }
    let Some(json_with_terminator) = assignment.trim_start().strip_prefix('=') else {
        return Err(ParseFailure::InvalidPluginsEnvelope { path });
    };
    let Some(json) = json_with_terminator.trim().strip_suffix(';') else {
        return Err(ParseFailure::InvalidPluginsEnvelope { path });
    };

    let values: Vec<Value> =
        serde_json::from_str(json.trim()).map_err(|source| ParseFailure::InvalidJson {
            path: path.clone(),
            source,
        })?;
    let mut plugins = Vec::with_capacity(values.len());
    for (index, value) in values.into_iter().enumerate() {
        let Value::Object(fields) = value else {
            return Err(ParseFailure::InvalidPluginRecord { path, index });
        };
        plugins.push(PluginConfiguration::new(index, fields));
    }
    Ok(ParsedDocument::Plugins(plugins))
}

fn canonical_map_id(path: &Path) -> Option<u32> {
    let file_name = path.file_name()?.to_str()?;
    let digits = file_name.strip_prefix("Map")?.strip_suffix(".json")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let id = digits.parse::<u32>().ok()?;
    (format!("Map{id:03}.json") == file_name).then_some(id)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};

    use crate::rpg_maker::ProjectName;
    use crate::storage::file_system::ReadFile;

    use super::*;

    #[test]
    fn standard_file_names_round_trip_without_accepting_nonstandard_files() {
        for file in StandardDataFile::ALL {
            assert_eq!(
                StandardDataFile::from_file_name(file.file_name()),
                Some(file)
            );
        }
        assert_eq!(StandardDataFile::from_file_name("QuestData.json"), None);
        assert_eq!(StandardDataFile::from_file_name("Map001.json"), None);
    }

    #[test]
    fn plugin_records_remain_in_source_order_and_keep_unknown_fields() {
        let documents = RpgMakerProjectDocuments::new(
            BTreeMap::new(),
            vec![
                PluginConfiguration::new(
                    8,
                    serde_json::from_value(serde_json::json!({
                        "name": "Later",
                        "futureField": {"kept": true}
                    }))
                    .expect("插件测试对象应该有效"),
                ),
                PluginConfiguration::new(
                    2,
                    serde_json::from_value(serde_json::json!({
                        "name": "Earlier",
                        "description": "保留"
                    }))
                    .expect("插件测试对象应该有效"),
                ),
            ],
        );

        assert_eq!(documents.plugins()[0].fields()["name"], "Earlier");
        assert_eq!(documents.plugins()[0].fields()["description"], "保留");
        assert_eq!(documents.plugins()[1].fields()["name"], "Later");
        assert_eq!(documents.plugins()[1].fields()["futureField"]["kept"], true);
    }

    #[test]
    fn only_canonical_map_file_names_are_recognized() {
        assert_eq!(canonical_map_id(Path::new("Map001.json")), Some(1));
        assert_eq!(canonical_map_id(Path::new("Map999.json")), Some(999));
        assert_eq!(canonical_map_id(Path::new("Map1000.json")), Some(1000));
        assert_eq!(canonical_map_id(Path::new("Map01.json")), None);
        assert_eq!(canonical_map_id(Path::new("Map0001.json")), None);
        assert_eq!(canonical_map_id(Path::new("MapInfos.json")), None);
        assert_eq!(canonical_map_id(Path::new("QuestData.json")), None);
    }

    #[tokio::test]
    async fn empty_selection_does_not_touch_any_root_capability() {
        let harness = Harness::new(HashMap::new(), Vec::new(), 2);

        let documents = harness
            .service()
            .read(&project(), RpgMakerDocumentSelection::empty())
            .await
            .expect("空选择应该直接成功");

        assert!(documents.documents().is_empty());
        assert!(documents.plugins().is_empty());
        assert_eq!(harness.file_calls.load(Ordering::SeqCst), 0);
        assert_eq!(harness.list_calls.load(Ordering::SeqCst), 0);
        assert_eq!(harness.cpu_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn selected_documents_keep_bounded_reads_and_submit_parses_as_reads_finish() {
        let root = project().source_root().to_path_buf();
        let actors = root.join("data").join("Actors.json");
        let map_one = root.join("data").join("Map001.json");
        let map_thousand = root.join("data").join("Map1000.json");
        let plugins = root.join("js").join("plugins.js");
        let files = HashMap::from([
            (
                actors.clone(),
                r#"[null,{"name":"  勇者  ","unknown":{"kept":true}}]"#
                    .as_bytes()
                    .to_vec(),
            ),
            (
                map_one.clone(),
                r#"{"displayName":"村庄"}"#.as_bytes().to_vec(),
            ),
            (
                map_thousand.clone(),
                r#"{"displayName":"隐藏地图"}"#.as_bytes().to_vec(),
            ),
            (
                plugins.clone(),
                r#"// Generated by RPG Maker.
var $plugins =
[
{"name":"QuestMenu","status":false,"description":"保留","parameters":{"Title":"任务"},"future":{"kept":true}}
];"#
                    .as_bytes()
                    .to_vec(),
            ),
        ]);
        let entries = vec![
            map_thousand,
            root.join("data").join("Map0001.json"),
            root.join("data").join("MapInfos.json"),
            root.join("data").join("QuestData.json"),
            map_one,
        ];
        let harness = Harness::new(files, entries, 3);
        let selection = RpgMakerDocumentSelection::new([StandardDataFile::Actors], true, true);

        let documents = harness
            .service()
            .read(&project(), selection)
            .await
            .expect("规范文档应该成功读取");

        assert_eq!(harness.list_calls.load(Ordering::SeqCst), 1);
        assert_eq!(harness.file_calls.load(Ordering::SeqCst), 4);
        assert!(harness.max_file_active.load(Ordering::SeqCst) > 1);
        assert!(harness.max_file_active.load(Ordering::SeqCst) <= 3);
        assert!(harness.max_cpu_active.load(Ordering::SeqCst) > 1);
        assert_eq!(harness.cpu_calls.load(Ordering::SeqCst), 4);
        assert_eq!(
            documents
                .document(RpgMakerDocumentId::Data(StandardDataFile::Actors))
                .expect("Actors 应该存在")[1]["name"],
            "  勇者  "
        );
        assert_eq!(
            documents
                .document(RpgMakerDocumentId::Data(StandardDataFile::Actors))
                .expect("Actors 应该存在")[1]["unknown"]["kept"],
            true
        );
        assert_eq!(
            documents
                .document(RpgMakerDocumentId::Map(1))
                .expect("Map001 应该存在")["displayName"],
            "村庄"
        );
        assert_eq!(
            documents
                .document(RpgMakerDocumentId::Map(1000))
                .expect("Map1000 应该存在")["displayName"],
            "隐藏地图"
        );
        assert_eq!(documents.plugins().len(), 1);
        assert_eq!(documents.plugins()[0].index(), 0);
        assert_eq!(documents.plugins()[0].fields()["status"], false);
        assert_eq!(documents.plugins()[0].fields()["future"]["kept"], true);
    }

    #[tokio::test]
    async fn exact_map_selection_reads_only_that_map_without_listing_data() {
        let root = project().source_root().to_path_buf();
        let map = root.join("data").join("Map042.json");
        let harness = Harness::new(
            HashMap::from([(map, r#"{"displayName":"精确地图"}"#.as_bytes().to_vec())]),
            Vec::new(),
            1,
        );
        let mut selection = RpgMakerDocumentSelection::empty();
        selection.insert_map(42);

        let documents = harness
            .service()
            .read(&project(), selection)
            .await
            .expect("精确 Map 选择应该成功");

        assert_eq!(harness.list_calls.load(Ordering::SeqCst), 0);
        assert_eq!(harness.file_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            documents
                .document(RpgMakerDocumentId::Map(42))
                .expect("Map042 应被读取")["displayName"],
            "精确地图"
        );
    }

    #[tokio::test]
    async fn exact_nonstandard_data_file_is_read_without_directory_enumeration() {
        let root = project().source_root().to_path_buf();
        let file = DataFileName::parse("Disciplines.json").expect("安全基名应合法");
        let path = root.join("data").join(file.as_str());
        let harness = Harness::new(
            HashMap::from([(path, r#"[{"Name":"Baking"}]"#.as_bytes().to_vec())]),
            Vec::new(),
            1,
        );
        let mut selection = RpgMakerDocumentSelection::empty();
        selection.insert_data_file(file.clone());

        let documents = harness
            .service()
            .read(&project(), selection)
            .await
            .expect("非标准 JSON 应按精确基名读取");

        assert_eq!(harness.list_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            documents
                .document(RpgMakerDocumentId::DataFile(file))
                .expect("自定义 data 文档应存在")[0]["Name"],
            "Baking"
        );
    }

    #[tokio::test]
    async fn invalid_utf8_and_invalid_plugin_record_keep_precise_stage() {
        let root = project().source_root().to_path_buf();
        let actors = root.join("data").join("Actors.json");
        let invalid_utf8 =
            Harness::new(HashMap::from([(actors.clone(), vec![0xff])]), Vec::new(), 1);

        let error = invalid_utf8
            .service()
            .read(
                &project(),
                RpgMakerDocumentSelection::new([StandardDataFile::Actors], false, false),
            )
            .await
            .expect_err("非法 UTF-8 应该失败");
        assert!(matches!(
            error,
            RpgMakerProjectDocumentReadingError::InvalidUtf8 { path, .. } if path == actors
        ));

        let plugins = root.join("js").join("plugins.js");
        let invalid_record = Harness::new(
            HashMap::from([(plugins.clone(), b"var $plugins = [null];".to_vec())]),
            Vec::new(),
            1,
        );
        let error = invalid_record
            .service()
            .read(&project(), RpgMakerDocumentSelection::new([], false, true))
            .await
            .expect_err("插件记录必须是对象");
        assert!(matches!(
            error,
            RpgMakerProjectDocumentReadingError::InvalidPluginRecord { path, index: 0 }
                if path == plugins
        ));
    }

    #[tokio::test]
    async fn malformed_plugins_envelope_is_not_treated_as_generic_json() {
        let plugins = project().source_root().join("js").join("plugins.js");
        let harness = Harness::new(
            HashMap::from([(plugins.clone(), b"const plugins = [];".to_vec())]),
            Vec::new(),
            1,
        );

        let error = harness
            .service()
            .read(&project(), RpgMakerDocumentSelection::new([], false, true))
            .await
            .expect_err("非官方外壳应该明确失败");

        assert!(matches!(
            error,
            RpgMakerProjectDocumentReadingError::InvalidPluginsEnvelope { path } if path == plugins
        ));
    }

    #[tokio::test]
    async fn cpu_panic_is_not_confused_with_a_document_parse_error() {
        let actors = project().source_root().join("data").join("Actors.json");
        let service = RpgMakerProjectDocumentReadingService::new(
            FakeFileReader {
                files: Arc::new(HashMap::from([(
                    actors.clone(),
                    r#"[null,{"name":"勇者"}]"#.as_bytes().to_vec(),
                )])),
                calls: Arc::new(AtomicUsize::new(0)),
                active: Arc::new(AtomicUsize::new(0)),
                max_active: Arc::new(AtomicUsize::new(0)),
            },
            FakeDirectoryLister {
                entries: Arc::new(Vec::new()),
                calls: Arc::new(AtomicUsize::new(0)),
            },
            PanickedCpuExecutor,
            RpgMakerDocumentReadingConfig::new(NonZeroUsize::new(1).expect("测试读取并发必须非零")),
        );

        let error = service
            .read(
                &project(),
                RpgMakerDocumentSelection::new([StandardDataFile::Actors], false, false),
            )
            .await
            .expect_err("CPU panic 必须作为调度错误返回");

        assert!(matches!(
            error,
            RpgMakerProjectDocumentReadingError::ScheduleParse {
                path,
                source: CpuTaskExecutionError::TaskPanicked
            } if path == actors
        ));
    }

    #[test]
    fn reading_future_is_send() {
        fn assert_send(_: impl Send) {}

        let harness = Harness::new(HashMap::new(), Vec::new(), 1);
        let service = harness.service();
        let project = project();
        assert_send(service.read(&project, RpgMakerDocumentSelection::empty()));
    }

    #[derive(Clone, Debug)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeError {}

    #[derive(Clone)]
    struct FakeFileReader {
        files: Arc<HashMap<PathBuf, Vec<u8>>>,
        calls: Arc<AtomicUsize>,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
    }

    impl FileReader for FakeFileReader {
        type Error = FakeError;

        async fn read_file(&self, path: PathBuf) -> Result<ReadFile, ReadFileError<Self::Error>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            update_max(&self.max_active, active);
            YieldOnce::new().await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            let Some(bytes) = self.files.get(&path) else {
                return Err(ReadFileError::NotFound { path });
            };
            Ok(ReadFile::new(path, bytes.clone()))
        }
    }

    #[derive(Clone)]
    struct FakeDirectoryLister {
        entries: Arc<Vec<PathBuf>>,
        calls: Arc<AtomicUsize>,
    }

    impl DirectoryLister for FakeDirectoryLister {
        type Error = FakeError;

        async fn list_directory(
            &self,
            _path: PathBuf,
        ) -> Result<Vec<crate::storage::file_system::DirectoryEntry>, ListDirectoryError<Self::Error>>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .entries
                .iter()
                .cloned()
                .map(|path| {
                    crate::storage::file_system::DirectoryEntry::new(
                        path,
                        DirectoryEntryKind::RegularFile,
                    )
                })
                .collect())
        }
    }

    #[derive(Clone)]
    struct FakeCpuExecutor {
        calls: Arc<AtomicUsize>,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
    }

    impl CpuTaskExecutor for FakeCpuExecutor {
        type Error = FakeError;

        async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            update_max(&self.max_active, active);
            YieldOnce::new().await;
            let result = task();
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(result)
        }
    }

    #[derive(Clone, Copy)]
    struct PanickedCpuExecutor;

    impl CpuTaskExecutor for PanickedCpuExecutor {
        type Error = FakeError;

        async fn execute<T, F>(&self, _task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            Err(CpuTaskExecutionError::TaskPanicked)
        }
    }

    struct Harness {
        files: Arc<HashMap<PathBuf, Vec<u8>>>,
        entries: Arc<Vec<PathBuf>>,
        file_calls: Arc<AtomicUsize>,
        list_calls: Arc<AtomicUsize>,
        cpu_calls: Arc<AtomicUsize>,
        file_active: Arc<AtomicUsize>,
        max_file_active: Arc<AtomicUsize>,
        cpu_active: Arc<AtomicUsize>,
        max_cpu_active: Arc<AtomicUsize>,
        read_concurrency: NonZeroUsize,
    }

    impl Harness {
        fn new(
            files: HashMap<PathBuf, Vec<u8>>,
            entries: Vec<PathBuf>,
            read_concurrency: usize,
        ) -> Self {
            Self {
                files: Arc::new(files),
                entries: Arc::new(entries),
                file_calls: Arc::new(AtomicUsize::new(0)),
                list_calls: Arc::new(AtomicUsize::new(0)),
                cpu_calls: Arc::new(AtomicUsize::new(0)),
                file_active: Arc::new(AtomicUsize::new(0)),
                max_file_active: Arc::new(AtomicUsize::new(0)),
                cpu_active: Arc::new(AtomicUsize::new(0)),
                max_cpu_active: Arc::new(AtomicUsize::new(0)),
                read_concurrency: NonZeroUsize::new(read_concurrency)
                    .expect("测试读取并发必须非零"),
            }
        }

        fn service(
            &self,
        ) -> RpgMakerProjectDocumentReadingService<
            FakeFileReader,
            FakeDirectoryLister,
            FakeCpuExecutor,
        > {
            RpgMakerProjectDocumentReadingService::new(
                FakeFileReader {
                    files: Arc::clone(&self.files),
                    calls: Arc::clone(&self.file_calls),
                    active: Arc::clone(&self.file_active),
                    max_active: Arc::clone(&self.max_file_active),
                },
                FakeDirectoryLister {
                    entries: Arc::clone(&self.entries),
                    calls: Arc::clone(&self.list_calls),
                },
                FakeCpuExecutor {
                    calls: Arc::clone(&self.cpu_calls),
                    active: Arc::clone(&self.cpu_active),
                    max_active: Arc::clone(&self.max_cpu_active),
                },
                RpgMakerDocumentReadingConfig::new(self.read_concurrency),
            )
        }
    }

    struct YieldOnce {
        yielded: bool,
    }

    impl YieldOnce {
        const fn new() -> Self {
            Self { yielded: false }
        }
    }

    impl Future for YieldOnce {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
            if self.yielded {
                Poll::Ready(())
            } else {
                self.yielded = true;
                context.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    fn update_max(maximum: &AtomicUsize, candidate: usize) {
        maximum.fetch_max(candidate, Ordering::SeqCst);
    }

    fn project() -> OpenedProject {
        OpenedProject::new(
            "demo".parse::<ProjectName>().expect("项目名称应该有效"),
            PathBuf::from("C:/projects/demo"),
            PathBuf::from("C:/projects/demo/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::rpg_maker::project::test_layout_profile(),
        )
    }
}
