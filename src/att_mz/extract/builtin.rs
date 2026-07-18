//! RPG Maker MZ 固定位置文本的完整快照提取。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;

use futures_util::StreamExt;
use futures_util::stream;

use serde_json::{Map, Value};

use crate::att_mz::project::OpenedProject;
use crate::storage::cpu::{CpuTaskExecutionError, CpuTaskExecutor};

use super::document::{
    MzDocumentId, MzDocumentSelection, MzProjectDocumentReader, MzProjectDocuments,
    StandardDataFile,
};
use super::model::{
    BuiltinSnapshot, ExtractedTextField, ExtractedTextGroup, MzLocation, MzLocationStep, MzSource,
    SnapshotModelError, TextGroupKind,
};
use super::store::BuiltinSnapshotStore;

/// 刷新 MZ 内置规格能够确定的标准文本资产。
pub(crate) trait BuiltInExtraction: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn refresh(
        &self,
        project: &OpenedProject,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 从无损 MZ 文档建立 Builtin 快照，再由 Store 原子替换旧快照。
pub(crate) struct BuiltInExtractionService<R, S, C> {
    document_reader: R,
    snapshot_store: S,
    cpu_executor: C,
    config: BuiltInExtractionConfig,
}

impl<R, S, C> BuiltInExtractionService<R, S, C> {
    pub(crate) fn new(
        document_reader: R,
        snapshot_store: S,
        cpu_executor: C,
        config: BuiltInExtractionConfig,
    ) -> Self {
        Self {
            document_reader,
            snapshot_store,
            cpu_executor,
            config,
        }
    }
}

/// Builtin 阶段由外部明确提供的 CPU 并行上限。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BuiltInExtractionConfig {
    scan_concurrency: NonZeroUsize,
}

impl BuiltInExtractionConfig {
    pub(crate) const fn new(scan_concurrency: NonZeroUsize) -> Self {
        Self { scan_concurrency }
    }

    pub(crate) const fn scan_concurrency(self) -> NonZeroUsize {
        self.scan_concurrency
    }
}

impl<R, S, C> BuiltInExtraction for BuiltInExtractionService<R, S, C>
where
    R: MzProjectDocumentReader,
    S: BuiltinSnapshotStore,
    C: CpuTaskExecutor,
{
    type Error = BuiltInExtractionError<R::Error, S::Error, C::Error>;

    async fn refresh(&self, project: &OpenedProject) -> Result<(), Self::Error> {
        let documents = self
            .document_reader
            .read(project, builtin_document_selection())
            .await
            .map_err(BuiltInExtractionError::ReadDocuments)?;

        let snapshot =
            match build_builtin_snapshot_parallel(&self.cpu_executor, self.config, documents).await
            {
                Ok(snapshot) => snapshot,
                Err(ParallelBuiltinBuildError::Compute(source)) => {
                    return Err(BuiltInExtractionError::ScheduleCompute(source));
                }
                Err(ParallelBuiltinBuildError::Build(BuildBuiltinSnapshotError::Malformed(
                    source,
                ))) => {
                    return Err(BuiltInExtractionError::MalformedDocument(source));
                }
                Err(ParallelBuiltinBuildError::Build(BuildBuiltinSnapshotError::Model(source))) => {
                    return Err(BuiltInExtractionError::BuildSnapshot(*source));
                }
            };

        self.snapshot_store
            .replace_builtin(project, snapshot)
            .await
            .map_err(BuiltInExtractionError::Persist)?;
        Ok(())
    }
}

fn builtin_document_selection() -> MzDocumentSelection {
    MzDocumentSelection::new(
        [
            StandardDataFile::Actors,
            StandardDataFile::Armors,
            StandardDataFile::Classes,
            StandardDataFile::CommonEvents,
            StandardDataFile::Enemies,
            StandardDataFile::Items,
            StandardDataFile::Skills,
            StandardDataFile::States,
            StandardDataFile::System,
            StandardDataFile::Troops,
            StandardDataFile::Weapons,
        ],
        true,
        false,
    )
}

/// Builtin 服务在直接依赖和自身文档解释边界上的阶段错误。
#[derive(Debug)]
pub(crate) enum BuiltInExtractionError<RE, SE, CE> {
    ReadDocuments(RE),
    ScheduleCompute(CpuTaskExecutionError<CE>),
    MalformedDocument(BuiltinDocumentError),
    BuildSnapshot(SnapshotModelError),
    Persist(SE),
}

impl<RE, SE, CE> fmt::Display for BuiltInExtractionError<RE, SE, CE>
where
    RE: Error,
    SE: Error,
    CE: Error,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadDocuments(source) => write!(formatter, "读取 MZ 文档失败：{source}"),
            Self::ScheduleCompute(source) => {
                write!(formatter, "调度 Builtin CPU 计算失败：{source}")
            }
            Self::MalformedDocument(source) => write!(formatter, "MZ 文档结构错误：{source}"),
            Self::BuildSnapshot(source) => write!(formatter, "构造 Builtin 快照失败：{source}"),
            Self::Persist(source) => write!(formatter, "保存 Builtin 快照失败：{source}"),
        }
    }
}

impl<RE, SE, CE> Error for BuiltInExtractionError<RE, SE, CE>
where
    RE: Error + 'static,
    SE: Error + 'static,
    CE: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadDocuments(source) => Some(source),
            Self::ScheduleCompute(source) => Some(source),
            Self::MalformedDocument(source) => Some(source),
            Self::BuildSnapshot(source) => Some(source),
            Self::Persist(source) => Some(source),
        }
    }
}

/// 一个所选标准 MZ 文档不符合固定结构。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BuiltinDocumentError {
    location: String,
    message: String,
}

impl BuiltinDocumentError {
    fn new(location: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            location: location.into(),
            message: message.into(),
        }
    }

    #[cfg(test)]
    pub(crate) fn location(&self) -> &str {
        &self.location
    }
}

impl fmt::Display for BuiltinDocumentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}：{}", self.location, self.message)
    }
}

impl Error for BuiltinDocumentError {}

#[derive(Debug)]
enum BuildBuiltinSnapshotError {
    Malformed(BuiltinDocumentError),
    Model(Box<SnapshotModelError>),
}

impl From<BuiltinDocumentError> for BuildBuiltinSnapshotError {
    fn from(value: BuiltinDocumentError) -> Self {
        Self::Malformed(value)
    }
}

