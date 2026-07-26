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
use std::sync::Arc;

use futures_util::stream::{self, StreamExt};
use serde_json::{Map, Value};

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, RecoveryFact, SafeDiagnostic, SafeDiagnosticSource,
};
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::rpg_maker::json::{StackSafeJsonError, StackSafeJsonValue, from_str as parse_json};
use crate::rpg_maker::project::OpenedProject;
pub(crate) use crate::rpg_maker::text::StandardDataFile;
use crate::rpg_maker::text::{DataFileName, MapId, RpgMakerSource};
use crate::storage::file_system::{
    DirectoryEntryKind, DirectoryLister, FileReader, ListDirectoryError, ReadFileError,
};
use crate::storage::scoped_path::{ExactPathCaseMismatch, resolve_exact_directory_entry};

/// 一个已加载文档的稳定身份。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum RpgMakerDocumentId {
    Data(StandardDataFile),
    DataFile(DataFileName),
    Map(MapId),
}

/// 调用方本次真正需要的 RPG Maker 文档集合。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerDocumentSelection {
    standard_files: BTreeSet<StandardDataFile>,
    data_files: BTreeSet<DataFileName>,
    map_ids: BTreeSet<MapId>,
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

    /// 请求一个已通过安全基名校验的精确 `data/*.json` 文档，并统一收敛其
    /// 标准文件、规范 Map 或自定义文件身份。
    pub(crate) fn insert_data_file(&mut self, file: DataFileName) {
        match RpgMakerSource::data_file(file) {
            RpgMakerSource::Data(file) => {
                self.standard_files.insert(file);
            }
            RpgMakerSource::DataFile(file) => {
                self.data_files.insert(file);
            }
            RpgMakerSource::Map(map_id) => {
                self.map_ids.insert(map_id);
            }
            RpgMakerSource::PluginParameter { .. } => {
                unreachable!("DataFileName 不会产生插件参数来源")
            }
        }
    }

    pub(crate) fn request_all_maps(&mut self) {
        self.all_maps = true;
    }

    /// 请求一个已由结构化位置确定的精确 Map 文档。
    pub(crate) fn insert_map(&mut self, map_id: MapId) {
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

    pub(crate) fn map_ids(&self) -> &BTreeSet<MapId> {
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
    fields: StackSafeJsonValue,
}

impl PluginConfiguration {
    pub(crate) fn new(index: usize, fields: Map<String, Value>) -> Self {
        Self {
            index,
            fields: StackSafeJsonValue::new(Value::Object(fields)),
        }
    }

    pub(crate) fn index(&self) -> usize {
        self.index
    }

    #[cfg(test)]
    pub(crate) fn fields(&self) -> &Map<String, Value> {
        self.fields
            .as_object()
            .expect("插件记录在解析边界已保证是对象")
    }

    pub(crate) fn into_parts(self) -> (usize, StackSafeJsonValue) {
        (self.index, self.fields)
    }
}

/// 一次读取所得的完整、无损 RPG Maker 文档集合。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerProjectDocuments {
    documents: BTreeMap<RpgMakerDocumentId, Arc<StackSafeJsonValue>>,
    plugins: Vec<PluginConfiguration>,
}

impl RpgMakerProjectDocuments {
    pub(crate) fn new(
        documents: BTreeMap<RpgMakerDocumentId, Value>,
        mut plugins: Vec<PluginConfiguration>,
    ) -> Self {
        plugins.sort_by_key(PluginConfiguration::index);
        Self {
            documents: documents
                .into_iter()
                .map(|(id, value)| (id, Arc::new(StackSafeJsonValue::new(value))))
                .collect(),
            plugins,
        }
    }

    pub(crate) fn from_stack_safe_parts(
        documents: BTreeMap<RpgMakerDocumentId, StackSafeJsonValue>,
        mut plugins: Vec<PluginConfiguration>,
    ) -> Self {
        plugins.sort_by_key(PluginConfiguration::index);
        Self {
            documents: documents
                .into_iter()
                .map(|(id, value)| (id, Arc::new(value)))
                .collect(),
            plugins,
        }
    }

    pub(crate) fn from_shared_parts(
        documents: BTreeMap<RpgMakerDocumentId, Arc<StackSafeJsonValue>>,
        mut plugins: Vec<PluginConfiguration>,
    ) -> Self {
        plugins.sort_by_key(PluginConfiguration::index);
        Self { documents, plugins }
    }

    pub(crate) fn empty() -> Self {
        Self::new(BTreeMap::new(), Vec::new())
    }

    pub(crate) fn document(&self, id: RpgMakerDocumentId) -> Option<&Value> {
        self.documents.get(&id).map(|value| value.as_ref().as_ref())
    }

