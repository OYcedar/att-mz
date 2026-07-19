//! RPG Maker 固定位置文本与标准事件块的完整 Builtin 快照提取。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::num::NonZeroUsize;

use futures_util::StreamExt;
use futures_util::stream;

use serde_json::{Map, Value};

use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::rpg_maker::dialogue::{
    DialoguePhysicalLine, MvDialogueDefinition, MvDialogueDefinitionError,
    MvDialogueProjectionError, MvDialogueProjector,
};
use crate::rpg_maker::model::{
    DialogueLinePart, DialogueLineRecipe, DialogueWriteRecipe, DirectSpeakerTarget, DirectTextPart,
    DirectTextRecipe, TextFieldRole, TextProjectionRecipe,
};
use crate::rpg_maker::project::OpenedProject;

use super::document::{
    RpgMakerDocumentId, RpgMakerDocumentSelection, RpgMakerProjectDocumentReader,
    RpgMakerProjectDocuments, StandardDataFile,
};
use super::model::{
    BuiltinSnapshot, ExtractedTextField, ExtractedTextGroup, RpgMakerLocation,
    RpgMakerLocationStep, RpgMakerSource, SnapshotModelError, TextGroupKind,
};
use super::store::{BuiltinProjectDefinitionUpdate, BuiltinSnapshotStore};

/// 刷新 RPG Maker 内置规格能够确定的标准文本资产。
pub(crate) trait BuiltInExtraction: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn refresh(
        &self,
        project: &OpenedProject,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 从无损 RPG Maker 文档建立 Builtin 快照，再由 Store 原子替换旧快照。
pub(crate) struct BuiltInExtractionService<R, S, C> {
    document_reader: R,
    snapshot_store: S,
    cpu_executor: C,
    config: BuiltInExtractionConfig,
    dialogue_definition: BuiltinDialogueDefinition,
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
            dialogue_definition: BuiltinDialogueDefinition::MzNative,
        }
    }

    /// 建立 MV Builtin 提取，并明确复用项目定义或完整替换定义。
    pub(crate) fn for_mv(
        document_reader: R,
        snapshot_store: S,
        cpu_executor: C,
        config: BuiltInExtractionConfig,
        dialogue_definition: MvDialogueDefinitionSelection,
    ) -> Self {
        Self {
            document_reader,
            snapshot_store,
            cpu_executor,
            config,
            dialogue_definition: match dialogue_definition {
                MvDialogueDefinitionSelection::ReuseProjectDefinition => {
                    BuiltinDialogueDefinition::MvReuseProjectDefinition
                }
                MvDialogueDefinitionSelection::Replace {
                    projector,
                    definition,
                } => BuiltinDialogueDefinition::MvReplace {
                    projector,
                    definition,
                },
            },
        }
    }
}

pub(crate) enum MvDialogueDefinitionSelection {
    ReuseProjectDefinition,
    Replace {
        projector: MvDialogueProjector,
        definition: MvDialogueDefinition,
    },
}

enum BuiltinDialogueDefinition {
    MzNative,
    MvReuseProjectDefinition,
    MvReplace {
        projector: MvDialogueProjector,
        definition: MvDialogueDefinition,
    },
}