impl From<SnapshotModelError> for BuildBuiltinSnapshotError {
    fn from(value: SnapshotModelError) -> Self {
        Self::Model(Box::new(value))
    }
}

#[cfg(test)]
fn build_builtin_snapshot(
    documents: &MzProjectDocuments,
) -> Result<BuiltinSnapshot, BuildBuiltinSnapshotError> {
    let mut groups = Vec::new();

    for (file, fields) in database_specs() {
        extract_database_entries(documents, file, fields, &mut groups)?;
    }
    extract_system(documents, &mut groups)?;
    extract_maps(documents, &mut groups)?;
    extract_common_events(documents, &mut groups)?;
    extract_troops(documents, &mut groups)?;

    BuiltinSnapshot::new(groups).map_err(Into::into)
}

#[derive(Debug)]
enum ParallelBuiltinBuildError<CE> {
    Compute(CpuTaskExecutionError<CE>),
    Build(BuildBuiltinSnapshotError),
}

async fn build_builtin_snapshot_parallel<C>(
    cpu_executor: &C,
    config: BuiltInExtractionConfig,
    documents: MzProjectDocuments,
) -> Result<BuiltinSnapshot, ParallelBuiltinBuildError<C::Error>>
where
    C: CpuTaskExecutor,
{
    let work_units = builtin_work_units(documents).map_err(|error| {
        ParallelBuiltinBuildError::Build(BuildBuiltinSnapshotError::Malformed(error))
    })?;
    let results = stream::iter(
        work_units
            .into_iter()
            .map(|work_unit| cpu_executor.execute(move || work_unit.run())),
    )
    .buffered(config.scan_concurrency().get())
    .collect::<Vec<_>>()
    .await;

    let mut groups = Vec::new();
    for result in results {
        let local_groups = result.map_err(ParallelBuiltinBuildError::Compute)?;
        groups.extend(local_groups.map_err(ParallelBuiltinBuildError::Build)?);
    }

    cpu_executor
        .execute(move || BuiltinSnapshot::new(groups).map_err(Into::into))
        .await
        .map_err(ParallelBuiltinBuildError::Compute)?
        .map_err(ParallelBuiltinBuildError::Build)
}

enum BuiltinWorkUnit {
    Database {
        file: StandardDataFile,
        field_names: &'static [&'static str],
        document: Value,
    },
    System(Value),
    Map {
        map_id: u32,
        document: Value,
    },
    CommonEvents(Value),
    Troops(Value),
}

impl BuiltinWorkUnit {
    fn run(self) -> Result<Vec<ExtractedTextGroup>, BuildBuiltinSnapshotError> {
        let mut groups = Vec::new();
        match self {
            Self::Database {
                file,
                field_names,
                document,
            } => {
                let documents = single_document(MzDocumentId::Data(file), document);
                extract_database_entries(&documents, file, field_names, &mut groups)?;
            }
            Self::System(document) => {
                let documents =
                    single_document(MzDocumentId::Data(StandardDataFile::System), document);
                extract_system(&documents, &mut groups)?;
            }
            Self::Map { map_id, document } => {
                let documents = single_document(MzDocumentId::Map(map_id), document);
                extract_maps(&documents, &mut groups)?;
            }
            Self::CommonEvents(document) => {
                let documents =
                    single_document(MzDocumentId::Data(StandardDataFile::CommonEvents), document);
                extract_common_events(&documents, &mut groups)?;
            }
            Self::Troops(document) => {
                let documents =
                    single_document(MzDocumentId::Data(StandardDataFile::Troops), document);
                extract_troops(&documents, &mut groups)?;
            }
        }
        Ok(groups)
    }
}

fn single_document(id: MzDocumentId, document: Value) -> MzProjectDocuments {
    MzProjectDocuments::new([(id, document)].into_iter().collect(), Vec::new())
}

fn builtin_work_units(
    documents: MzProjectDocuments,
) -> Result<Vec<BuiltinWorkUnit>, BuiltinDocumentError> {
    let (mut documents, _plugins) = documents.into_parts();
    let mut work_units = Vec::new();

    for (file, field_names) in database_specs() {
        let document = take_data_document(&mut documents, file)?;
        work_units.push(BuiltinWorkUnit::Database {
            file,
            field_names,
            document,
        });
    }

    work_units.push(BuiltinWorkUnit::System(take_data_document(
        &mut documents,
        StandardDataFile::System,
    )?));

    let map_ids = documents
        .keys()
        .filter_map(|id| match id {
            MzDocumentId::Map(map_id) => Some(*map_id),
            MzDocumentId::Data(_) => None,
        })
        .collect::<Vec<_>>();
    for map_id in map_ids {
        let document = documents
            .remove(&MzDocumentId::Map(map_id))
            .expect("刚从文档键集合取得的地图必须仍然存在");
        work_units.push(BuiltinWorkUnit::Map { map_id, document });
    }

    work_units.push(BuiltinWorkUnit::CommonEvents(take_data_document(
        &mut documents,
        StandardDataFile::CommonEvents,
    )?));
    work_units.push(BuiltinWorkUnit::Troops(take_data_document(
        &mut documents,
        StandardDataFile::Troops,
    )?));

    Ok(work_units)
}

fn take_data_document(
    documents: &mut std::collections::BTreeMap<MzDocumentId, Value>,
    file: StandardDataFile,
) -> Result<Value, BuiltinDocumentError> {
    documents
        .remove(&MzDocumentId::Data(file))
        .ok_or_else(|| BuiltinDocumentError::new(format!("data/{}", file.file_name()), "文档缺失"))
}

fn database_specs() -> [(StandardDataFile, &'static [&'static str]); 8] {
    // `message1`～`message4` 是 MZ 数据文件的原始字段名，保留它们可直接定位和
    // 调试源数据。语义上，Skills 的 message1/2 是技能使用消息；States 的四项
    // 依次对应角色附加、敌人附加、状态持续和状态解除消息。
    [
        (StandardDataFile::Actors, &["name", "nickname", "profile"]),
        (StandardDataFile::Classes, &["name"]),
        (
            StandardDataFile::Skills,
            &["name", "description", "message1", "message2"],
        ),
        (StandardDataFile::Items, &["name", "description"]),
        (StandardDataFile::Weapons, &["name", "description"]),
        (StandardDataFile::Armors, &["name", "description"]),
        (StandardDataFile::Enemies, &["name"]),
        (
            StandardDataFile::States,
            &["name", "message1", "message2", "message3", "message4"],
        ),
    ]
}