    #[cfg(test)]
    pub(crate) fn insert_document(&mut self, id: RpgMakerDocumentId, value: Value) {
        drop(
            self.documents
                .insert(id, Arc::new(StackSafeJsonValue::new(value))),
        );
    }

    pub(crate) fn documents(&self) -> &BTreeMap<RpgMakerDocumentId, Arc<StackSafeJsonValue>> {
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
        BTreeMap<RpgMakerDocumentId, StackSafeJsonValue>,
        Vec<PluginConfiguration>,
    ) {
        let documents = self
            .documents
            .into_iter()
            .map(|(id, value)| {
                let value = Arc::try_unwrap(value).unwrap_or_else(|shared| shared.as_ref().clone());
                (id, value)
            })
            .collect();
        (documents, self.plugins)
    }

    /// 将共享解析根拆成可移动到并行 CPU 工作单元的轻量引用。
    pub(crate) fn into_shared_parts(
        self,
    ) -> (
        BTreeMap<RpgMakerDocumentId, Arc<StackSafeJsonValue>>,
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

    /// 在文档请求集合建立后报告真实分母，并在每份文档完成读取与解析后
    /// 报告绝对完成数。不需要局部进度的调用方继续使用 `read`。
    fn read_with_progress(
        &self,
        project: &OpenedProject,
        selection: RpgMakerDocumentSelection,
        progress: DocumentReadProgress,
    ) -> impl Future<Output = Result<RpgMakerProjectDocuments, Self::Error>> + Send {
        let _ = progress;
        self.read(project, selection)
    }
}

/// 在一次 Extract 命令内让多个 owner 复用已经成功解析的文档。
///
/// 包装器只缓存成功结果；失败读取不会污染后续状态。Builtin 与 Rules 仍由上层
/// 串行调度，因此 owner 的提交和错误顺序不变；若内部调用方并发请求，则缓存锁
/// 会让第二个请求在第一个成功后只补读缺失文档。
pub(crate) struct CommandScopedRpgMakerDocumentReader<R> {
    reader: Arc<R>,
    cache: Arc<tokio::sync::Mutex<CommandDocumentCache>>,
}

impl<R> CommandScopedRpgMakerDocumentReader<R> {
    pub(crate) fn new(reader: R) -> Self {
        Self {
            reader: Arc::new(reader),
            cache: Arc::new(tokio::sync::Mutex::new(CommandDocumentCache::default())),
        }
    }
}

impl<R> Clone for CommandScopedRpgMakerDocumentReader<R> {
    fn clone(&self) -> Self {
        Self {
            reader: Arc::clone(&self.reader),
            cache: Arc::clone(&self.cache),
        }
    }
}

impl<R> RpgMakerProjectDocumentReader for CommandScopedRpgMakerDocumentReader<R>
where
    R: RpgMakerProjectDocumentReader,
{
    type Error = R::Error;

    async fn read(
        &self,
        project: &OpenedProject,
        selection: RpgMakerDocumentSelection,
    ) -> Result<RpgMakerProjectDocuments, Self::Error> {
        self.read_with_progress(project, selection, DocumentReadProgress::default())
            .await
    }

    async fn read_with_progress(
        &self,
        project: &OpenedProject,
        selection: RpgMakerDocumentSelection,
        progress: DocumentReadProgress,
    ) -> Result<RpgMakerProjectDocuments, Self::Error> {
        if selection.is_empty() {
            return Ok(RpgMakerProjectDocuments::empty());
        }

        let mut cache = self.cache.lock().await;
        let cached_documents = cache.select_documents(&selection, cache.all_maps_loaded);
        let cached_count =
            u64::try_from(cached_documents.len()).expect("命中文档数必须能表示为 u64");
        let missing = cache.missing_selection(&selection);
        if missing.is_empty() {
            progress.observe(0, cached_count);
            progress.observe(cached_count, cached_count);
            return Ok(RpgMakerProjectDocuments::from_shared_parts(
                cached_documents,
                Vec::new(),
            ));
        }

        let saw_progress = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fresh = self
            .reader
            .read_with_progress(
                project,
                missing.clone(),
                DocumentReadProgress::new({
                    let progress = progress.clone();
                    let saw_progress = Arc::clone(&saw_progress);
                    move |completed, total| {
                        let combined_total = cached_count.saturating_add(total);
                        if cached_count > 0
                            && !saw_progress.swap(true, std::sync::atomic::Ordering::Relaxed)
                        {
                            progress.observe(0, combined_total);
                        } else {
                            saw_progress.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        progress.observe(cached_count.saturating_add(completed), combined_total);
                    }
                }),
            )
            .await?;
        if cached_count > 0 && !saw_progress.load(std::sync::atomic::Ordering::Relaxed) {
            progress.observe(0, cached_count);
            progress.observe(cached_count, cached_count);
        }

        let loaded_all_maps = missing.includes_all_maps();
        let (documents, plugins) = fresh.into_shared_parts();
        cache.documents.extend(documents);
        cache.all_maps_loaded |= loaded_all_maps;
        Ok(RpgMakerProjectDocuments::from_shared_parts(
            cache.select_documents(&selection, true),
            plugins,
        ))
    }
}

#[derive(Default)]
struct CommandDocumentCache {
    documents: BTreeMap<RpgMakerDocumentId, Arc<StackSafeJsonValue>>,
    all_maps_loaded: bool,
}

impl CommandDocumentCache {
    fn missing_selection(
        &self,
        selection: &RpgMakerDocumentSelection,
    ) -> RpgMakerDocumentSelection {
        let mut missing = selection.clone();
        missing.standard_files.retain(|file| {
            !self
                .documents
                .contains_key(&RpgMakerDocumentId::Data(*file))
        });
        missing.data_files.retain(|file| {
            !self
                .documents
                .contains_key(&RpgMakerDocumentId::DataFile(file.clone()))
        });
        missing.map_ids.retain(|map_id| {
            !self
                .documents
                .contains_key(&RpgMakerDocumentId::Map(*map_id))
        });
        if self.all_maps_loaded {
            missing.all_maps = false;
        }
        // plugins.js 只由 Rules 消费，不为没有第二个消费者的值建立深克隆缓存。
        missing
    }

    fn select_documents(
        &self,
        selection: &RpgMakerDocumentSelection,
        include_all_cached_maps: bool,
    ) -> BTreeMap<RpgMakerDocumentId, Arc<StackSafeJsonValue>> {
        let mut selected = BTreeMap::new();
        for file in selection.standard_files() {
            self.insert_if_cached(&mut selected, RpgMakerDocumentId::Data(*file));
        }
        for file in selection.data_files() {
            self.insert_if_cached(&mut selected, RpgMakerDocumentId::DataFile(file.clone()));
        }
        if !selection.includes_all_maps() || include_all_cached_maps {
            for map_id in selection.map_ids() {
                self.insert_if_cached(&mut selected, RpgMakerDocumentId::Map(*map_id));
            }
        }
        if selection.includes_all_maps() && include_all_cached_maps && self.all_maps_loaded {
            selected.extend(self.documents.iter().filter_map(|(id, document)| {
                if matches!(id, RpgMakerDocumentId::Map(_)) {
                    Some((id.clone(), Arc::clone(document)))
                } else {
                    None
                }
            }));
        }
        selected
    }

    fn insert_if_cached(
        &self,
        selected: &mut BTreeMap<RpgMakerDocumentId, Arc<StackSafeJsonValue>>,
        id: RpgMakerDocumentId,
    ) {
        if let Some(document) = self.documents.get(&id) {
            selected.insert(id, Arc::clone(document));
        }
    }
}

/// 文档读取的同步、不可失败绝对进度入口。
#[derive(Clone)]
pub(crate) struct DocumentReadProgress {
    observe: Arc<dyn Fn(u64, u64) + Send + Sync>,
}

impl DocumentReadProgress {
    pub(crate) fn new<F>(observe: F) -> Self
    where
        F: Fn(u64, u64) + Send + Sync + 'static,
    {
        Self {
            observe: Arc::new(observe),
        }
    }

    fn observe(&self, completed: u64, total: u64) {
        (self.observe)(completed, total);
    }
}

impl Default for DocumentReadProgress {
    fn default() -> Self {
        Self::new(|_, _| {})
    }
}

/// RPG Maker 文档读取阶段的内部执行策略。
///
/// 在途读取宽度由产品基准选择，不是用户配置或项目总量上限；CPU 解析并行度统一由 CPU
/// 根执行器管理。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerDocumentReadingConfig {
    max_active_reads: NonZeroUsize,
}

impl RpgMakerDocumentReadingConfig {
    pub(crate) const fn new(max_active_reads: NonZeroUsize) -> Self {
        Self { max_active_reads }
    }