impl BuiltinDialogueDefinition {
    fn resolve(
        &self,
        project: &OpenedProject,
    ) -> Result<
        (BuiltinDialogueProjection, BuiltinProjectDefinitionUpdate),
        MvDialogueDefinitionError,
    > {
        match self {
            Self::MzNative => Ok((
                BuiltinDialogueProjection::MzNative,
                BuiltinProjectDefinitionUpdate::Reuse,
            )),
            Self::MvReuseProjectDefinition => Ok((
                BuiltinDialogueProjection::MvInline(project.mv_dialogue_definition().compile()?),
                BuiltinProjectDefinitionUpdate::Reuse,
            )),
            Self::MvReplace {
                projector,
                definition,
            } => Ok((
                BuiltinDialogueProjection::MvInline(projector.fork_for_scan()),
                BuiltinProjectDefinitionUpdate::Replace(definition.clone()),
            )),
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

enum BuiltinDialogueProjection {
    MzNative,
    MvInline(MvDialogueProjector),
}

impl BuiltinDialogueProjection {
    fn fork_for_scan(&self) -> Self {
        match self {
            Self::MzNative => Self::MzNative,
            Self::MvInline(projector) => Self::MvInline(projector.fork_for_scan()),
        }
    }

    fn merge_scan(&mut self, other: Self) {
        match (self, other) {
            (Self::MzNative, Self::MzNative) => {}
            (Self::MvInline(current), Self::MvInline(scanned)) => current.merge_scan(scanned),
            _ => unreachable!("Builtin 并行分片必须使用同一种对话投影"),
        }
    }

    fn finish(self) -> Result<(), MvDialogueProjectionError> {
        match self {
            Self::MzNative => Ok(()),
            Self::MvInline(projector) => projector.finish(),
        }
    }
}

impl<R, S, C> BuiltInExtraction for BuiltInExtractionService<R, S, C>
where
    R: RpgMakerProjectDocumentReader,
    S: BuiltinSnapshotStore,
    C: CpuTaskExecutor,
{
    type Error = BuiltInExtractionError<R::Error, S::Error, C::Error>;

    async fn refresh(&self, project: &OpenedProject) -> Result<(), Self::Error> {
        let (dialogue_projection, project_definition_update) = self
            .dialogue_definition
            .resolve(project)
            .map_err(BuiltInExtractionError::CompileDialogueDefinition)?;
        let documents = self
            .document_reader
            .read(project, builtin_document_selection())
            .await
            .map_err(BuiltInExtractionError::ReadDocuments)?;

        let snapshot = match build_builtin_snapshot_parallel(
            &self.cpu_executor,
            self.config,
            documents,
            &dialogue_projection,
        )
        .await
        {
            Ok(snapshot) => snapshot,
            Err(ParallelBuiltinBuildError::Compute(source)) => {
                return Err(BuiltInExtractionError::ScheduleCompute(source));
            }
            Err(ParallelBuiltinBuildError::Build(BuildBuiltinSnapshotError::Malformed(source))) => {
                return Err(BuiltInExtractionError::MalformedDocument(source));
            }
            Err(ParallelBuiltinBuildError::Build(BuildBuiltinSnapshotError::Model(source))) => {
                return Err(BuiltInExtractionError::BuildSnapshot(*source));
            }
            Err(ParallelBuiltinBuildError::Build(BuildBuiltinSnapshotError::Dialogue(source))) => {
                return Err(BuiltInExtractionError::ProjectDialogue(source));
            }
        };

        self.snapshot_store
            .replace_builtin(project, snapshot, project_definition_update)
            .await
            .map_err(BuiltInExtractionError::Persist)?;
        Ok(())
    }
}

fn builtin_document_selection() -> RpgMakerDocumentSelection {
    RpgMakerDocumentSelection::new(
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
    CompileDialogueDefinition(MvDialogueDefinitionError),
    ReadDocuments(RE),
    ScheduleCompute(CpuTaskExecutionError<CE>),
    MalformedDocument(BuiltinDocumentError),
    BuildSnapshot(SnapshotModelError),
    ProjectDialogue(MvDialogueProjectionError),
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
            Self::CompileDialogueDefinition(source) => {
                write!(formatter, "编译 MV 对话定义失败：{source}")
            }
            Self::ReadDocuments(source) => write!(formatter, "读取 RPG Maker 文档失败：{source}"),
            Self::ScheduleCompute(source) => {
                write!(formatter, "调度 Builtin CPU 计算失败：{source}")
            }
            Self::MalformedDocument(source) => {
                write!(formatter, "RPG Maker 文档结构错误：{source}")
            }
            Self::BuildSnapshot(source) => write!(formatter, "构造 Builtin 快照失败：{source}"),
            Self::ProjectDialogue(source) => write!(formatter, "投影 MV 对话失败：{source}"),
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
            Self::CompileDialogueDefinition(source) => Some(source),
            Self::ReadDocuments(source) => Some(source),
            Self::ScheduleCompute(source) => Some(source),
            Self::MalformedDocument(source) => Some(source),
            Self::BuildSnapshot(source) => Some(source),
            Self::ProjectDialogue(source) => Some(source),
            Self::Persist(source) => Some(source),
        }
    }
}

/// 一个所选标准 RPG Maker 文档不符合固定结构。
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
    Dialogue(MvDialogueProjectionError),
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

impl From<MvDialogueProjectionError> for BuildBuiltinSnapshotError {
    fn from(value: MvDialogueProjectionError) -> Self {
        Self::Dialogue(value)
    }
}

#[cfg(test)]
fn build_builtin_snapshot(
    documents: &RpgMakerProjectDocuments,
) -> Result<BuiltinSnapshot, BuildBuiltinSnapshotError> {
    build_builtin_snapshot_with_dialogue_projection(documents, BuiltinDialogueProjection::MzNative)
}

#[cfg(test)]
fn build_builtin_snapshot_with_dialogue_projection(
    documents: &RpgMakerProjectDocuments,
    mut dialogue_projection: BuiltinDialogueProjection,
) -> Result<BuiltinSnapshot, BuildBuiltinSnapshotError> {
    let mut groups = Vec::new();

    for (file, fields) in database_specs() {
        extract_database_entries(documents, file, fields, &mut groups)?;
    }
    extract_system(documents, &mut groups)?;
    extract_maps(documents, &mut dialogue_projection, &mut groups)?;
    extract_common_events(documents, &mut dialogue_projection, &mut groups)?;
    extract_troops(documents, &mut dialogue_projection, &mut groups)?;
    dialogue_projection.finish()?;

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
    documents: RpgMakerProjectDocuments,
    dialogue_projection: &BuiltinDialogueProjection,
) -> Result<BuiltinSnapshot, ParallelBuiltinBuildError<C::Error>>
where
    C: CpuTaskExecutor,
{
    let work_units = builtin_work_units(documents, dialogue_projection).map_err(|error| {
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
    let mut dialogue_projection = dialogue_projection.fork_for_scan();
    for result in results {
        let local_result = result.map_err(ParallelBuiltinBuildError::Compute)?;
        let local_result = local_result.map_err(ParallelBuiltinBuildError::Build)?;
        groups.extend(local_result.groups);
        if let Some(scanned) = local_result.dialogue_projection {
            dialogue_projection.merge_scan(scanned);
        }
    }
    dialogue_projection
        .finish()
        .map_err(BuildBuiltinSnapshotError::Dialogue)
        .map_err(ParallelBuiltinBuildError::Build)?;

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
        dialogue_projection: BuiltinDialogueProjection,
    },
    CommonEvents {
        document: Value,
        dialogue_projection: BuiltinDialogueProjection,
    },
    Troops {
        document: Value,
        dialogue_projection: BuiltinDialogueProjection,
    },
}

struct BuiltinWorkUnitResult {
    groups: Vec<ExtractedTextGroup>,
    dialogue_projection: Option<BuiltinDialogueProjection>,
}

impl BuiltinWorkUnit {
    fn run(self) -> Result<BuiltinWorkUnitResult, BuildBuiltinSnapshotError> {
        let mut groups = Vec::new();
        let dialogue_projection = match self {
            Self::Database {
                file,
                field_names,
                document,
            } => {
                let documents = single_document(RpgMakerDocumentId::Data(file), document);
                extract_database_entries(&documents, file, field_names, &mut groups)?;
                None
            }
            Self::System(document) => {
                let documents =
                    single_document(RpgMakerDocumentId::Data(StandardDataFile::System), document);
                extract_system(&documents, &mut groups)?;
                None
            }
            Self::Map {
                map_id,
                document,
                mut dialogue_projection,
            } => {
                let documents = single_document(RpgMakerDocumentId::Map(map_id), document);
                extract_maps(&documents, &mut dialogue_projection, &mut groups)?;
                Some(dialogue_projection)
            }
            Self::CommonEvents {
                document,
                mut dialogue_projection,
            } => {
                let documents = single_document(
                    RpgMakerDocumentId::Data(StandardDataFile::CommonEvents),
                    document,
                );
                extract_common_events(&documents, &mut dialogue_projection, &mut groups)?;
                Some(dialogue_projection)
            }
            Self::Troops {
                document,
                mut dialogue_projection,
            } => {
                let documents =
                    single_document(RpgMakerDocumentId::Data(StandardDataFile::Troops), document);
                extract_troops(&documents, &mut dialogue_projection, &mut groups)?;
                Some(dialogue_projection)
            }
        };
        Ok(BuiltinWorkUnitResult {
            groups,
            dialogue_projection,
        })
    }
}

fn single_document(id: RpgMakerDocumentId, document: Value) -> RpgMakerProjectDocuments {
    RpgMakerProjectDocuments::new([(id, document)].into_iter().collect(), Vec::new())
}

fn builtin_work_units(
    documents: RpgMakerProjectDocuments,
    dialogue_projection: &BuiltinDialogueProjection,
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
            RpgMakerDocumentId::Map(map_id) => Some(*map_id),
            RpgMakerDocumentId::Data(_) | RpgMakerDocumentId::DataFile(_) => None,
        })
        .collect::<Vec<_>>();
    for map_id in map_ids {
        let document = documents
            .remove(&RpgMakerDocumentId::Map(map_id))
            .expect("刚从文档键集合取得的地图必须仍然存在");
        work_units.push(BuiltinWorkUnit::Map {
            map_id,
            document,
            dialogue_projection: dialogue_projection.fork_for_scan(),
        });
    }

    work_units.push(BuiltinWorkUnit::CommonEvents {
        document: take_data_document(&mut documents, StandardDataFile::CommonEvents)?,
        dialogue_projection: dialogue_projection.fork_for_scan(),
    });
    work_units.push(BuiltinWorkUnit::Troops {
        document: take_data_document(&mut documents, StandardDataFile::Troops)?,
        dialogue_projection: dialogue_projection.fork_for_scan(),
    });

    Ok(work_units)
}

fn take_data_document(
    documents: &mut std::collections::BTreeMap<RpgMakerDocumentId, Value>,
    file: StandardDataFile,
) -> Result<Value, BuiltinDocumentError> {
    documents
        .remove(&RpgMakerDocumentId::Data(file))
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
    documents: &RpgMakerProjectDocuments,
    file: StandardDataFile,
    field_names: &[&str],
    groups: &mut Vec<ExtractedTextGroup>,
) -> Result<(), BuildBuiltinSnapshotError> {
    let source = RpgMakerSource::data(file);
    let root = required_data_document(documents, file)?;
    let entries = expect_array(root, source.to_string())?;

    for (entry_index, entry) in entries.iter().enumerate() {
        if entry.is_null() {
            continue;
        }
        let entry_steps = vec![RpgMakerLocationStep::index(entry_index)];
        let entry_location = RpgMakerLocation::value(source.clone(), entry_steps.clone());
        let object = expect_object(entry, entry_location.to_string())?;
        let mut fields = Vec::new();
        for field_name in field_names {
            let mut field_steps = entry_steps.clone();
            field_steps.push(RpgMakerLocationStep::key(*field_name));
            let exact_location = RpgMakerLocation::value(source.clone(), field_steps);
            let text = expect_string_field(object, field_name, &exact_location)?;
            push_text_field(&mut fields, *field_name, exact_location, text)?;
        }
        push_group(groups, TextGroupKind::DatabaseEntry, entry_location, fields)?;
    }
    Ok(())
}

fn extract_system(
    documents: &RpgMakerProjectDocuments,
    groups: &mut Vec<ExtractedTextGroup>,
) -> Result<(), BuildBuiltinSnapshotError> {
    let source = RpgMakerSource::data(StandardDataFile::System);
    let root = required_data_document(documents, StandardDataFile::System)?;
    let object = expect_object(root, source.to_string())?;

    let mut identity_fields = Vec::new();
    for field_name in ["gameTitle", "currencyUnit"] {
        let exact_location =
            RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::key(field_name)]);
        let text = expect_string_field(object, field_name, &exact_location)?;
        push_text_field(&mut identity_fields, field_name, exact_location, text)?;
    }
    push_group(
        groups,
        TextGroupKind::System,
        RpgMakerLocation::value(source.clone(), Vec::new()),
        identity_fields,
    )?;

    let terms_location =
        RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::key("terms")]);
    let terms = object
        .get("terms")
        .ok_or_else(|| missing_value(&terms_location))?;
    let terms = expect_object(terms, terms_location.to_string())?;
    for field_name in ["basic", "commands", "params"] {
        let steps = vec![
            RpgMakerLocationStep::key("terms"),
            RpgMakerLocationStep::key(field_name),
        ];
        let location = RpgMakerLocation::value(source.clone(), steps.clone());
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
        RpgMakerLocationStep::key("terms"),
        RpgMakerLocationStep::key("messages"),
    ];
    let messages_location = RpgMakerLocation::value(source.clone(), messages_steps.clone());
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
        let steps = vec![RpgMakerLocationStep::key(field_name)];
        let location = RpgMakerLocation::value(source.clone(), steps.clone());
        let value = object
            .get(field_name)
            .ok_or_else(|| missing_value(&location))?;
        extract_string_array_group(groups, source.clone(), steps, value, field_name)?;
    }

    Ok(())
}