fn extract_database_entries(
    documents: &MzProjectDocuments,
    file: StandardDataFile,
    field_names: &[&str],
    groups: &mut Vec<ExtractedTextGroup>,
) -> Result<(), BuildBuiltinSnapshotError> {
    let source = MzSource::data(file);
    let root = required_data_document(documents, file)?;
    let entries = expect_array(root, source.to_string())?;

    for (entry_index, entry) in entries.iter().enumerate() {
        if entry.is_null() {
            continue;
        }
        let entry_steps = vec![MzLocationStep::index(entry_index)];
        let entry_location = MzLocation::value(source.clone(), entry_steps.clone());
        let object = expect_object(entry, entry_location.to_string())?;
        let mut fields = Vec::new();
        for field_name in field_names {
            let mut field_steps = entry_steps.clone();
            field_steps.push(MzLocationStep::key(*field_name));
            let exact_location = MzLocation::value(source.clone(), field_steps);
            let text = expect_string_field(object, field_name, &exact_location)?;
            push_text_field(&mut fields, *field_name, exact_location, text)?;
        }
        push_group(groups, TextGroupKind::DatabaseEntry, entry_location, fields)?;
    }
    Ok(())
}

fn extract_system(
    documents: &MzProjectDocuments,
    groups: &mut Vec<ExtractedTextGroup>,
) -> Result<(), BuildBuiltinSnapshotError> {
    let source = MzSource::data(StandardDataFile::System);
    let root = required_data_document(documents, StandardDataFile::System)?;
    let object = expect_object(root, source.to_string())?;

    let mut identity_fields = Vec::new();
    for field_name in ["gameTitle", "currencyUnit"] {
        let exact_location =
            MzLocation::value(source.clone(), vec![MzLocationStep::key(field_name)]);
        let text = expect_string_field(object, field_name, &exact_location)?;
        push_text_field(&mut identity_fields, field_name, exact_location, text)?;
    }
    push_group(
        groups,
        TextGroupKind::System,
        MzLocation::value(source.clone(), Vec::new()),
        identity_fields,
    )?;

    let terms_location = MzLocation::value(source.clone(), vec![MzLocationStep::key("terms")]);
    let terms = object
        .get("terms")
        .ok_or_else(|| missing_value(&terms_location))?;
    let terms = expect_object(terms, terms_location.to_string())?;
    for field_name in ["basic", "commands", "params"] {
        let steps = vec![
            MzLocationStep::key("terms"),
            MzLocationStep::key(field_name),
        ];
        let location = MzLocation::value(source.clone(), steps.clone());
        let value = terms
            .get(field_name)
            .ok_or_else(|| missing_value(&location))?;
        extract_string_array_group(
            groups,
            source.clone(),
            steps,
            value,
            &format!("terms.{field_name}"),
        )?;
    }

    let messages_steps = vec![
        MzLocationStep::key("terms"),
        MzLocationStep::key("messages"),
    ];
    let messages_location = MzLocation::value(source.clone(), messages_steps.clone());
    let messages = terms
        .get("messages")
        .ok_or_else(|| missing_value(&messages_location))?;
    let messages = expect_object(messages, messages_location.to_string())?;
    extract_string_object_group(
        groups,
        source.clone(),
        messages_steps,
        messages,
        "terms.messages",
    )?;

    for field_name in [
        "elements",
        "skillTypes",
        "weaponTypes",
        "armorTypes",
        "equipTypes",
    ] {
        let steps = vec![MzLocationStep::key(field_name)];
        let location = MzLocation::value(source.clone(), steps.clone());
        let value = object
            .get(field_name)
            .ok_or_else(|| missing_value(&location))?;
        extract_string_array_group(groups, source.clone(), steps, value, field_name)?;
    }

    Ok(())
}

fn extract_string_array_group(
    groups: &mut Vec<ExtractedTextGroup>,
    source: MzSource,
    steps: Vec<MzLocationStep>,
    value: &Value,
    field_prefix: &str,
) -> Result<(), BuildBuiltinSnapshotError> {
    let group_location = MzLocation::value(source.clone(), steps.clone());
    let values = expect_array(value, group_location.to_string())?;
    let mut fields = Vec::new();
    for (index, value) in values.iter().enumerate() {
        if value.is_null() {
            continue;
        }
        let mut exact_steps = steps.clone();
        exact_steps.push(MzLocationStep::index(index));
        let exact_location = MzLocation::value(source.clone(), exact_steps);
        let text = expect_string(value, &exact_location)?;
        push_text_field(
            &mut fields,
            format!("{field_prefix}[{index}]"),
            exact_location,
            text,
        )?;
    }
    push_group(groups, TextGroupKind::System, group_location, fields)?;
    Ok(())
}

fn extract_string_object_group(
    groups: &mut Vec<ExtractedTextGroup>,
    source: MzSource,
    steps: Vec<MzLocationStep>,
    object: &Map<String, Value>,
    field_prefix: &str,
) -> Result<(), BuildBuiltinSnapshotError> {
    let group_location = MzLocation::value(source.clone(), steps.clone());
    let mut fields = Vec::new();
    for (key, value) in object {
        let mut exact_steps = steps.clone();
        exact_steps.push(MzLocationStep::key(key));
        let exact_location = MzLocation::value(source.clone(), exact_steps);
        let text = expect_string(value, &exact_location)?;
        push_text_field(
            &mut fields,
            format!("{field_prefix}.{key}"),
            exact_location,
            text,
        )?;
    }
    push_group(groups, TextGroupKind::System, group_location, fields)?;
    Ok(())
}

fn extract_maps(
    documents: &MzProjectDocuments,
    groups: &mut Vec<ExtractedTextGroup>,
) -> Result<(), BuildBuiltinSnapshotError> {
    for (document_id, document) in documents.documents() {
        let MzDocumentId::Map(map_id) = document_id else {
            continue;
        };
        let source = MzSource::map(*map_id);
        let root = expect_object(document, source.to_string())?;

        let display_location =
            MzLocation::value(source.clone(), vec![MzLocationStep::key("displayName")]);
        let display_name = expect_string_field(root, "displayName", &display_location)?;
        let mut fields = Vec::new();
        push_text_field(&mut fields, "displayName", display_location, display_name)?;
        push_group(
            groups,
            TextGroupKind::Map,
            MzLocation::value(source.clone(), Vec::new()),
            fields,
        )?;

        let events_location =
            MzLocation::value(source.clone(), vec![MzLocationStep::key("events")]);
        let events = root
            .get("events")
            .ok_or_else(|| missing_value(&events_location))?;
        let events = expect_array(events, events_location.to_string())?;
        extract_map_event_lists(source, events, groups)?;
    }
    Ok(())
}