    pub(crate) const fn max_active_reads(self) -> NonZeroUsize {
        self.max_active_reads
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
        self.read_with_progress(project, selection, DocumentReadProgress::default())
            .await
    }

    async fn read_with_progress(
        &self,
        project: &OpenedProject,
        selection: RpgMakerDocumentSelection,
        progress: DocumentReadProgress,
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
            .map_err(|error| match error {
                BuildDocumentRequestsError::ListData(source) => {
                    RpgMakerProjectDocumentReadingError::ListData(source)
                }
                BuildDocumentRequestsError::ListJs(source) => {
                    RpgMakerProjectDocumentReadingError::ListJs(source)
                }
                BuildDocumentRequestsError::FileNameCaseMismatch(source) => {
                    RpgMakerProjectDocumentReadingError::FileNameCaseMismatch(source)
                }
            })?;
        let request_count = requests.len();
        let total = u64::try_from(request_count).expect("RPG Maker 文档请求数必须能用 u64 表达");
        if total > 0 {
            progress.observe(0, total);
        }
        let max_active_reads = self.config.max_active_reads().get();
        // 第二层只覆盖本次有限请求集合，使读取完成后立即进入 CPU 根的准入等待；
        // 实际执行与已接管队列仍由 CPU 根的统一预算约束。
        let parse_submission_capacity = request_count.max(1);
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
        .buffer_unordered(max_active_reads);

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

        futures_util::pin_mut!(work);
        let mut outcomes = Vec::with_capacity(request_count);
        let mut completed = 0_u64;
        while let Some(outcome) = work.next().await {
            if outcome.1.is_ok() {
                completed = completed.saturating_add(1);
                progress.observe(completed, total);
            }
            outcomes.push(outcome);
        }
        outcomes.sort_by_key(|(request_index, _)| *request_index);
        let mut documents = BTreeMap::new();
        let mut plugins = Vec::new();
        for (_, result) in outcomes {
            match result? {
                ParsedDocument::Json { id, value } => {
                    documents.insert(id, value);
                }
                ParsedDocument::Plugins(records) => plugins = records,
            }
        }

        Ok(RpgMakerProjectDocuments::from_stack_safe_parts(
            documents, plugins,
        ))
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
    ) -> Result<Vec<DocumentRequest>, BuildDocumentRequestsError<L::Error>> {
        let data_entries = if !selection.standard_files().is_empty()
            || !selection.data_files().is_empty()
            || !selection.map_ids().is_empty()
            || selection.includes_all_maps()
        {
            self.directory_lister
                .list_directory(data_root.to_path_buf())
                .await
                .map_err(BuildDocumentRequestsError::ListData)?
        } else {
            Vec::new()
        };
        let mut documents = BTreeMap::new();
        for file in selection.standard_files() {
            let path = exact_or_requested_path(data_root, file.file_name(), &data_entries)
                .map_err(BuildDocumentRequestsError::FileNameCaseMismatch)?;
            documents.insert(RpgMakerDocumentId::Data(*file), path);
        }
        for file in selection.data_files() {
            let path = exact_or_requested_path(data_root, file.as_str(), &data_entries)
                .map_err(BuildDocumentRequestsError::FileNameCaseMismatch)?;
            documents.insert(RpgMakerDocumentId::DataFile(file.clone()), path);
        }
        for map_id in selection.map_ids() {
            let file_name = map_id.file_name();
            let path = exact_or_requested_path(data_root, &file_name, &data_entries)
                .map_err(BuildDocumentRequestsError::FileNameCaseMismatch)?;
            documents.insert(RpgMakerDocumentId::Map(*map_id), path);
        }

        if selection.includes_all_maps() {
            for entry in &data_entries {
                if entry.kind() != DirectoryEntryKind::RegularFile {
                    continue;
                }
                let path = entry.resolved_path();
                if let Some(map_id) = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(MapId::from_canonical_file_name)
                {
                    documents.insert(RpgMakerDocumentId::Map(map_id), path.to_path_buf());
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
            let entries = self
                .directory_lister
                .list_directory(js_root.to_path_buf())
                .await
                .map_err(BuildDocumentRequestsError::ListJs)?;
            let path = exact_or_requested_path(js_root, "plugins.js", &entries)
                .map_err(BuildDocumentRequestsError::FileNameCaseMismatch)?;
            requests.push(DocumentRequest {
                kind: DocumentRequestKind::Plugins,
                path,
            });
        }
        Ok(requests)
    }
}

fn exact_or_requested_path(
    parent: &Path,
    file_name: &str,
    entries: &[crate::storage::file_system::DirectoryEntry],
) -> Result<PathBuf, ExactPathCaseMismatch> {
    resolve_exact_directory_entry(
        parent,
        file_name,
        entries.iter().map(|entry| entry.resolved_path()),
    )
    .map(|resolved| resolved.unwrap_or_else(|| parent.join(file_name)))
}

#[derive(Debug)]
enum BuildDocumentRequestsError<L> {
    ListData(ListDirectoryError<L>),
    ListJs(ListDirectoryError<L>),
    FileNameCaseMismatch(ExactPathCaseMismatch),
}

/// 无损读取 RPG Maker 文档时的阶段错误。
#[derive(Debug)]
pub(crate) enum RpgMakerProjectDocumentReadingError<F, L, C> {
    ListData(ListDirectoryError<L>),
    ListJs(ListDirectoryError<L>),
    FileNameCaseMismatch(ExactPathCaseMismatch),
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
        source: StackSafeJsonError,
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

impl<F, L, C> RpgMakerProjectDocumentReadingError<F, L, C>
where
    F: SafeDiagnosticSource,
    L: SafeDiagnosticSource,
    CpuTaskExecutionError<C>: SafeDiagnosticSource,
{
    /// 在仍持有文档路径、解析位置和具体根错误时建立公开投影。
    ///
    /// 共享文档读取器同时服务 Extract(Builtin/Rules)与 WriteBack;责任 code 与
    /// 阶段属于调用域,由调用方传入,本类型不得替调用方猜测阶段。
    pub(crate) fn safe_diagnostic_at(
        &self,
        code: DiagnosticCode,
        stage: DiagnosticStage,
    ) -> SafeDiagnostic {
        match self {
            Self::ListData(source) | Self::ListJs(source) => {
                list_directory_diagnostic(source, code, stage)
            }
            Self::FileNameCaseMismatch(source) => SafeDiagnostic::new(
                code,
                stage,
                DiagnosticSubject::path(source.requested()),
                DiagnosticReason::failure(DiagnosticFailureKind::StateMismatch),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixInput,
            )
            .with_recovery(RecoveryFact::path(source.actual())),
            Self::ReadDocument { source, .. } => read_document_diagnostic(source, code, stage),
            Self::ScheduleParse { path, source } => source
                .safe_diagnostic_source(stage, DiagnosticImpact::Unchanged, DiagnosticAction::Retry)
                .with_recovery(RecoveryFact::path(path)),
            Self::InvalidUtf8 { path, source } => SafeDiagnostic::new(
                code,
                stage,
                DiagnosticSubject::path(path),
                DiagnosticReason::InvalidUtf8 {
                    valid_up_to: u64::try_from(source.valid_up_to()).unwrap_or(u64::MAX),
                    error_len: source
                        .error_len()
                        .map(|value| u64::try_from(value).unwrap_or(u64::MAX)),
                },
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixInput,
            ),
            Self::InvalidJson { path, source } => SafeDiagnostic::new(
                code,
                stage,
                DiagnosticSubject::path(path),
                DiagnosticReason::failure(DiagnosticFailureKind::SourceDocumentInvalid),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixInput,
            )
            .with_recovery(RecoveryFact::component(format!(
                "json_line={}; json_column={}",
                source.line(),
                source.column()
            ))),
            Self::InvalidPluginsEnvelope { path } => {
                source_document_invalid(path, None, code, stage)
            }
            Self::InvalidPluginRecord { path, index } => {
                source_document_invalid(path, Some(*index), code, stage)
            }
        }
    }
}

impl<F, L, C> SafeDiagnosticSource for RpgMakerProjectDocumentReadingError<F, L, C>
where
    F: SafeDiagnosticSource,
    L: SafeDiagnosticSource,
    CpuTaskExecutionError<C>: SafeDiagnosticSource,
{
    /// 共享文档读取器只有 Extract 与 WriteBack 两个真实调用域;调用方传入的
    /// stage 决定责任 code,本类型不再替调用方把一切失败归到 WriteBack。
    fn safe_diagnostic_source(
        &self,
        stage: DiagnosticStage,
        _impact: DiagnosticImpact,
        _fallback_action: DiagnosticAction,
    ) -> SafeDiagnostic {
        let code = match stage {
            DiagnosticStage::WriteBack => DiagnosticCode::WriteBackDocumentRead,
            _ => DiagnosticCode::ExtractDocumentRead,
        };
        self.safe_diagnostic_at(code, stage)
    }
}

fn list_directory_diagnostic<E>(
    source: &ListDirectoryError<E>,
    code: DiagnosticCode,
    stage: DiagnosticStage,
) -> SafeDiagnostic
where
    E: SafeDiagnosticSource,
{
    match source {
        ListDirectoryError::NotFound { path } => SafeDiagnostic::new(
            code,
            stage,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixInput,
        ),
        ListDirectoryError::NotDirectory { path } => SafeDiagnostic::new(
            code,
            stage,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::InvalidPath),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixInput,
        ),
        ListDirectoryError::Io { path, source } => source
            .safe_diagnostic_source(
                stage,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckPathAndPermissions,
            )
            .with_recovery(RecoveryFact::path(path)),
    }
}

fn read_document_diagnostic<E>(
    source: &ReadFileError<E>,
    code: DiagnosticCode,
    stage: DiagnosticStage,
) -> SafeDiagnostic
where
    E: SafeDiagnosticSource,
{
    match source {
        ReadFileError::NotFound { path } => SafeDiagnostic::new(
            code,
            stage,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixInput,
        ),
        ReadFileError::NotFile { path } => SafeDiagnostic::new(
            code,
            stage,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::InvalidPath),
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixInput,
        ),
        ReadFileError::Io { path, source } => source
            .safe_diagnostic_source(
                stage,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckPathAndPermissions,
            )
            .with_recovery(RecoveryFact::path(path)),
    }
}

fn source_document_invalid(
    path: &Path,
    plugin_index: Option<usize>,
    code: DiagnosticCode,
    stage: DiagnosticStage,
) -> SafeDiagnostic {
    let diagnostic = SafeDiagnostic::new(
        code,
        stage,
        DiagnosticSubject::path(path),
        DiagnosticReason::failure(DiagnosticFailureKind::SourceDocumentInvalid),
        DiagnosticImpact::Unchanged,
        DiagnosticAction::FixInput,
    );
    plugin_index.map_or(diagnostic.clone(), |index| {
        diagnostic.with_recovery(RecoveryFact::component(format!("plugin_index={index}")))
    })
}

impl<F, L, C> fmt::Display for RpgMakerProjectDocumentReadingError<F, L, C>
where
    F: fmt::Display,
    L: fmt::Display,
    C: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ListData(error) => write!(formatter, "无法列举 RPG Maker data 目录：{error}"),
            Self::ListJs(error) => write!(formatter, "无法列举 RPG Maker js 目录：{error}"),
            Self::FileNameCaseMismatch(error) => error.fmt(formatter),
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
            Self::ListData(error) | Self::ListJs(error) => Some(error),
            Self::FileNameCaseMismatch(error) => Some(error),
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
        value: StackSafeJsonValue,
    },
    Plugins(Vec<PluginConfiguration>),
}