fn extract_string_array_group(
    groups: &mut Vec<ExtractedTextGroup>,
    source: RpgMakerSource,
    steps: Vec<RpgMakerLocationStep>,
    value: &Value,
    field_prefix: &str,
) -> Result<(), BuildBuiltinSnapshotError> {
    let group_location = RpgMakerLocation::value(source.clone(), steps.clone());
    let values = expect_array(value, group_location.to_string())?;
    let mut fields = Vec::new();
    for (index, value) in values.iter().enumerate() {
        if value.is_null() {
            continue;
        }
        let mut exact_steps = steps.clone();
        exact_steps.push(RpgMakerLocationStep::index(index));
        let exact_location = RpgMakerLocation::value(source.clone(), exact_steps);
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
    source: RpgMakerSource,
    steps: Vec<RpgMakerLocationStep>,
    object: &Map<String, Value>,
    field_prefix: &str,
) -> Result<(), BuildBuiltinSnapshotError> {
    let group_location = RpgMakerLocation::value(source.clone(), steps.clone());
    let mut fields = Vec::new();
    for (key, value) in object {
        let mut exact_steps = steps.clone();
        exact_steps.push(RpgMakerLocationStep::key(key));
        let exact_location = RpgMakerLocation::value(source.clone(), exact_steps);
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
    documents: &RpgMakerProjectDocuments,
    dialogue_projection: &mut BuiltinDialogueProjection,
    groups: &mut Vec<ExtractedTextGroup>,
) -> Result<(), BuildBuiltinSnapshotError> {
    for (document_id, document) in documents.documents() {
        let RpgMakerDocumentId::Map(map_id) = document_id else {
            continue;
        };
        let source = RpgMakerSource::map(*map_id);
        let root = expect_object(document, source.to_string())?;

        let display_location = RpgMakerLocation::value(
            source.clone(),
            vec![RpgMakerLocationStep::key("displayName")],
        );
        let display_name = expect_string_field(root, "displayName", &display_location)?;
        let mut fields = Vec::new();
        push_text_field(&mut fields, "displayName", display_location, display_name)?;
        push_group(
            groups,
            TextGroupKind::Map,
            RpgMakerLocation::value(source.clone(), Vec::new()),
            fields,
        )?;

        let events_location =
            RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::key("events")]);
        let events = root
            .get("events")
            .ok_or_else(|| missing_value(&events_location))?;
        let events = expect_array(events, events_location.to_string())?;
        extract_map_event_lists(source, events, dialogue_projection, groups)?;
    }
    Ok(())
}