fn extract_map_event_lists(
    source: MzSource,
    events: &[Value],
    groups: &mut Vec<ExtractedTextGroup>,
) -> Result<(), BuildBuiltinSnapshotError> {
    for (event_index, event) in events.iter().enumerate() {
        if event.is_null() {
            continue;
        }
        let event_steps = vec![
            MzLocationStep::key("events"),
            MzLocationStep::index(event_index),
        ];
        let event_location = MzLocation::value(source.clone(), event_steps.clone());
        let event = expect_object(event, event_location.to_string())?;
        let mut pages_steps = event_steps.clone();
        pages_steps.push(MzLocationStep::key("pages"));
        let pages_location = MzLocation::value(source.clone(), pages_steps.clone());
        let pages = event
            .get("pages")
            .ok_or_else(|| missing_value(&pages_location))?;
        let pages = expect_array(pages, pages_location.to_string())?;

        for (page_index, page) in pages.iter().enumerate() {
            let mut page_steps = pages_steps.clone();
            page_steps.push(MzLocationStep::index(page_index));
            let page_location = MzLocation::value(source.clone(), page_steps.clone());
            let page = expect_object(page, page_location.to_string())?;
            let mut list_steps = page_steps;
            list_steps.push(MzLocationStep::key("list"));
            let list_location = MzLocation::value(source.clone(), list_steps.clone());
            let list = page
                .get("list")
                .ok_or_else(|| missing_value(&list_location))?;
            let list = expect_array(list, list_location.to_string())?;
            extract_event_list(source.clone(), list_steps, list, groups)?;
        }
    }
    Ok(())
}

fn extract_common_events(
    documents: &MzProjectDocuments,
    groups: &mut Vec<ExtractedTextGroup>,
) -> Result<(), BuildBuiltinSnapshotError> {
    let source = MzSource::data(StandardDataFile::CommonEvents);
    let root = required_data_document(documents, StandardDataFile::CommonEvents)?;
    let events = expect_array(root, source.to_string())?;
    for (event_index, event) in events.iter().enumerate() {
        if event.is_null() {
            continue;
        }
        let event_steps = vec![MzLocationStep::index(event_index)];
        let event_location = MzLocation::value(source.clone(), event_steps.clone());
        let event = expect_object(event, event_location.to_string())?;
        let mut list_steps = event_steps;
        list_steps.push(MzLocationStep::key("list"));
        let list_location = MzLocation::value(source.clone(), list_steps.clone());
        let list = event
            .get("list")
            .ok_or_else(|| missing_value(&list_location))?;
        let list = expect_array(list, list_location.to_string())?;
        extract_event_list(source.clone(), list_steps, list, groups)?;
    }
    Ok(())
}

fn extract_troops(
    documents: &MzProjectDocuments,
    groups: &mut Vec<ExtractedTextGroup>,
) -> Result<(), BuildBuiltinSnapshotError> {
    let source = MzSource::data(StandardDataFile::Troops);
    let root = required_data_document(documents, StandardDataFile::Troops)?;
    let troops = expect_array(root, source.to_string())?;
    for (troop_index, troop) in troops.iter().enumerate() {
        if troop.is_null() {
            continue;
        }
        let troop_steps = vec![MzLocationStep::index(troop_index)];
        let troop_location = MzLocation::value(source.clone(), troop_steps.clone());
        let troop = expect_object(troop, troop_location.to_string())?;
        let mut pages_steps = troop_steps;
        pages_steps.push(MzLocationStep::key("pages"));
        let pages_location = MzLocation::value(source.clone(), pages_steps.clone());
        let pages = troop
            .get("pages")
            .ok_or_else(|| missing_value(&pages_location))?;
        let pages = expect_array(pages, pages_location.to_string())?;
        for (page_index, page) in pages.iter().enumerate() {
            let mut page_steps = pages_steps.clone();
            page_steps.push(MzLocationStep::index(page_index));
            let page_location = MzLocation::value(source.clone(), page_steps.clone());
            let page = expect_object(page, page_location.to_string())?;
            let mut list_steps = page_steps;
            list_steps.push(MzLocationStep::key("list"));
            let list_location = MzLocation::value(source.clone(), list_steps.clone());
            let list = page
                .get("list")
                .ok_or_else(|| missing_value(&list_location))?;
            let list = expect_array(list, list_location.to_string())?;
            extract_event_list(source.clone(), list_steps, list, groups)?;
        }
    }
    Ok(())
}

fn extract_event_list(
    source: MzSource,
    list_steps: Vec<MzLocationStep>,
    list: &[Value],
    groups: &mut Vec<ExtractedTextGroup>,
) -> Result<(), BuildBuiltinSnapshotError> {
    let mut command_index = 0;
    while command_index < list.len() {
        let command = command_at(&source, &list_steps, list, command_index)?;
        match command.code {
            101 => {
                command_index = extract_dialogue(
                    &source,
                    &list_steps,
                    list,
                    command_index,
                    command.parameters,
                    groups,
                )?;
            }
            102 => extract_choices(
                &source,
                &list_steps,
                command_index,
                command.parameters,
                groups,
            )?,
            105 => {
                command_index =
                    extract_scrolling_text(&source, &list_steps, list, command_index, groups)?;
            }
            320 => extract_actor_change(
                &source,
                &list_steps,
                command_index,
                command.parameters,
                "name",
                groups,
            )?,
            324 => extract_actor_change(
                &source,
                &list_steps,
                command_index,
                command.parameters,
                "nickname",
                groups,
            )?,
            325 => extract_actor_change(
                &source,
                &list_steps,
                command_index,
                command.parameters,
                "profile",
                groups,
            )?,
            401 | 405 => {
                return Err(BuiltinDocumentError::new(
                    command.location.to_string(),
                    format!("事件指令 {} 缺少对应的起始指令", command.code),
                )
                .into());
            }
            _ => {}
        }
        command_index += 1;
    }
    Ok(())
}

struct EventCommand<'a> {
    code: i64,
    parameters: &'a [Value],
    location: MzLocation,
}