#[derive(Debug)]
enum ParseFailure {
    InvalidUtf8 {
        path: PathBuf,
        source: Utf8Error,
    },
    InvalidJson {
        path: PathBuf,
        source: StackSafeJsonError,
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
            let value =
                parse_json(text).map_err(|source| ParseFailure::InvalidJson { path, source })?;
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

    let values = parse_json(json.trim()).map_err(|source| ParseFailure::InvalidJson {
        path: path.clone(),
        source,
    })?;
    if !values.is_array() {
        return Err(ParseFailure::InvalidPluginsEnvelope { path });
    }
    let Value::Array(values) = values.into_inner() else {
        unreachable!("已检查 plugins.js 赋值是 array")
    };
    let values = values
        .into_iter()
        .map(StackSafeJsonValue::new)
        .collect::<Vec<_>>();
    let mut plugins = Vec::with_capacity(values.len());
    for (index, value) in values.into_iter().enumerate() {
        if !value.is_object() {
            return Err(ParseFailure::InvalidPluginRecord { path, index });
        }
        let Value::Object(fields) = value.into_inner() else {
            unreachable!("已检查插件记录是对象")
        };
        plugins.push(PluginConfiguration::new(index, fields));
    }
    Ok(ParsedDocument::Plugins(plugins))
}

#[cfg(test)]
pub(crate) fn parse_json_document_for_test(
    id: RpgMakerDocumentId,
    text: &str,
) -> RpgMakerProjectDocuments {
    let ParsedDocument::Json { id, value } = parse_document(
        DocumentRequestKind::Json(id),
        PathBuf::from("data/Test.json"),
        text.as_bytes().to_vec(),
    )
    .expect("测试 Standard JSON 必须有效") else {
        unreachable!("JSON 测试请求必须返回 JSON 文档")
    };
    RpgMakerProjectDocuments::from_stack_safe_parts(BTreeMap::from([(id, value)]), Vec::new())
}

#[cfg(test)]
pub(crate) fn parse_plugins_document_for_test(text: &str) -> RpgMakerProjectDocuments {
    let ParsedDocument::Plugins(plugins) = parse_document(
        DocumentRequestKind::Plugins,
        PathBuf::from("js/plugins.js"),
        text.as_bytes().to_vec(),
    )
    .expect("测试 plugins.js 必须有效") else {
        unreachable!("plugins 测试请求必须返回插件记录")
    };
    RpgMakerProjectDocuments::from_stack_safe_parts(BTreeMap::new(), plugins)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
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
        assert_eq!(
            MapId::from_canonical_file_name("Map001.json"),
            Some(map_id(1))
        );
        assert_eq!(
            MapId::from_canonical_file_name("Map999.json"),
            Some(map_id(999))
        );
        assert_eq!(
            MapId::from_canonical_file_name("Map1000.json"),
            Some(map_id(1000))
        );
        assert_eq!(MapId::from_canonical_file_name("Map000.json"), None);
        assert_eq!(MapId::from_canonical_file_name("Map01.json"), None);
        assert_eq!(MapId::from_canonical_file_name("Map0001.json"), None);
        assert_eq!(MapId::from_canonical_file_name("MapInfos.json"), None);
        assert_eq!(MapId::from_canonical_file_name("QuestData.json"), None);
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
        let progress = Arc::new(Mutex::new(Vec::new()));

        let documents = harness
            .service()
            .read_with_progress(
                &project(),
                selection,
                DocumentReadProgress::new({
                    let progress = Arc::clone(&progress);
                    move |completed, total| {
                        progress
                            .lock()
                            .expect("进度记录锁不应中毒")
                            .push((completed, total));
                    }
                }),
            )
            .await
            .expect("规范文档应该成功读取");

        assert_eq!(harness.list_calls.load(Ordering::SeqCst), 2);
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
                .document(RpgMakerDocumentId::Map(map_id(1)))
                .expect("Map001 应该存在")["displayName"],
            "村庄"
        );
        assert_eq!(
            documents
                .document(RpgMakerDocumentId::Map(map_id(1000)))
                .expect("Map1000 应该存在")["displayName"],
            "隐藏地图"
        );
        assert_eq!(documents.plugins().len(), 1);
        assert_eq!(documents.plugins()[0].index(), 0);
        assert_eq!(documents.plugins()[0].fields()["status"], false);
        assert_eq!(documents.plugins()[0].fields()["future"]["kept"], true);
        assert_eq!(
            *progress.lock().expect("进度记录锁不应中毒"),
            [(0, 4), (1, 4), (2, 4), (3, 4), (4, 4)]
        );
    }

    #[tokio::test]
    async fn builtin_and_rules_share_overlapping_parsed_documents_within_one_command() {
        let root = project().source_root().to_path_buf();
        let actors = root.join("data").join("Actors.json");
        let items = root.join("data").join("Items.json");
        let map = root.join("data").join("Map001.json");
        let harness = Harness::new(
            HashMap::from([
                (
                    actors,
                    r#"[null,{"name":"勇者","profile":"共同内容"}]"#.as_bytes().to_vec(),
                ),
                (
                    items,
                    r#"[null,{"name":"药水","description":"恢复"}]"#.as_bytes().to_vec(),
                ),
                (
                    map,
                    r#"{"displayName":"共同地图","events":[]}"#.as_bytes().to_vec(),
                ),
            ]),
            Vec::new(),
            2,
        );
        let reader = CommandScopedRpgMakerDocumentReader::new(harness.service());

        let builtin_documents = reader
            .read(
                &project(),
                RpgMakerDocumentSelection::new([StandardDataFile::Actors], true, false),
            )
            .await
            .expect("Builtin 文档应读取成功");
        let progress = Arc::new(Mutex::new(Vec::new()));
        let rules_documents = reader
            .read_with_progress(
                &project(),
                RpgMakerDocumentSelection::new(
                    [StandardDataFile::Actors, StandardDataFile::Items],
                    true,
                    false,
                ),
                DocumentReadProgress::new({
                    let progress = Arc::clone(&progress);
                    move |completed, total| {
                        progress
                            .lock()
                            .expect("进度记录锁不应中毒")
                            .push((completed, total));
                    }
                }),
            )
            .await
            .expect("Rules 应只补读缺失文档");

        let builtin_actors = builtin_documents
            .documents()
            .get(&RpgMakerDocumentId::Data(StandardDataFile::Actors))
            .expect("Builtin 应取得 Actors");
        let rules_actors = rules_documents
            .documents()
            .get(&RpgMakerDocumentId::Data(StandardDataFile::Actors))
            .expect("Rules 应取得同一 Actors");
        assert!(Arc::ptr_eq(builtin_actors, rules_actors));
        assert_eq!(builtin_actors[1]["profile"], "共同内容");
        assert_eq!(rules_actors[1]["profile"], "共同内容");
        let builtin_map = builtin_documents
            .documents()
            .get(&RpgMakerDocumentId::Map(map_id(1)))
            .expect("Builtin 应取得 Map001");
        let rules_map = rules_documents
            .documents()
            .get(&RpgMakerDocumentId::Map(map_id(1)))
            .expect("Rules 应取得同一 Map001");
        assert!(Arc::ptr_eq(builtin_map, rules_map));
        assert!(
            rules_documents
                .document(RpgMakerDocumentId::Data(StandardDataFile::Items))
                .is_some()
        );
        assert_eq!(harness.file_calls.load(Ordering::SeqCst), 3);
        assert_eq!(harness.cpu_calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            *progress.lock().expect("进度记录锁不应中毒"),
            [(0, 3), (2, 3), (3, 3)]
        );
    }

    #[tokio::test]
    async fn exact_map_selection_lists_data_to_preserve_exact_case_identity() {
        let root = project().source_root().to_path_buf();
        let map = root.join("data").join("Map042.json");
        let harness = Harness::new(
            HashMap::from([(map, r#"{"displayName":"精确地图"}"#.as_bytes().to_vec())]),
            Vec::new(),
            1,
        );
        let mut selection = RpgMakerDocumentSelection::empty();
        selection.insert_map(map_id(42));

        let documents = harness
            .service()
            .read(&project(), selection)
            .await
            .expect("精确 Map 选择应该成功");

        assert_eq!(harness.list_calls.load(Ordering::SeqCst), 1);
        assert_eq!(harness.file_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            documents
                .document(RpgMakerDocumentId::Map(map_id(42)))
                .expect("Map042 应被读取")["displayName"],
            "精确地图"
        );
    }

    #[tokio::test]
    async fn exact_nonstandard_data_file_is_resolved_from_the_real_directory_entry() {
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

        assert_eq!(harness.list_calls.load(Ordering::SeqCst), 1);
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
        let progress = Arc::new(Mutex::new(Vec::new()));

        let error = invalid_utf8
            .service()
            .read_with_progress(
                &project(),
                RpgMakerDocumentSelection::new([StandardDataFile::Actors], false, false),
                DocumentReadProgress::new({
                    let progress = Arc::clone(&progress);
                    move |completed, total| {
                        progress
                            .lock()
                            .expect("进度记录锁不应中毒")
                            .push((completed, total));
                    }
                }),
            )
            .await
            .expect_err("非法 UTF-8 应该失败");
        assert!(matches!(
            error,
            RpgMakerProjectDocumentReadingError::InvalidUtf8 { path, .. } if path == actors
        ));
        assert_eq!(*progress.lock().expect("进度记录锁不应中毒"), [(0, 1)]);

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

    // 测试需要一个真实的 Utf8Error 值；字面量非法正是本函数的目的。
    #[allow(invalid_from_utf8)]
    fn invalid_utf8_error() -> Utf8Error {
        let bytes = [0xffu8];
        std::str::from_utf8(&bytes).expect_err("单字节 0xff 不是合法 UTF-8")
    }

    #[test]
    fn document_read_diagnostics_carry_the_calling_stage_not_a_hardcoded_write_back() {
        // 共享读取器同时服务 Extract 与 WriteBack;失败的责任 code 与阶段
        // 必须来自真实调用域,不得把 Extract 失败标成 write_back。
        let error: RpgMakerProjectDocumentReadingError<
            crate::runtime::filesystem::SystemFileSystemError,
            crate::runtime::filesystem::SystemFileSystemError,
            crate::runtime::cpu::CpuExecutorUnavailable,
        > = RpgMakerProjectDocumentReadingError::InvalidUtf8 {
            path: PathBuf::from(r"C:\games\demo\data\Actors.json"),
            source: invalid_utf8_error(),
        };

        let extract = error.safe_diagnostic_source(
            DiagnosticStage::Extract,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::CheckProjectState,
        );
        assert_eq!(extract.code, DiagnosticCode::ExtractDocumentRead);
        assert_eq!(extract.stage, DiagnosticStage::Extract);

        let write_back = error.safe_diagnostic_source(
            DiagnosticStage::WriteBack,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixInput,
        );
        assert_eq!(write_back.code, DiagnosticCode::WriteBackDocumentRead);
        assert_eq!(write_back.stage, DiagnosticStage::WriteBack);
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
    async fn case_alias_is_rejected_before_a_platform_can_open_it() {
        let root = project().source_root().to_path_buf();
        let actual = root.join("data").join("actors.json");
        let harness = Harness::new(
            HashMap::from([(actual.clone(), b"[]".to_vec())]),
            Vec::new(),
            1,
        );

        let error = harness
            .service()
            .read(
                &project(),
                RpgMakerDocumentSelection::new([StandardDataFile::Actors], false, false),
            )
            .await
            .expect_err("真实文件名大小写不匹配必须显式失败");

        assert!(matches!(
            error,
            RpgMakerProjectDocumentReadingError::FileNameCaseMismatch(ref mismatch)
                if mismatch.requested() == root.join("data").join("Actors.json")
                    && mismatch.actual() == actual
        ));
        assert_eq!(harness.file_calls.load(Ordering::SeqCst), 0);
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
                entries: Arc::new(vec![actors.clone()]),
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
            path: PathBuf,
        ) -> Result<Vec<crate::storage::file_system::DirectoryEntry>, ListDirectoryError<Self::Error>>
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .entries
                .iter()
                .filter(|entry| entry.parent() == Some(path.as_path()))
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
        max_active_reads: NonZeroUsize,
    }

    impl Harness {
        fn new(
            files: HashMap<PathBuf, Vec<u8>>,
            mut entries: Vec<PathBuf>,
            max_active_reads: usize,
        ) -> Self {
            entries.extend(files.keys().cloned());
            entries.sort();
            entries.dedup();
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
                max_active_reads: NonZeroUsize::new(max_active_reads)
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
                RpgMakerDocumentReadingConfig::new(self.max_active_reads),
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

    fn map_id(value: u32) -> MapId {
        MapId::new(value).expect("测试 Map ID 必须为正整数")
    }
}