fn extract_map_event_lists(
    source: RpgMakerSource,
    events: &[Value],
    dialogue_projection: &mut BuiltinDialogueProjection,
    groups: &mut Vec<ExtractedTextGroup>,
) -> Result<(), BuildBuiltinSnapshotError> {
    for (event_index, event) in events.iter().enumerate() {
        if event.is_null() {
            continue;
        }
        let event_steps = vec![
            RpgMakerLocationStep::key("events"),
            RpgMakerLocationStep::index(event_index),
        ];
        let event_location = RpgMakerLocation::value(source.clone(), event_steps.clone());
        let event = expect_object(event, event_location.to_string())?;
        let mut pages_steps = event_steps.clone();
        pages_steps.push(RpgMakerLocationStep::key("pages"));
        let pages_location = RpgMakerLocation::value(source.clone(), pages_steps.clone());
        let pages = event
            .get("pages")
            .ok_or_else(|| missing_value(&pages_location))?;
        let pages = expect_array(pages, pages_location.to_string())?;

        for (page_index, page) in pages.iter().enumerate() {
            let mut page_steps = pages_steps.clone();
            page_steps.push(RpgMakerLocationStep::index(page_index));
            let page_location = RpgMakerLocation::value(source.clone(), page_steps.clone());
            let page = expect_object(page, page_location.to_string())?;
            let mut list_steps = page_steps;
            list_steps.push(RpgMakerLocationStep::key("list"));
            let list_location = RpgMakerLocation::value(source.clone(), list_steps.clone());
            let list = page
                .get("list")
                .ok_or_else(|| missing_value(&list_location))?;
            let list = expect_array(list, list_location.to_string())?;
            extract_event_list(
                source.clone(),
                list_steps,
                list,
                dialogue_projection,
                groups,
            )?;
        }
    }
    Ok(())
}

fn extract_common_events(
    documents: &RpgMakerProjectDocuments,
    dialogue_projection: &mut BuiltinDialogueProjection,
    groups: &mut Vec<ExtractedTextGroup>,
) -> Result<(), BuildBuiltinSnapshotError> {
    let source = RpgMakerSource::data(StandardDataFile::CommonEvents);
    let root = required_data_document(documents, StandardDataFile::CommonEvents)?;
    let events = expect_array(root, source.to_string())?;
    for (event_index, event) in events.iter().enumerate() {
        if event.is_null() {
            continue;
        }
        let event_steps = vec![RpgMakerLocationStep::index(event_index)];
        let event_location = RpgMakerLocation::value(source.clone(), event_steps.clone());
        let event = expect_object(event, event_location.to_string())?;
        let mut list_steps = event_steps;
        list_steps.push(RpgMakerLocationStep::key("list"));
        let list_location = RpgMakerLocation::value(source.clone(), list_steps.clone());
        let list = event
            .get("list")
            .ok_or_else(|| missing_value(&list_location))?;
        let list = expect_array(list, list_location.to_string())?;
        extract_event_list(
            source.clone(),
            list_steps,
            list,
            dialogue_projection,
            groups,
        )?;
    }
    Ok(())
}

fn extract_troops(
    documents: &RpgMakerProjectDocuments,
    dialogue_projection: &mut BuiltinDialogueProjection,
    groups: &mut Vec<ExtractedTextGroup>,
) -> Result<(), BuildBuiltinSnapshotError> {
    let source = RpgMakerSource::data(StandardDataFile::Troops);
    let root = required_data_document(documents, StandardDataFile::Troops)?;
    let troops = expect_array(root, source.to_string())?;
    for (troop_index, troop) in troops.iter().enumerate() {
        if troop.is_null() {
            continue;
        }
        let troop_steps = vec![RpgMakerLocationStep::index(troop_index)];
        let troop_location = RpgMakerLocation::value(source.clone(), troop_steps.clone());
        let troop = expect_object(troop, troop_location.to_string())?;
        let mut pages_steps = troop_steps;
        pages_steps.push(RpgMakerLocationStep::key("pages"));
        let pages_location = RpgMakerLocation::value(source.clone(), pages_steps.clone());
        let pages = troop
            .get("pages")
            .ok_or_else(|| missing_value(&pages_location))?;
        let pages = expect_array(pages, pages_location.to_string())?;
        for (page_index, page) in pages.iter().enumerate() {
            let mut page_steps = pages_steps.clone();
            page_steps.push(RpgMakerLocationStep::index(page_index));
            let page_location = RpgMakerLocation::value(source.clone(), page_steps.clone());
            let page = expect_object(page, page_location.to_string())?;
            let mut list_steps = page_steps;
            list_steps.push(RpgMakerLocationStep::key("list"));
            let list_location = RpgMakerLocation::value(source.clone(), list_steps.clone());
            let list = page
                .get("list")
                .ok_or_else(|| missing_value(&list_location))?;
            let list = expect_array(list, list_location.to_string())?;
            extract_event_list(
                source.clone(),
                list_steps,
                list,
                dialogue_projection,
                groups,
            )?;
        }
    }
    Ok(())
}

fn extract_event_list(
    source: RpgMakerSource,
    list_steps: Vec<RpgMakerLocationStep>,
    list: &[Value],
    dialogue_projection: &mut BuiltinDialogueProjection,
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
                    dialogue_projection,
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
    location: RpgMakerLocation,
}

fn command_at<'a>(
    source: &RpgMakerSource,
    list_steps: &[RpgMakerLocationStep],
    list: &'a [Value],
    command_index: usize,
) -> Result<EventCommand<'a>, BuiltinDocumentError> {
    let mut command_steps = list_steps.to_vec();
    command_steps.push(RpgMakerLocationStep::index(command_index));
    let location = RpgMakerLocation::value(source.clone(), command_steps);
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
    source: &RpgMakerSource,
    list_steps: &[RpgMakerLocationStep],
    list: &[Value],
    start_index: usize,
    parameters: &[Value],
    dialogue_projection: &mut BuiltinDialogueProjection,
    groups: &mut Vec<ExtractedTextGroup>,
) -> Result<usize, BuildBuiltinSnapshotError> {
    let group_location = command_location(source, list_steps, start_index);
    let mut next_index = start_index + 1;
    let mut lines = Vec::new();
    while next_index < list.len() {
        let command = command_at(source, list_steps, list, next_index)?;
        if command.code != 401 {
            break;
        }
        let exact_location = parameter_location(source, list_steps, next_index, 0);
        let text = parameter_string(command.parameters, 0, &exact_location)?;
        lines.push(DialoguePhysicalLine::new(exact_location, text));
        next_index += 1;
    }

    let group = match dialogue_projection {
        BuiltinDialogueProjection::MzNative => project_mz_dialogue(
            source,
            list_steps,
            start_index,
            parameters,
            group_location,
            lines,
        )?,
        BuiltinDialogueProjection::MvInline(projector) => {
            project_mv_dialogue(projector, group_location, lines)?
        }
    };
    groups.push(group);
    Ok(next_index.saturating_sub(1))
}