fn command_at<'a>(
    source: &MzSource,
    list_steps: &[MzLocationStep],
    list: &'a [Value],
    command_index: usize,
) -> Result<EventCommand<'a>, BuiltinDocumentError> {
    let mut command_steps = list_steps.to_vec();
    command_steps.push(MzLocationStep::index(command_index));
    let location = MzLocation::value(source.clone(), command_steps);
    let command = expect_object(&list[command_index], location.to_string())?;
    let code = command
        .get("code")
        .and_then(Value::as_i64)
        .ok_or_else(|| BuiltinDocumentError::new(location.to_string(), "code 必须是整数"))?;
    let parameters = command
        .get("parameters")
        .ok_or_else(|| BuiltinDocumentError::new(location.to_string(), "缺少 parameters"))?;
    let parameters = expect_array(parameters, format!("{location}.parameters"))?;
    Ok(EventCommand {
        code,
        parameters,
        location,
    })
}

fn extract_dialogue(
    source: &MzSource,
    list_steps: &[MzLocationStep],
    list: &[Value],
    start_index: usize,
    parameters: &[Value],
    groups: &mut Vec<ExtractedTextGroup>,
) -> Result<usize, BuildBuiltinSnapshotError> {
    let group_location = command_location(source, list_steps, start_index);
    let mut fields = Vec::new();
    let speaker_location = parameter_location(source, list_steps, start_index, 4);
    let speaker = parameter_string(parameters, 4, &speaker_location)?;
    push_text_field(&mut fields, "speaker", speaker_location, speaker)?;

    let mut next_index = start_index + 1;
    let mut body_index = 0;
    while next_index < list.len() {
        let command = command_at(source, list_steps, list, next_index)?;
        if command.code != 401 {
            break;
        }
        let exact_location = parameter_location(source, list_steps, next_index, 0);
        let text = parameter_string(command.parameters, 0, &exact_location)?;
        push_text_field(
            &mut fields,
            format!("body[{body_index}]"),
            exact_location,
            text,
        )?;
        body_index += 1;
        next_index += 1;
    }

    push_group(groups, TextGroupKind::EventDialogue, group_location, fields)?;
    Ok(next_index.saturating_sub(1))
}

fn extract_choices(
    source: &MzSource,
    list_steps: &[MzLocationStep],
    command_index: usize,
    parameters: &[Value],
    groups: &mut Vec<ExtractedTextGroup>,
) -> Result<(), BuildBuiltinSnapshotError> {
    let choices_location = parameter_location(source, list_steps, command_index, 0);
    let choices = parameters
        .first()
        .ok_or_else(|| missing_value(&choices_location))?;
    let choices = expect_array(choices, choices_location.to_string())?;
    let mut fields = Vec::new();
    for (choice_index, choice) in choices.iter().enumerate() {
        let mut steps = value_steps(&choices_location);
        steps.push(MzLocationStep::index(choice_index));
        let exact_location = MzLocation::value(source.clone(), steps);
        let text = expect_string(choice, &exact_location)?;
        push_text_field(
            &mut fields,
            format!("choice[{choice_index}]"),
            exact_location,
            text,
        )?;
    }
    push_group(
        groups,
        TextGroupKind::EventChoices,
        command_location(source, list_steps, command_index),
        fields,
    )?;
    Ok(())
}

fn extract_scrolling_text(
    source: &MzSource,
    list_steps: &[MzLocationStep],
    list: &[Value],
    start_index: usize,
    groups: &mut Vec<ExtractedTextGroup>,
) -> Result<usize, BuildBuiltinSnapshotError> {
    let mut fields = Vec::new();
    let mut next_index = start_index + 1;
    let mut body_index = 0;
    while next_index < list.len() {
        let command = command_at(source, list_steps, list, next_index)?;
        if command.code != 405 {
            break;
        }
        let exact_location = parameter_location(source, list_steps, next_index, 0);
        let text = parameter_string(command.parameters, 0, &exact_location)?;
        push_text_field(
            &mut fields,
            format!("body[{body_index}]"),
            exact_location,
            text,
        )?;
        body_index += 1;
        next_index += 1;
    }
    push_group(
        groups,
        TextGroupKind::EventScrollingText,
        command_location(source, list_steps, start_index),
        fields,
    )?;
    Ok(next_index.saturating_sub(1))
}

fn extract_actor_change(
    source: &MzSource,
    list_steps: &[MzLocationStep],
    command_index: usize,
    parameters: &[Value],
    field_name: &str,
    groups: &mut Vec<ExtractedTextGroup>,
) -> Result<(), BuildBuiltinSnapshotError> {
    let exact_location = parameter_location(source, list_steps, command_index, 1);
    let text = parameter_string(parameters, 1, &exact_location)?;
    let mut fields = Vec::new();
    push_text_field(&mut fields, field_name, exact_location, text)?;
    push_group(
        groups,
        TextGroupKind::EventCommand,
        command_location(source, list_steps, command_index),
        fields,
    )?;
    Ok(())
}

fn command_location(
    source: &MzSource,
    list_steps: &[MzLocationStep],
    command_index: usize,
) -> MzLocation {
    let mut steps = list_steps.to_vec();
    steps.push(MzLocationStep::index(command_index));
    MzLocation::value(source.clone(), steps)
}

fn parameter_location(
    source: &MzSource,
    list_steps: &[MzLocationStep],
    command_index: usize,
    parameter_index: usize,
) -> MzLocation {
    let mut steps = list_steps.to_vec();
    steps.extend([
        MzLocationStep::index(command_index),
        MzLocationStep::key("parameters"),
        MzLocationStep::index(parameter_index),
    ]);
    MzLocation::value(source.clone(), steps)
}

fn value_steps(location: &MzLocation) -> Vec<MzLocationStep> {
    location.steps().to_vec()
}

fn parameter_string<'a>(
    parameters: &'a [Value],
    index: usize,
    location: &MzLocation,
) -> Result<&'a str, BuiltinDocumentError> {
    let value = parameters
        .get(index)
        .ok_or_else(|| missing_value(location))?;
    expect_string(value, location)
}

fn push_text_field(
    fields: &mut Vec<ExtractedTextField>,
    field_name: impl Into<String>,
    exact_location: MzLocation,
    original_text: &str,
) -> Result<(), SnapshotModelError> {
    if original_text.trim().is_empty() {
        return Ok(());
    }
    fields.push(ExtractedTextField::new(
        field_name,
        exact_location,
        original_text,
    )?);
    Ok(())
}

fn push_group(
    groups: &mut Vec<ExtractedTextGroup>,
    kind: TextGroupKind,
    group_location: MzLocation,
    fields: Vec<ExtractedTextField>,
) -> Result<(), SnapshotModelError> {
    if fields.is_empty() {
        return Ok(());
    }
    groups.push(ExtractedTextGroup::new(kind, group_location, fields)?);
    Ok(())
}