fn project_mz_dialogue(
    source: &RpgMakerSource,
    list_steps: &[RpgMakerLocationStep],
    start_index: usize,
    parameters: &[Value],
    group_location: RpgMakerLocation,
    lines: Vec<DialoguePhysicalLine>,
) -> Result<ExtractedTextGroup, BuildBuiltinSnapshotError> {
    let mut fields = Vec::new();
    let speaker_location = parameter_location(source, list_steps, start_index, 4);
    let direct_speaker = match parameters.get(4) {
        None => None,
        Some(value) => {
            let speaker = expect_string(value, &speaker_location)?;
            if speaker.trim().is_empty() {
                None
            } else {
                fields.push(ExtractedTextField::projected(
                    TextFieldRole::DialogueSpeaker,
                    speaker_location.clone(),
                    speaker,
                )?);
                Some(DirectSpeakerTarget::new(speaker_location, speaker))
            }
        }
    };

    let mut body_index = 0;
    let mut line_recipes = Vec::with_capacity(lines.len());
    for line in lines {
        let parts = if line.expected_raw().trim().is_empty() {
            vec![DialogueLinePart::Literal(line.expected_raw().to_owned())]
        } else {
            fields.push(ExtractedTextField::projected(
                TextFieldRole::DialogueBody { index: body_index },
                line.physical_location().clone(),
                line.expected_raw(),
            )?);
            let parts = vec![DialogueLinePart::BodySlot { index: body_index }];
            body_index += 1;
            parts
        };
        line_recipes.push(
            DialogueLineRecipe::new(line.physical_location().clone(), line.expected_raw(), parts)
                .map_err(SnapshotModelError::Projection)?,
        );
    }

    let recipe = DialogueWriteRecipe::new(group_location.clone(), direct_speaker, line_recipes)
        .map_err(SnapshotModelError::Projection)?;
    ExtractedTextGroup::projected(
        TextGroupKind::EventDialogue,
        group_location,
        fields,
        vec![TextProjectionRecipe::Dialogue(recipe)],
    )
    .map_err(Into::into)
}

fn project_mv_dialogue(
    projector: &mut MvDialogueProjector,
    group_location: RpgMakerLocation,
    lines: Vec<DialoguePhysicalLine>,
) -> Result<ExtractedTextGroup, BuildBuiltinSnapshotError> {
    let projected = projector.project(group_location.clone(), lines)?;
    let (leaves, recipe) = projected.into_parts();
    let fields = leaves
        .into_iter()
        .map(|leaf| {
            let (role, physical_location, original_text) = leaf.into_parts();
            ExtractedTextField::projected(role, physical_location, original_text)
        })
        .collect::<Result<Vec<_>, _>>()?;
    ExtractedTextGroup::projected(
        TextGroupKind::EventDialogue,
        group_location,
        fields,
        vec![TextProjectionRecipe::Dialogue(recipe)],
    )
    .map_err(Into::into)
}

fn extract_choices(
    source: &RpgMakerSource,
    list_steps: &[RpgMakerLocationStep],
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
        steps.push(RpgMakerLocationStep::index(choice_index));
        let exact_location = RpgMakerLocation::value(source.clone(), steps);
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
    source: &RpgMakerSource,
    list_steps: &[RpgMakerLocationStep],
    list: &[Value],
    start_index: usize,
    groups: &mut Vec<ExtractedTextGroup>,
) -> Result<usize, BuildBuiltinSnapshotError> {
    let mut fields = Vec::new();
    let mut recipes = Vec::new();
    let mut next_index = start_index + 1;
    let mut body_index = 0;
    while next_index < list.len() {
        let command = command_at(source, list_steps, list, next_index)?;
        if command.code != 405 {
            break;
        }
        let exact_location = parameter_location(source, list_steps, next_index, 0);
        let text = parameter_string(command.parameters, 0, &exact_location)?;
        let parts = if text.trim().is_empty() {
            vec![DirectTextPart::Literal(text.to_owned())]
        } else {
            let role = TextFieldRole::ScrollingTextBody { index: body_index };
            fields.push(ExtractedTextField::projected(
                role.clone(),
                exact_location.clone(),
                text,
            )?);
            vec![DirectTextPart::TextSlot { role }]
        };
        recipes.push(TextProjectionRecipe::Direct(
            DirectTextRecipe::new(exact_location, text, parts)
                .map_err(SnapshotModelError::Projection)?,
        ));
        body_index += 1;
        next_index += 1;
    }
    if !fields.is_empty() {
        groups.push(ExtractedTextGroup::projected(
            TextGroupKind::EventScrollingText,
            command_location(source, list_steps, start_index),
            fields,
            recipes,
        )?);
    }
    Ok(next_index.saturating_sub(1))
}

fn extract_actor_change(
    source: &RpgMakerSource,
    list_steps: &[RpgMakerLocationStep],
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
    source: &RpgMakerSource,
    list_steps: &[RpgMakerLocationStep],
    command_index: usize,
) -> RpgMakerLocation {
    let mut steps = list_steps.to_vec();
    steps.push(RpgMakerLocationStep::index(command_index));
    RpgMakerLocation::value(source.clone(), steps)
}

fn parameter_location(
    source: &RpgMakerSource,
    list_steps: &[RpgMakerLocationStep],
    command_index: usize,
    parameter_index: usize,
) -> RpgMakerLocation {
    let mut steps = list_steps.to_vec();
    steps.extend([
        RpgMakerLocationStep::index(command_index),
        RpgMakerLocationStep::key("parameters"),
        RpgMakerLocationStep::index(parameter_index),
    ]);
    RpgMakerLocation::value(source.clone(), steps)
}

fn value_steps(location: &RpgMakerLocation) -> Vec<RpgMakerLocationStep> {
    location.steps().to_vec()
}

fn parameter_string<'a>(
    parameters: &'a [Value],
    index: usize,
    location: &RpgMakerLocation,
) -> Result<&'a str, BuiltinDocumentError> {
    let value = parameters
        .get(index)
        .ok_or_else(|| missing_value(location))?;
    expect_string(value, location)
}