fn required_data_document(
    documents: &MzProjectDocuments,
    file: StandardDataFile,
) -> Result<&Value, BuiltinDocumentError> {
    documents
        .document(MzDocumentId::Data(file))
        .ok_or_else(|| BuiltinDocumentError::new(format!("data/{}", file.file_name()), "文档缺失"))
}

fn expect_object(
    value: &Value,
    location: impl Into<String>,
) -> Result<&Map<String, Value>, BuiltinDocumentError> {
    value
        .as_object()
        .ok_or_else(|| BuiltinDocumentError::new(location, "必须是对象"))
}

fn expect_array(
    value: &Value,
    location: impl Into<String>,
) -> Result<&[Value], BuiltinDocumentError> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| BuiltinDocumentError::new(location, "必须是数组"))
}

fn expect_string<'a>(
    value: &'a Value,
    location: &MzLocation,
) -> Result<&'a str, BuiltinDocumentError> {
    value
        .as_str()
        .ok_or_else(|| BuiltinDocumentError::new(location.to_string(), "必须是字符串"))
}

fn expect_string_field<'a>(
    object: &'a Map<String, Value>,
    field_name: &str,
    location: &MzLocation,
) -> Result<&'a str, BuiltinDocumentError> {
    let value = object
        .get(field_name)
        .ok_or_else(|| missing_value(location))?;
    expect_string(value, location)
}