fn push_text_field(
    fields: &mut Vec<ExtractedTextField>,
    field_name: impl Into<String>,
    exact_location: RpgMakerLocation,
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
    group_location: RpgMakerLocation,
    fields: Vec<ExtractedTextField>,
) -> Result<(), SnapshotModelError> {
    if fields.is_empty() {
        return Ok(());
    }
    groups.push(ExtractedTextGroup::new(kind, group_location, fields)?);
    Ok(())
}

fn required_data_document(
    documents: &RpgMakerProjectDocuments,
    file: StandardDataFile,
) -> Result<&Value, BuiltinDocumentError> {
    documents
        .document(RpgMakerDocumentId::Data(file))
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
    location: &RpgMakerLocation,
) -> Result<&'a str, BuiltinDocumentError> {
    value
        .as_str()
        .ok_or_else(|| BuiltinDocumentError::new(location.to_string(), "必须是字符串"))
}

fn expect_string_field<'a>(
    object: &'a Map<String, Value>,
    field_name: &str,
    location: &RpgMakerLocation,
) -> Result<&'a str, BuiltinDocumentError> {
    let value = object
        .get(field_name)
        .ok_or_else(|| missing_value(location))?;
    expect_string(value, location)
}

fn missing_value(location: &RpgMakerLocation) -> BuiltinDocumentError {
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
        response: Result<RpgMakerProjectDocuments, FakeError>,
        selections: Arc<Mutex<Vec<RpgMakerDocumentSelection>>>,
    }

    impl RpgMakerProjectDocumentReader for FakeReader {
        type Error = FakeError;

        async fn read(
            &self,
            _project: &OpenedProject,
            selection: RpgMakerDocumentSelection,
        ) -> Result<RpgMakerProjectDocuments, Self::Error> {
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
        project_definition_updates: Arc<Mutex<Vec<BuiltinProjectDefinitionUpdate>>>,
        failure: Option<FakeError>,
    }

    impl BuiltinSnapshotStore for FakeStore {
        type Error = FakeError;

        async fn replace_builtin(
            &self,
            _project: &OpenedProject,
            snapshot: BuiltinSnapshot,
            project_definition_update: BuiltinProjectDefinitionUpdate,
        ) -> Result<(), Self::Error> {
            self.snapshots
                .lock()
                .expect("快照记录锁不应中毒")
                .push(snapshot);
            self.project_definition_updates
                .lock()
                .expect("项目定义更新记录锁不应中毒")
                .push(project_definition_update);
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
            &BuiltinDialogueProjection::MzNative,
        )
        .await
        .expect("并行 Builtin 扫描应该成功");

        assert_eq!(actual, expected);
        assert!(calls.load(Ordering::SeqCst) > 1);
        assert_eq!(max_active.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn mv_projection_reuses_event_scan_and_merges_rule_hits_across_work_units() {
        let documents = mv_dialogue_documents();
        let definition = crate::rpg_maker::dialogue::MvDialogueDefinition::parse_toml(
            r#"
                [[rule]]
                pattern = '\\n<(?<speaker>[^>:]+):>'

                [[rule]]
                pattern = '\A(?<speaker>バニー淫魔)\z'
            "#,
        )
        .expect("MV 对话定义应合法");
        let projection =
            BuiltinDialogueProjection::MvInline(definition.compile().expect("MV 对话规则应能编译"));
        let expected =
            build_builtin_snapshot_with_dialogue_projection(&documents, projection.fork_for_scan())
                .expect("两个规则分布在不同文档时应在整份快照上验收");

        let actual = build_builtin_snapshot_parallel(
            &FakeCpu,
            BuiltInExtractionConfig::new(NonZeroUsize::new(3).expect("测试并发数必须非零")),
            documents,
            &projection,
        )
        .await
        .expect("并行扫描必须合并各分片的规则命中数");

        assert_eq!(actual, expected);
        let common_dialogue = actual
            .groups()
            .iter()
            .find(|group| {
                group.kind() == TextGroupKind::EventDialogue
                    && group
                        .group_location()
                        .to_string()
                        .starts_with("data/CommonEvents.json")
            })
            .expect("应提取公共事件中的 inline 姓名");
        assert_eq!(field_text(common_dialogue, "speaker"), "莉莉");
        assert_eq!(field_text(common_dialogue, "body[0]"), "你好");
        assert_eq!(field_text(common_dialogue, "body[1]"), "第二行");
        let [TextProjectionRecipe::Dialogue(recipe)] = common_dialogue.recipes() else {
            panic!("MV 对话必须使用唯一块级配方");
        };
        assert_eq!(recipe.lines().len(), 3, "空白 401 也必须进入写回配方");
        assert!(matches!(
            recipe.lines()[1].parts(),
            [DialogueLinePart::Literal(value)] if value == "   "
        ));

        let map_dialogue = actual
            .groups()
            .iter()
            .find(|group| {
                group.kind() == TextGroupKind::EventDialogue
                    && group
                        .group_location()
                        .to_string()
                        .starts_with("data/Map001.json")
            })
            .expect("应提取地图中的精确首行姓名");
        assert_eq!(field_text(map_dialogue, "speaker"), "バニー淫魔");
        assert_eq!(field_text(map_dialogue, "body[0]"), "「台词」");
        let [TextProjectionRecipe::Dialogue(recipe)] = map_dialogue.recipes() else {
            panic!("MV 对话必须使用唯一块级配方");
        };
        assert!(
            recipe.lines()[0]
                .parts()
                .iter()
                .all(|part| !matches!(part, DialogueLinePart::BodySlot { .. })),
            "整条第一行是姓名时不得建立正文叶"
        );
    }

    #[test]
    fn mv_projection_rejects_a_rule_without_any_non_blank_speaker_in_the_snapshot() {
        let definition = crate::rpg_maker::dialogue::MvDialogueDefinition::parse_toml(
            "[[rule]]\npattern = '\\A(?<speaker>不会出现)\\z'\n",
        )
        .expect("MV 对话定义应合法");
        let error = build_builtin_snapshot_with_dialogue_projection(
            &complete_documents(),
            BuiltinDialogueProjection::MvInline(definition.compile().expect("MV 对话规则应能编译")),
        )
        .expect_err("未捕获非空 Speaker 的规则必须让整份快照失败");

        assert!(matches!(
            error,
            BuildBuiltinSnapshotError::Dialogue(MvDialogueProjectionError::RuleCapturedNoSpeaker {
                rule_number: 1
            })
        ));
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
            &BuiltinDialogueProjection::MzNative,
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
                project_definition_updates: Arc::new(Mutex::new(Vec::new())),
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
        assert_eq!(
            *service
                .snapshot_store
                .project_definition_updates
                .lock()
                .expect("项目定义更新记录锁不应中毒"),
            [BuiltinProjectDefinitionUpdate::Reuse]
        );
    }

    #[tokio::test]
    async fn mv_definition_intent_is_forwarded_with_the_built_snapshot() {
        let definition = crate::rpg_maker::dialogue::MvDialogueDefinition::parse_toml(
            r#"
                [[rule]]
                pattern = '(?<speaker>.+)'
            "#,
        )
        .expect("测试定义应合法");
        let updates = Arc::new(Mutex::new(Vec::new()));
        let service = BuiltInExtractionService::for_mv(
            FakeReader {
                response: Ok(complete_documents()),
                selections: Arc::new(Mutex::new(Vec::new())),
            },
            FakeStore {
                snapshots: Arc::new(Mutex::new(Vec::new())),
                project_definition_updates: Arc::clone(&updates),
                failure: None,
            },
            FakeCpu,
            BuiltInExtractionConfig::new(NonZeroUsize::new(2).expect("测试并发数必须非零")),
            MvDialogueDefinitionSelection::Replace {
                projector: definition.compile().expect("测试定义应能编译"),
                definition: definition.clone(),
            },
        );

        service.refresh(&project()).await.expect("快照应成功保存");

        assert_eq!(
            *updates.lock().expect("项目定义更新记录锁不应中毒"),
            [BuiltinProjectDefinitionUpdate::Replace(definition)]
        );
    }

    #[tokio::test]
    async fn mv_reuse_compiles_the_definition_from_the_opened_project_under_the_lease() {
        let snapshots = Arc::new(Mutex::new(Vec::new()));
        let updates = Arc::new(Mutex::new(Vec::new()));
        let service = BuiltInExtractionService::for_mv(
            FakeReader {
                response: Ok(complete_documents()),
                selections: Arc::new(Mutex::new(Vec::new())),
            },
            FakeStore {
                snapshots: Arc::clone(&snapshots),
                project_definition_updates: Arc::clone(&updates),
                failure: None,
            },
            FakeCpu,
            BuiltInExtractionConfig::new(NonZeroUsize::new(2).expect("测试并发数必须非零")),
            MvDialogueDefinitionSelection::ReuseProjectDefinition,
        );

        service
            .refresh(&project())
            .await
            .expect("项目中的显式空 MV 对话定义应可复用");

        let snapshots = snapshots.lock().expect("快照记录锁不应中毒");
        let dialogue = snapshots[0]
            .groups()
            .iter()
            .find(|group| {
                group.kind() == TextGroupKind::EventDialogue
                    && group
                        .group_location()
                        .to_string()
                        .starts_with("data/CommonEvents.json")
            })
            .expect("应提取公共事件对话");
        assert!(
            dialogue
                .fields()
                .iter()
                .all(|field| field.role() != &TextFieldRole::DialogueSpeaker),
            "MV 不得把 101.parameters[4] 当作原生姓名"
        );
        assert_eq!(field_text(dialogue, "body[0]"), "你好");
        assert_eq!(
            *updates.lock().expect("项目定义更新记录锁不应中毒"),
            [BuiltinProjectDefinitionUpdate::Reuse]
        );
    }

    #[tokio::test]
    async fn mv_zero_hit_replacement_stops_before_the_atomic_store() {
        let definition = crate::rpg_maker::dialogue::MvDialogueDefinition::parse_toml(
            "[[rule]]\npattern = '\\A(?<speaker>不会出现)\\z'\n",
        )
        .expect("测试定义应合法");
        let snapshots = Arc::new(Mutex::new(Vec::new()));
        let updates = Arc::new(Mutex::new(Vec::new()));
        let service = BuiltInExtractionService::for_mv(
            FakeReader {
                response: Ok(complete_documents()),
                selections: Arc::new(Mutex::new(Vec::new())),
            },
            FakeStore {
                snapshots: Arc::clone(&snapshots),
                project_definition_updates: Arc::clone(&updates),
                failure: None,
            },
            FakeCpu,
            BuiltInExtractionConfig::new(NonZeroUsize::new(2).expect("测试并发数必须非零")),
            MvDialogueDefinitionSelection::Replace {
                projector: definition.compile().expect("测试定义应能编译"),
                definition,
            },
        );

        let error = service
            .refresh(&project())
            .await
            .expect_err("零命中规则不得替换定义或 Builtin 快照");

        assert!(matches!(
            error,
            BuiltInExtractionError::ProjectDialogue(
                MvDialogueProjectionError::RuleCapturedNoSpeaker { rule_number: 1 }
            )
        ));
        assert!(snapshots.lock().expect("快照记录锁不应中毒").is_empty());
        assert!(updates.lock().expect("定义记录锁不应中毒").is_empty());
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
            RpgMakerDocumentId::Data(StandardDataFile::Items),
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
        response: Result<RpgMakerProjectDocuments, FakeError>,
        store_failure: Option<FakeError>,
    ) -> BuiltInExtractionService<FakeReader, FakeStore, FakeCpu> {
        BuiltInExtractionService::new(
            FakeReader {
                response,
                selections: Arc::new(Mutex::new(Vec::new())),
            },
            FakeStore {
                snapshots: Arc::new(Mutex::new(Vec::new())),
                project_definition_updates: Arc::new(Mutex::new(Vec::new())),
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
            crate::rpg_maker::project::test_layout_profile(),
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
    fn accepts_four_parameter_headers_and_keeps_blank_401_in_dialogue_recipe() {
        let mut documents = complete_documents();
        documents.insert_document(
            RpgMakerDocumentId::Data(StandardDataFile::CommonEvents),
            json!([null, {"list": [
                {"code": 101, "parameters": ["", 0, 0, 2]},
                {"code": 401, "parameters": ["   "]},
                {"code": 401, "parameters": ["正文"]},
                {"code": 0, "parameters": []}
            ]}]),
        );

        let snapshot = build_builtin_snapshot(&documents).expect("四参数消息头是合法 MZ 数据");
        let dialogue = snapshot
            .groups()
            .iter()
            .find(|group| {
                group.kind() == TextGroupKind::EventDialogue
                    && group
                        .group_location()
                        .to_string()
                        .starts_with("data/CommonEvents.json")
            })
            .expect("应保留对话组");

        assert!(
            dialogue
                .fields()
                .iter()
                .all(|field| field.role() != &TextFieldRole::DialogueSpeaker)
        );
        assert_eq!(field_text(dialogue, "body[0]"), "正文");
        let [TextProjectionRecipe::Dialogue(recipe)] = dialogue.recipes() else {
            panic!("Builtin 对话必须使用唯一块级配方");
        };
        assert_eq!(recipe.lines().len(), 2);
        assert!(matches!(
            recipe.lines()[0].parts(),
            [DialogueLinePart::Literal(value)] if value == "   "
        ));
    }

    #[test]
    fn keeps_blank_405_as_a_frozen_physical_recipe_without_a_logical_leaf() {
        let mut documents = complete_documents();
        documents.insert_document(
            RpgMakerDocumentId::Data(StandardDataFile::CommonEvents),
            json!([null, {"list": [
                {"code": 105, "parameters": [2, false]},
                {"code": 405, "parameters": ["第一行"]},
                {"code": 405, "parameters": ["   "]},
                {"code": 405, "parameters": ["第三行"]},
                {"code": 0, "parameters": []}
            ]}]),
        );

        let snapshot = build_builtin_snapshot(&documents).expect("空白滚动行应可逆提取");
        let scrolling = snapshot
            .groups()
            .iter()
            .find(|group| group.kind() == TextGroupKind::EventScrollingText)
            .expect("应保留滚动文本组");

        assert_eq!(field_text(scrolling, "body[0]"), "第一行");
        assert_eq!(field_text(scrolling, "body[2]"), "第三行");
        assert_eq!(scrolling.fields().len(), 2);
        assert_eq!(scrolling.recipes().len(), 3);
        assert_eq!(scrolling.mutation_targets().len(), 3);
        assert!(matches!(
            &scrolling.recipes()[1],
            TextProjectionRecipe::Direct(recipe)
                if recipe.expected_raw() == "   "
                    && matches!(recipe.parts(), [DirectTextPart::Literal(value)] if value == "   ")
        ));
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
                RpgMakerDocumentId::Data(StandardDataFile::CommonEvents),
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
                BuildBuiltinSnapshotError::Dialogue(_) => panic!("不应进入 MV 对话投影"),
            }
        }
    }

    #[test]
    fn rejects_wrong_fixed_field_type() {
        let mut documents = complete_documents();
        documents.insert_document(
            RpgMakerDocumentId::Data(StandardDataFile::Items),
            json!([null, {"name": 42, "description": "说明"}]),
        );

        let error = build_builtin_snapshot(&documents).expect_err("错误字段类型必须失败");

        match error {
            BuildBuiltinSnapshotError::Malformed(error) => {
                assert_eq!(error.location(), "data/Items.json[1].name");
                assert!(error.to_string().contains("必须是字符串"));
            }
            BuildBuiltinSnapshotError::Model(_) => panic!("应该是文档结构错误"),
            BuildBuiltinSnapshotError::Dialogue(_) => panic!("不应进入 MV 对话投影"),
        }
    }

    fn mv_dialogue_documents() -> RpgMakerProjectDocuments {
        let mut documents = complete_documents();
        documents.insert_document(
            RpgMakerDocumentId::Data(StandardDataFile::CommonEvents),
            json!([null, {"list": [
                {"code": 101, "parameters": ["", 0, 0, 2, "不应读取的原生姓名"]},
                {"code": 401, "parameters": ["\\n<莉莉:>你好"]},
                {"code": 401, "parameters": ["   "]},
                {"code": 401, "parameters": ["第二行"]},
                {"code": 0, "parameters": []}
            ]}]),
        );
        documents.insert_document(
            RpgMakerDocumentId::Map(1),
            json!({
                "displayName": "起始村庄",
                "events": [null, {"pages": [{"list": [
                    {"code": 101, "parameters": ["", 0, 0, 2, "不应读取的原生姓名"]},
                    {"code": 401, "parameters": ["バニー淫魔"]},
                    {"code": 401, "parameters": ["「台词」"]},
                    {"code": 0, "parameters": []}
                ]}]}]
            }),
        );
        documents
    }

    fn complete_documents() -> RpgMakerProjectDocuments {
        let mut documents = BTreeMap::new();
        documents.insert(
            RpgMakerDocumentId::Data(StandardDataFile::Actors),
            json!([null, {
                "name": "勇者",
                "nickname": "   ",
                "profile": " mixed 日本語 English "
            }]),
        );
        documents.insert(
            RpgMakerDocumentId::Data(StandardDataFile::Classes),
            json!([null, {"name": "战士"}]),
        );
        documents.insert(
            RpgMakerDocumentId::Data(StandardDataFile::Skills),
            json!([null, {
                "name": "斩击",
                "description": "攻击敌人",
                "message1": "发动斩击！",
                "message2": "",
            }]),
        );
        documents.insert(
            RpgMakerDocumentId::Data(StandardDataFile::Items),
            json!([null, {"name": "  宝剑  ", "description": "锋利的宝剑"}]),
        );
        documents.insert(
            RpgMakerDocumentId::Data(StandardDataFile::Weapons),
            json!([null, {"name": "木剑", "description": "练习用"}]),
        );
        documents.insert(
            RpgMakerDocumentId::Data(StandardDataFile::Armors),
            json!([null, {"name": "布衣", "description": "轻便"}]),
        );
        documents.insert(
            RpgMakerDocumentId::Data(StandardDataFile::Enemies),
            json!([null, {"name": "史莱姆"}]),
        );
        documents.insert(
            RpgMakerDocumentId::Data(StandardDataFile::States),
            json!([null, {
                "name": "中毒",
                "message1": "中了毒！",
                "message2": "中了毒！",
                "message3": "仍在中毒。",
                "message4": "毒消失了。"
            }]),
        );
        documents.insert(
            RpgMakerDocumentId::Data(StandardDataFile::System),
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
            RpgMakerDocumentId::Data(StandardDataFile::CommonEvents),
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
            RpgMakerDocumentId::Data(StandardDataFile::Troops),
            json!([null, {"pages": [{"list": [
                {"code": 101, "parameters": ["", 0, 0, 2, "敌人"]},
                {"code": 401, "parameters": ["受死吧！"]},
                {"code": 0, "parameters": []}
            ]}]}]),
        );
        documents.insert(
            RpgMakerDocumentId::Map(1),
            json!({
                "displayName": "起始村庄",
                "events": [null, {"pages": [{"list": [
                    {"code": 101, "parameters": ["", 0, 0, 2, "村民"]},
                    {"code": 401, "parameters": ["欢迎来到村庄。"]},
                    {"code": 0, "parameters": []}
                ]}]}]
            }),
        );
        RpgMakerProjectDocuments::new(documents, Vec::new())
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