fn missing_value(location: &MzLocation) -> BuiltinDocumentError {
    BuiltinDocumentError::new(location.to_string(), "字段或元素缺失")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;

    #[derive(Clone)]
    struct FakeReader {
        response: Result<MzProjectDocuments, FakeError>,
        selections: Arc<Mutex<Vec<MzDocumentSelection>>>,
    }

    impl MzProjectDocumentReader for FakeReader {
        type Error = FakeError;

        async fn read(
            &self,
            _project: &OpenedProject,
            selection: MzDocumentSelection,
        ) -> Result<MzProjectDocuments, Self::Error> {
            self.selections
                .lock()
                .expect("文档选择记录锁不应中毒")
                .push(selection);
            self.response.clone()
        }
    }

    #[derive(Clone)]
    struct FakeStore {
        snapshots: Arc<Mutex<Vec<BuiltinSnapshot>>>,
        failure: Option<FakeError>,
    }

    impl BuiltinSnapshotStore for FakeStore {
        type Error = FakeError;

        async fn replace_builtin(
            &self,
            _project: &OpenedProject,
            snapshot: BuiltinSnapshot,
        ) -> Result<(), Self::Error> {
            self.snapshots
                .lock()
                .expect("快照记录锁不应中毒")
                .push(snapshot);
            match &self.failure {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            }
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeError {}

    #[derive(Clone, Copy)]
    struct FakeCpu;

    impl CpuTaskExecutor for FakeCpu {
        type Error = FakeError;

        async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            Ok(task())
        }
    }

    #[derive(Clone)]
    struct RecordingCpu {
        calls: Arc<AtomicUsize>,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
        root_limit: usize,
    }

    impl CpuTaskExecutor for RecordingCpu {
        type Error = FakeError;

        async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let active = loop {
                let current = self.active.load(Ordering::SeqCst);
                if current < self.root_limit
                    && self
                        .active
                        .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                {
                    break current + 1;
                }
                tokio::task::yield_now().await;
            };
            self.max_active.fetch_max(active, Ordering::SeqCst);
            tokio::task::yield_now().await;
            let output = task();
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(output)
        }
    }

    #[derive(Clone, Copy)]
    struct PanickedCpu;

    impl CpuTaskExecutor for PanickedCpu {
        type Error = FakeError;

        async fn execute<T, F>(&self, _task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            Err(CpuTaskExecutionError::TaskPanicked)
        }
    }

    #[test]
    fn config_keeps_the_explicit_scan_limit() {
        let config =
            BuiltInExtractionConfig::new(NonZeroUsize::new(3).expect("测试并发数必须非零"));

        assert_eq!(config.scan_concurrency().get(), 3);
    }

    #[tokio::test]
    async fn parallel_scan_obeys_the_stage_limit_and_keeps_serial_results() {
        let expected = build_builtin_snapshot(&complete_documents()).expect("串行规格实现应该成功");
        let calls = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let cpu = RecordingCpu {
            calls: Arc::clone(&calls),
            active: Arc::new(AtomicUsize::new(0)),
            max_active: Arc::clone(&max_active),
            root_limit: 3,
        };

        let actual = build_builtin_snapshot_parallel(
            &cpu,
            BuiltInExtractionConfig::new(NonZeroUsize::new(3).expect("测试并发数必须非零")),
            complete_documents(),
        )
        .await
        .expect("并行 Builtin 扫描应该成功");

        assert_eq!(actual, expected);
        assert!(calls.load(Ordering::SeqCst) > 1);
        assert_eq!(max_active.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn root_cpu_budget_can_backpressure_a_larger_stage_limit() {
        let max_active = Arc::new(AtomicUsize::new(0));
        let cpu = RecordingCpu {
            calls: Arc::new(AtomicUsize::new(0)),
            active: Arc::new(AtomicUsize::new(0)),
            max_active: Arc::clone(&max_active),
            root_limit: 2,
        };

        build_builtin_snapshot_parallel(
            &cpu,
            BuiltInExtractionConfig::new(NonZeroUsize::new(4).expect("测试并发数必须非零")),
            complete_documents(),
        )
        .await
        .expect("根预算背压不应改变 Builtin 结果");

        assert_eq!(max_active.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cpu_panic_keeps_compute_stage_and_stops_before_store() {
        let snapshots = Arc::new(Mutex::new(Vec::new()));
        let service = BuiltInExtractionService::new(
            FakeReader {
                response: Ok(complete_documents()),
                selections: Arc::new(Mutex::new(Vec::new())),
            },
            FakeStore {
                snapshots: Arc::clone(&snapshots),
                failure: None,
            },
            PanickedCpu,
            BuiltInExtractionConfig::new(NonZeroUsize::new(2).expect("测试并发数必须非零")),
        );

        let error = service
            .refresh(&project())
            .await
            .expect_err("CPU panic 必须作为计算阶段失败返回");

        assert!(matches!(
            error,
            BuiltInExtractionError::ScheduleCompute(CpuTaskExecutionError::TaskPanicked)
        ));
        assert!(snapshots.lock().expect("快照锁不应中毒").is_empty());
    }

    #[tokio::test]
    async fn service_requests_exact_builtin_documents_and_persists_once() {
        let service = service(Ok(complete_documents()), None);

        service
            .refresh(&project())
            .await
            .expect("完整文档应该被保存");

        let selections = service
            .document_reader
            .selections
            .lock()
            .expect("文档选择记录锁不应中毒");
        assert_eq!(selections.len(), 1);
        assert_eq!(selections[0], builtin_document_selection());
        assert!(selections[0].includes_all_maps());
        assert!(!selections[0].includes_plugins());
        assert_eq!(
            selections[0].standard_files(),
            &[
                StandardDataFile::Actors,
                StandardDataFile::Armors,
                StandardDataFile::Classes,
                StandardDataFile::CommonEvents,
                StandardDataFile::Enemies,
                StandardDataFile::Items,
                StandardDataFile::Skills,
                StandardDataFile::States,
                StandardDataFile::System,
                StandardDataFile::Troops,
                StandardDataFile::Weapons,
            ]
            .into_iter()
            .collect()
        );
        let snapshots = service
            .snapshot_store
            .snapshots
            .lock()
            .expect("快照记录锁不应中毒");
        assert_eq!(snapshots.len(), 1);
        assert!(!snapshots[0].groups().is_empty());
    }

    #[tokio::test]
    async fn reader_failure_stops_before_store() {
        let service = service(Err(FakeError("read failed")), None);

        let error = service
            .refresh(&project())
            .await
            .expect_err("读取失败应该直接返回");

        assert!(matches!(
            error,
            BuiltInExtractionError::ReadDocuments(FakeError("read failed"))
        ));
        assert!(
            service
                .snapshot_store
                .snapshots
                .lock()
                .expect("快照记录锁不应中毒")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn malformed_document_stops_before_store() {
        let mut documents = complete_documents();
        documents.insert_document(
            MzDocumentId::Data(StandardDataFile::Items),
            json!([null, {"name": 42, "description": "说明"}]),
        );
        let service = service(Ok(documents), None);

        let error = service
            .refresh(&project())
            .await
            .expect_err("文档结构错误应该直接返回");

        assert!(matches!(
            error,
            BuiltInExtractionError::MalformedDocument(_)
        ));
        assert!(
            service
                .snapshot_store
                .snapshots
                .lock()
                .expect("快照记录锁不应中毒")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn store_failure_keeps_persist_stage_and_source() {
        let service = service(Ok(complete_documents()), Some(FakeError("persist failed")));

        let error = service
            .refresh(&project())
            .await
            .expect_err("保存失败应该返回 Persist 阶段");

        assert!(matches!(
            error,
            BuiltInExtractionError::Persist(FakeError("persist failed"))
        ));
        assert_eq!(
            error
                .source()
                .and_then(|source| source.downcast_ref::<FakeError>()),
            Some(&FakeError("persist failed"))
        );
        assert_eq!(
            service
                .snapshot_store
                .snapshots
                .lock()
                .expect("快照记录锁不应中毒")
                .len(),
            1
        );
    }

    #[test]
    fn refresh_future_is_send() {
        let service = service(Ok(complete_documents()), None);
        let project = project();

        assert_send(service.refresh(&project));
    }

    fn assert_send(_: impl Send) {}

    fn service(
        response: Result<MzProjectDocuments, FakeError>,
        store_failure: Option<FakeError>,
    ) -> BuiltInExtractionService<FakeReader, FakeStore, FakeCpu> {
        BuiltInExtractionService::new(
            FakeReader {
                response,
                selections: Arc::new(Mutex::new(Vec::new())),
            },
            FakeStore {
                snapshots: Arc::new(Mutex::new(Vec::new())),
                failure: store_failure,
            },
            FakeCpu,
            BuiltInExtractionConfig::new(NonZeroUsize::new(3).expect("测试并发数必须非零")),
        )
    }

    fn project() -> OpenedProject {
        OpenedProject::new(
            "示例项目".parse().expect("测试项目名称应该合法"),
            "C:/att/示例项目".into(),
            "C:/att/示例项目/project.db".into(),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::att_mz::project::test_layout_profile(),
        )
    }

    #[test]
    fn extracts_compound_database_system_map_and_event_text() {
        let snapshot =
            build_builtin_snapshot(&complete_documents()).expect("完整最小 MZ 文档应该形成快照");

        let item_group = group_at(&snapshot, "data/Items.json[1]");
        assert_eq!(item_group.kind(), TextGroupKind::DatabaseEntry);
        assert_eq!(field_text(item_group, "name"), "  宝剑  ");
        assert_eq!(field_text(item_group, "description"), "锋利的宝剑");

        let dialogue = snapshot
            .groups()
            .iter()
            .find(|group| group.kind() == TextGroupKind::EventDialogue)
            .expect("应该提取对话组");
        assert_eq!(field_text(dialogue, "speaker"), "莉莉");
        assert_eq!(field_text(dialogue, "body[0]"), "你好");
        assert_eq!(field_text(dialogue, "body[1]"), "Welcome");
        assert_ne!(
            dialogue.fields()[1].exact_location(),
            dialogue.fields()[2].exact_location(),
            "每个 401 正文行必须拥有独立地址"
        );

        let choices = snapshot
            .groups()
            .iter()
            .find(|group| group.kind() == TextGroupKind::EventChoices)
            .expect("应该提取选项组");
        assert_eq!(choices.fields().len(), 2);
        assert_ne!(
            choices.fields()[0].exact_location(),
            choices.fields()[1].exact_location()
        );

        assert!(snapshot.groups().iter().any(|group| {
            group
                .fields()
                .iter()
                .any(|field| field.field_name() == "message4")
        }));
        assert!(snapshot.groups().iter().any(|group| {
            group
                .fields()
                .iter()
                .any(|field| field.field_name() == "terms.messages.alwaysDash")
        }));
        assert!(snapshot.groups().iter().any(|group| {
            group.kind() == TextGroupKind::EventDialogue
                && group
                    .group_location()
                    .to_string()
                    .starts_with("data/Map001.json")
        }));
        assert!(snapshot.groups().iter().any(|group| {
            group.kind() == TextGroupKind::EventDialogue
                && group
                    .group_location()
                    .to_string()
                    .starts_with("data/Troops.json")
        }));
    }

    #[test]
    fn keeps_original_whitespace_skips_only_blank_and_does_not_filter_language() {
        let snapshot =
            build_builtin_snapshot(&complete_documents()).expect("完整最小 MZ 文档应该形成快照");
        let actor = group_at(&snapshot, "data/Actors.json[1]");

        assert_eq!(field_text(actor, "name"), "勇者");
        assert_eq!(field_text(actor, "profile"), " mixed 日本語 English ");
        assert!(
            actor
                .fields()
                .iter()
                .all(|field| field.field_name() != "nickname"),
            "纯空白昵称应该跳过"
        );
    }

    #[test]
    fn rejects_orphan_continuation_before_building_a_snapshot() {
        for code in [401, 405] {
            let mut documents = complete_documents();
            documents.insert_document(
                MzDocumentId::Data(StandardDataFile::CommonEvents),
                json!([null, {"list": [{"code": code, "parameters": ["孤立正文"]}]}]),
            );

            let error = match build_builtin_snapshot(&documents) {
                Ok(_) => panic!("孤立 {code} 必须失败"),
                Err(error) => error,
            };

            match error {
                BuildBuiltinSnapshotError::Malformed(error) => {
                    assert!(error.to_string().contains(&code.to_string()));
                    assert!(error.to_string().contains("缺少对应的起始指令"));
                }
                BuildBuiltinSnapshotError::Model(_) => panic!("应该是文档结构错误"),
            }
        }
    }

    #[test]
    fn rejects_wrong_fixed_field_type() {
        let mut documents = complete_documents();
        documents.insert_document(
            MzDocumentId::Data(StandardDataFile::Items),
            json!([null, {"name": 42, "description": "说明"}]),
        );

        let error = build_builtin_snapshot(&documents).expect_err("错误字段类型必须失败");

        match error {
            BuildBuiltinSnapshotError::Malformed(error) => {
                assert_eq!(error.location(), "data/Items.json[1].name");
                assert!(error.to_string().contains("必须是字符串"));
            }
            BuildBuiltinSnapshotError::Model(_) => panic!("应该是文档结构错误"),
        }
    }

    fn complete_documents() -> MzProjectDocuments {
        let mut documents = BTreeMap::new();
        documents.insert(
            MzDocumentId::Data(StandardDataFile::Actors),
            json!([null, {
                "name": "勇者",
                "nickname": "   ",
                "profile": " mixed 日本語 English "
            }]),
        );
        documents.insert(
            MzDocumentId::Data(StandardDataFile::Classes),
            json!([null, {"name": "战士"}]),
        );
        documents.insert(
            MzDocumentId::Data(StandardDataFile::Skills),
            json!([null, {
                "name": "斩击",
                "description": "攻击敌人",
                "message1": "发动斩击！",
                "message2": "",
            }]),
        );
        documents.insert(
            MzDocumentId::Data(StandardDataFile::Items),
            json!([null, {"name": "  宝剑  ", "description": "锋利的宝剑"}]),
        );
        documents.insert(
            MzDocumentId::Data(StandardDataFile::Weapons),
            json!([null, {"name": "木剑", "description": "练习用"}]),
        );
        documents.insert(
            MzDocumentId::Data(StandardDataFile::Armors),
            json!([null, {"name": "布衣", "description": "轻便"}]),
        );
        documents.insert(
            MzDocumentId::Data(StandardDataFile::Enemies),
            json!([null, {"name": "史莱姆"}]),
        );
        documents.insert(
            MzDocumentId::Data(StandardDataFile::States),
            json!([null, {
                "name": "中毒",
                "message1": "中了毒！",
                "message2": "中了毒！",
                "message3": "仍在中毒。",
                "message4": "毒消失了。"
            }]),
        );
        documents.insert(
            MzDocumentId::Data(StandardDataFile::System),
            json!({
                "gameTitle": "示例游戏",
                "currencyUnit": "G",
                "terms": {
                    "basic": ["等级"],
                    "commands": ["战斗"],
                    "params": ["生命"],
                    "messages": {"alwaysDash": "始终奔跑"}
                },
                "elements": [null, "火"],
                "skillTypes": [null, "魔法"],
                "weaponTypes": [null, "剑"],
                "armorTypes": [null, "轻甲"],
                "equipTypes": [null, "武器"]
            }),
        );
        documents.insert(
            MzDocumentId::Data(StandardDataFile::CommonEvents),
            json!([null, {"list": [
                {"code": 101, "parameters": ["", 0, 0, 2, "莉莉"]},
                {"code": 401, "parameters": ["你好"]},
                {"code": 401, "parameters": ["Welcome"]},
                {"code": 102, "parameters": [["接受", "拒绝"], -1, 0, 2, 0]},
                {"code": 105, "parameters": [2, false]},
                {"code": 405, "parameters": ["滚动一"]},
                {"code": 405, "parameters": ["滚动二"]},
                {"code": 320, "parameters": [1, "新名字"]},
                {"code": 324, "parameters": [1, "新昵称"]},
                {"code": 325, "parameters": [1, "新简介"]},
                {"code": 0, "parameters": []}
            ]}]),
        );
        documents.insert(
            MzDocumentId::Data(StandardDataFile::Troops),
            json!([null, {"pages": [{"list": [
                {"code": 101, "parameters": ["", 0, 0, 2, "敌人"]},
                {"code": 401, "parameters": ["受死吧！"]},
                {"code": 0, "parameters": []}
            ]}]}]),
        );
        documents.insert(
            MzDocumentId::Map(1),
            json!({
                "displayName": "起始村庄",
                "events": [null, {"pages": [{"list": [
                    {"code": 101, "parameters": ["", 0, 0, 2, "村民"]},
                    {"code": 401, "parameters": ["欢迎来到村庄。"]},
                    {"code": 0, "parameters": []}
                ]}]}]
            }),
        );
        MzProjectDocuments::new(documents, Vec::new())
    }

    fn group_at<'a>(snapshot: &'a BuiltinSnapshot, location: &str) -> &'a ExtractedTextGroup {
        snapshot
            .groups()
            .iter()
            .find(|group| group.group_location().to_string() == location)
            .unwrap_or_else(|| panic!("缺少文本组 {location}"))
    }

    fn field_text<'a>(group: &'a ExtractedTextGroup, field_name: &str) -> &'a str {
        group
            .fields()
            .iter()
            .find(|field| field.field_name() == field_name)
            .unwrap_or_else(|| panic!("缺少字段 {field_name}"))
            .original_text()
    }
}
