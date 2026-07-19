//! 把 Standard Mutation Plan 应用到冻结 RPG Maker 文档的候选生成服务。

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value};

use super::standard::{
    EventBodyMutationAction, ReplaceDialogueMutation, ReplaceEventBodyMutation,
    RpgMakerWriteBackDocumentRewriter, SetTextMutation, StandardWriteBackMutation,
    StandardWriteBackMutationPlan,
};
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::rpg_maker::ProjectName;
use crate::rpg_maker::extract::document::{
    RpgMakerDocumentId, RpgMakerDocumentSelection, RpgMakerProjectDocumentReader,
    RpgMakerProjectDocuments,
};
use crate::rpg_maker::model::DialogueLinePart;
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::tag::simple_tag_spans;
use crate::rpg_maker::text::{RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource};

/// 一个已经完成安全相对路径校验的完整文件替换。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerRewrittenFile {
    relative_path: PathBuf,
    bytes: Vec<u8>,
}

impl RpgMakerRewrittenFile {
    pub(super) fn new(
        relative_path: PathBuf,
        bytes: Vec<u8>,
    ) -> Result<Self, RpgMakerWriteBackDocumentRewriteFailure> {
        validate_relative_output_path(&relative_path)?;
        Ok(Self {
            relative_path,
            bytes,
        })
    }

    #[cfg(test)]
    pub(crate) fn relative_path(&self) -> &Path {
        &self.relative_path
    }

    #[cfg(test)]
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn into_parts(self) -> (PathBuf, Vec<u8>) {
        (self.relative_path, self.bytes)
    }
}

/// Rewriter 为一个确定项目生成的全部文件覆盖候选。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerRewrittenDocuments {
    project_name: ProjectName,
    workspace_root: PathBuf,
    files: Vec<RpgMakerRewrittenFile>,
}

impl RpgMakerRewrittenDocuments {
    pub(super) fn new(
        project_name: ProjectName,
        workspace_root: PathBuf,
        mut files: Vec<RpgMakerRewrittenFile>,
    ) -> Result<Self, RpgMakerWriteBackDocumentRewriteFailure> {
        files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        for pair in files.windows(2) {
            if pair[0].relative_path == pair[1].relative_path {
                return Err(
                    RpgMakerWriteBackDocumentRewriteFailure::DuplicateOutputPath {
                        path: pair[0].relative_path.clone(),
                    },
                );
            }
        }
        Ok(Self {
            project_name,
            workspace_root,
            files,
        })
    }

    fn empty(project: &OpenedProject) -> Self {
        Self {
            project_name: project.name().clone(),
            workspace_root: project.workspace_root().to_path_buf(),
            files: Vec::new(),
        }
    }

    pub(crate) fn project_name(&self) -> &ProjectName {
        &self.project_name
    }

    pub(crate) fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    #[cfg(test)]
    pub(crate) fn files(&self) -> &[RpgMakerRewrittenFile] {
        &self.files
    }

    pub(crate) fn into_files(self) -> Vec<RpgMakerRewrittenFile> {
        self.files
    }
}

fn validate_relative_output_path(
    path: &Path,
) -> Result<(), RpgMakerWriteBackDocumentRewriteFailure> {
    let mut components = path.components();
    let Some(Component::Normal(root)) = components.next() else {
        return Err(RpgMakerWriteBackDocumentRewriteFailure::InvalidOutputPath {
            path: path.to_path_buf(),
        });
    };
    if root != "data" && root != "js" {
        return Err(RpgMakerWriteBackDocumentRewriteFailure::InvalidOutputPath {
            path: path.to_path_buf(),
        });
    }
    let mut has_leaf = false;
    for component in components {
        if !matches!(component, Component::Normal(_)) {
            return Err(RpgMakerWriteBackDocumentRewriteFailure::InvalidOutputPath {
                path: path.to_path_buf(),
            });
        }
        has_leaf = true;
    }
    if !has_leaf {
        return Err(RpgMakerWriteBackDocumentRewriteFailure::InvalidOutputPath {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

/// 通过共享文档读取能力和 CPU 根生成完整文件候选。
pub(crate) struct RpgMakerWriteBackDocumentRewritingService<R, C> {
    document_reader: R,
    cpu_executor: C,
}

impl<R, C> RpgMakerWriteBackDocumentRewritingService<R, C> {
    pub(crate) const fn new(document_reader: R, cpu_executor: C) -> Self {
        Self {
            document_reader,
            cpu_executor,
        }
    }
}

impl<R, C> RpgMakerWriteBackDocumentRewriter for RpgMakerWriteBackDocumentRewritingService<R, C>
where
    R: RpgMakerProjectDocumentReader,
    C: CpuTaskExecutor,
{
    type RewrittenDocuments = RpgMakerRewrittenDocuments;
    type Error = RpgMakerWriteBackDocumentRewritingError<R::Error, C::Error>;

    async fn rewrite(
        &self,
        project: &OpenedProject,
        plan: StandardWriteBackMutationPlan,
    ) -> Result<Self::RewrittenDocuments, Self::Error> {
        if plan.mutations().is_empty() {
            return Ok(RpgMakerRewrittenDocuments::empty(project));
        }

        let selection = selection_for_plan(&plan);
        let documents = self
            .document_reader
            .read(project, selection)
            .await
            .map_err(RpgMakerWriteBackDocumentRewritingError::ReadDocuments)?;
        let project_name = project.name().clone();
        let workspace_root = project.workspace_root().to_path_buf();
        let rewritten = self
            .cpu_executor
            .execute(move || rewrite_documents(project_name, workspace_root, documents, plan))
            .await
            .map_err(RpgMakerWriteBackDocumentRewritingError::ScheduleRewrite)?;
        rewritten.map_err(RpgMakerWriteBackDocumentRewritingError::Rewrite)
    }
}

fn selection_for_plan(plan: &StandardWriteBackMutationPlan) -> RpgMakerDocumentSelection {
    let mut selection = RpgMakerDocumentSelection::empty();
    for mutation in plan.mutations() {
        match mutation {
            StandardWriteBackMutation::SetText(mutation) => {
                select_source(&mut selection, mutation.exact_location().source());
            }
            StandardWriteBackMutation::ReplaceDialogue(mutation) => {
                select_source(&mut selection, mutation.group_location().source());
                if let Some(speaker) = mutation.recipe().direct_speaker() {
                    select_source(&mut selection, speaker.physical_location().source());
                }
                for line in mutation.recipe().lines() {
                    select_source(&mut selection, line.physical_location().source());
                }
            }
            StandardWriteBackMutation::ReplaceEventBody(mutation) => {
                select_source(&mut selection, mutation.group_location().source());
                for segment in mutation.segments() {
                    select_source(&mut selection, segment.exact_location().source());
                }
            }
        }
    }
    selection
}

fn select_source(selection: &mut RpgMakerDocumentSelection, source: &RpgMakerSource) {
    match source {
        RpgMakerSource::Data(file) => selection.insert_standard_file(*file),
        RpgMakerSource::DataFile(file) => selection.insert_data_file(file.clone()),
        RpgMakerSource::Map(map_id) => selection.insert_map(*map_id),
        RpgMakerSource::PluginParameter { .. } => selection.request_plugins(),
    }
}

struct MutableDocuments {
    documents: BTreeMap<RpgMakerDocumentId, Value>,
    plugins: Vec<(usize, Map<String, Value>)>,
    changed_documents: BTreeSet<RpgMakerDocumentId>,
    plugins_changed: bool,
}

impl MutableDocuments {
    fn new(documents: RpgMakerProjectDocuments) -> Self {
        let (documents, plugins) = documents.into_parts();
        Self {
            documents,
            plugins: plugins
                .into_iter()
                .map(|configuration| configuration.into_parts())
                .collect(),
            changed_documents: BTreeSet::new(),
            plugins_changed: false,
        }
    }

    fn document_mut(
        &mut self,
        source: &RpgMakerSource,
        location: &RpgMakerLocation,
    ) -> Result<(&mut Value, RpgMakerDocumentId), RpgMakerWriteBackDocumentRewriteFailure> {
        let id = document_id(source).ok_or_else(|| {
            mutation_failure(location, "插件参数位置不能作为 RPG Maker JSON 文档地址")
        })?;
        let document = self
            .documents
            .get_mut(&id)
            .ok_or_else(|| mutation_failure(location, "文档读取器没有返回 Mutation 请求的文档"))?;
        Ok((document, id))
    }

    fn mark_document_changed(&mut self, id: RpgMakerDocumentId) {
        self.changed_documents.insert(id);
    }
}

fn document_id(source: &RpgMakerSource) -> Option<RpgMakerDocumentId> {
    match source {
        RpgMakerSource::Data(file) => Some(RpgMakerDocumentId::Data(*file)),
        RpgMakerSource::DataFile(file) => Some(RpgMakerDocumentId::DataFile(file.clone())),
        RpgMakerSource::Map(map_id) => Some(RpgMakerDocumentId::Map(*map_id)),
        RpgMakerSource::PluginParameter { .. } => None,
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ContainerKey {
    source: RpgMakerSource,
    steps: Vec<RpgMakerLocationStep>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StructuralKey {
    source: RpgMakerSource,
    list_steps: Vec<RpgMakerLocationStep>,
    start_index: usize,
}

enum StructuralOperation<'a> {
    Comment {
        key: StructuralKey,
        mutations: Vec<&'a SetTextMutation>,
    },
    Event {
        key: StructuralKey,
        mutation: &'a ReplaceEventBodyMutation,
    },
    Dialogue {
        key: StructuralKey,
        mutation: &'a ReplaceDialogueMutation,
    },
}

impl StructuralOperation<'_> {
    fn key(&self) -> &StructuralKey {
        match self {
            Self::Comment { key, .. } | Self::Event { key, .. } | Self::Dialogue { key, .. } => key,
        }
    }
}

fn rewrite_documents(
    project_name: ProjectName,
    workspace_root: PathBuf,
    documents: RpgMakerProjectDocuments,
    plan: StandardWriteBackMutationPlan,
) -> Result<RpgMakerRewrittenDocuments, RpgMakerWriteBackDocumentRewriteFailure> {
    let mut documents = MutableDocuments::new(documents);
    let mut note_groups = BTreeMap::<ContainerKey, Vec<&SetTextMutation>>::new();
    let mut comment_groups = BTreeMap::<ContainerKey, Vec<&SetTextMutation>>::new();
    let mut event_mutations = Vec::new();
    let mut dialogue_mutations = Vec::new();

    for mutation in plan.mutations() {
        match mutation {
            StandardWriteBackMutation::SetText(mutation) => match mutation.exact_location() {
                RpgMakerLocation::Value { .. } => apply_value_mutation(&mut documents, mutation)?,
                RpgMakerLocation::NoteTag {
                    source,
                    container_steps,
                    ..
                } => note_groups
                    .entry(ContainerKey {
                        source: source.clone(),
                        steps: container_steps.clone(),
                    })
                    .or_default()
                    .push(mutation),
                RpgMakerLocation::CommentTag {
                    source,
                    command_steps,
                    ..
                } => comment_groups
                    .entry(ContainerKey {
                        source: source.clone(),
                        steps: command_steps.clone(),
                    })
                    .or_default()
                    .push(mutation),
            },
            StandardWriteBackMutation::ReplaceDialogue(mutation) => {
                dialogue_mutations.push(mutation)
            }
            StandardWriteBackMutation::ReplaceEventBody(mutation) => event_mutations.push(mutation),
        }
    }

    for (key, mutations) in note_groups {
        apply_note_mutations(&mut documents, &key, mutations)?;
    }

    let mut structural = Vec::new();
    for (key, mutations) in comment_groups {
        let structural_key =
            structural_key(&key.source, &key.steps, mutations[0].exact_location())?;
        structural.push(StructuralOperation::Comment {
            key: structural_key,
            mutations,
        });
    }
    for mutation in event_mutations {
        let key = event_structural_key(mutation)?;
        structural.push(StructuralOperation::Event { key, mutation });
    }
    for mutation in dialogue_mutations {
        let key = dialogue_structural_key(mutation)?;
        structural.push(StructuralOperation::Dialogue { key, mutation });
    }
    structural.sort_by(compare_structural_operations);
    for pair in structural.windows(2) {
        if pair[0].key() == pair[1].key() {
            return Err(mutation_failure(
                structural_operation_location(&pair[0]),
                "两个结构修改指向同一冻结事件命令",
            ));
        }
    }
    for operation in structural {
        match operation {
            StructuralOperation::Comment { key, mutations } => {
                apply_comment_mutations(&mut documents, &key, mutations)?;
            }
            StructuralOperation::Event { key, mutation } => {
                apply_event_body_mutation(&mut documents, &key, mutation)?;
            }
            StructuralOperation::Dialogue { key, mutation } => {
                apply_dialogue_mutation(&mut documents, &key, mutation)?;
            }
        }
    }

    serialize_rewritten_documents(project_name, workspace_root, documents)
}

fn structural_operation_location<'a>(
    operation: &'a StructuralOperation<'a>,
) -> &'a RpgMakerLocation {
    match operation {
        StructuralOperation::Comment { mutations, .. } => mutations[0].exact_location(),
        StructuralOperation::Event { mutation, .. } => mutation.group_location(),
        StructuralOperation::Dialogue { mutation, .. } => mutation.group_location(),
    }
}

fn compare_structural_operations(
    left: &StructuralOperation<'_>,
    right: &StructuralOperation<'_>,
) -> Ordering {
    let left = left.key();
    let right = right.key();
    left.source
        .cmp(&right.source)
        .then_with(|| left.list_steps.cmp(&right.list_steps))
        .then_with(|| right.start_index.cmp(&left.start_index))
}

fn apply_value_mutation(
    documents: &mut MutableDocuments,
    mutation: &SetTextMutation,
) -> Result<(), RpgMakerWriteBackDocumentRewriteFailure> {
    let RpgMakerLocation::Value { source, steps } = mutation.exact_location() else {
        return Err(mutation_failure(
            mutation.exact_location(),
            "普通文本 Mutation 使用了非 Value 位置",
        ));
    };
    match source {
        RpgMakerSource::Data(_) | RpgMakerSource::DataFile(_) | RpgMakerSource::Map(_) => {
            let (document, id) = documents.document_mut(source, mutation.exact_location())?;
            replace_string_at(
                document,
                steps,
                mutation.expected_original(),
                mutation.replacement(),
                mutation.exact_location(),
            )?;
            documents.mark_document_changed(id);
        }
        RpgMakerSource::PluginParameter {
            plugin_index,
            plugin_name,
            parameter_name,
        } => {
            let parameter = plugin_parameter_mut(
                &mut documents.plugins,
                *plugin_index,
                plugin_name,
                parameter_name,
                mutation.exact_location(),
            )?;
            replace_string_at(
                parameter,
                steps,
                mutation.expected_original(),
                mutation.replacement(),
                mutation.exact_location(),
            )?;
            documents.plugins_changed = true;
        }
    }
    Ok(())
}

fn plugin_parameter_mut<'a>(
    plugins: &'a mut [(usize, Map<String, Value>)],
    plugin_index: usize,
    plugin_name: &str,
    parameter_name: &str,
    location: &RpgMakerLocation,
) -> Result<&'a mut Value, RpgMakerWriteBackDocumentRewriteFailure> {
    let Some((stored_index, fields)) = plugins.get_mut(plugin_index) else {
        return Err(mutation_failure(
            location,
            "plugins.js 中不存在指定插件索引",
        ));
    };
    if *stored_index != plugin_index {
        return Err(mutation_failure(location, "插件记录索引与数组位置不一致"));
    }
    let actual_name = fields.get("name").and_then(Value::as_str);
    if actual_name != Some(plugin_name) {
        return Err(mutation_failure(
            location,
            "插件索引处的 name 与结构化位置不一致",
        ));
    }
    let parameters = fields
        .get_mut("parameters")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| mutation_failure(location, "插件记录的 parameters 不是对象"))?;
    parameters
        .get_mut(parameter_name)
        .ok_or_else(|| mutation_failure(location, "插件参数名在指定插件记录中不存在"))
}

fn replace_string_at(
    value: &mut Value,
    steps: &[RpgMakerLocationStep],
    expected: &str,
    replacement: &str,
    location: &RpgMakerLocation,
) -> Result<(), RpgMakerWriteBackDocumentRewriteFailure> {
    let Some((step, remaining)) = steps.split_first() else {
        let actual = value
            .as_str()
            .ok_or_else(|| mutation_failure(location, "目标值不是字符串"))?;
        if actual != expected {
            return Err(mutation_failure(
                location,
                "目标字符串与 expected_original 不一致",
            ));
        }
        *value = Value::String(replacement.to_owned());
        return Ok(());
    };

    match step {
        RpgMakerLocationStep::ObjectKey(key) => {
            let child = value
                .as_object_mut()
                .and_then(|object| object.get_mut(key))
                .ok_or_else(|| mutation_failure(location, "对象路径不存在或父值不是对象"))?;
            replace_string_at(child, remaining, expected, replacement, location)
        }
        RpgMakerLocationStep::ArrayIndex(index) => {
            let child = value
                .as_array_mut()
                .and_then(|array| array.get_mut(*index))
                .ok_or_else(|| mutation_failure(location, "数组路径越界或父值不是数组"))?;
            replace_string_at(child, remaining, expected, replacement, location)
        }
        RpgMakerLocationStep::DecodeJsonString => {
            let raw = value
                .as_str()
                .ok_or_else(|| mutation_failure(location, "DecodeJsonString 的目标不是字符串"))?;
            let mut decoded = serde_json::from_str::<Value>(raw).map_err(|source| {
                RpgMakerWriteBackDocumentRewriteFailure::DecodeNestedJson {
                    location: Box::new(location.clone()),
                    source,
                }
            })?;
            replace_string_at(&mut decoded, remaining, expected, replacement, location)?;
            let encoded = serde_json::to_string(&decoded).map_err(|source| {
                RpgMakerWriteBackDocumentRewriteFailure::EncodeNestedJson {
                    location: Box::new(location.clone()),
                    source,
                }
            })?;
            *value = Value::String(encoded);
            Ok(())
        }
    }
}

fn apply_note_mutations(
    documents: &mut MutableDocuments,
    key: &ContainerKey,
    mutations: Vec<&SetTextMutation>,
) -> Result<(), RpgMakerWriteBackDocumentRewriteFailure> {
    let representative = mutations[0].exact_location();
    let (document, id) = documents.document_mut(&key.source, representative)?;
    let container = value_at_structural_steps_mut(document, &key.steps, representative)?;
    let note = container
        .as_object_mut()
        .and_then(|object| object.get_mut("note"))
        .and_then(|value| value.as_str())
        .ok_or_else(|| mutation_failure(representative, "NoteTag 容器没有字符串 note 字段"))?
        .to_owned();
    let replaced = replace_tag_values(&note, &mutations, TagKind::Note)?;
    container
        .as_object_mut()
        .expect("上方已确认 NoteTag 容器是对象")
        .insert("note".to_owned(), Value::String(replaced));
    documents.mark_document_changed(id);
    Ok(())
}

#[derive(Clone, Copy)]
enum TagKind {
    Note,
    Comment,
}

fn replace_tag_values(
    original: &str,
    mutations: &[&SetTextMutation],
    kind: TagKind,
) -> Result<String, RpgMakerWriteBackDocumentRewriteFailure> {
    let spans = simple_tag_spans(original);
    let mut replacements = Vec::with_capacity(mutations.len());
    for mutation in mutations {
        let (tag_name, occurrence) = match (kind, mutation.exact_location()) {
            (
                TagKind::Note,
                RpgMakerLocation::NoteTag {
                    tag_name,
                    occurrence,
                    ..
                },
            )
            | (
                TagKind::Comment,
                RpgMakerLocation::CommentTag {
                    tag_name,
                    occurrence,
                    ..
                },
            ) => (tag_name.as_str(), *occurrence),
            _ => {
                return Err(mutation_failure(
                    mutation.exact_location(),
                    "标签 Mutation 的位置类型与容器不一致",
                ));
            }
        };
        let span = spans
            .iter()
            .find(|span| span.name() == tag_name && span.occurrence() == occurrence)
            .ok_or_else(|| {
                mutation_failure(mutation.exact_location(), "找不到指定标签 occurrence")
            })?;
        if span.value() != mutation.expected_original() {
            return Err(mutation_failure(
                mutation.exact_location(),
                "标签值与 expected_original 不一致",
            ));
        }
        replacements.push((
            span.value_range(),
            mutation.replacement(),
            mutation.exact_location(),
        ));
    }
    replacements.sort_by_key(|replacement| Reverse(replacement.0.start));
    for pair in replacements.windows(2) {
        if pair[0].0.start < pair[1].0.end {
            return Err(mutation_failure(pair[0].2, "标签值替换范围发生重叠"));
        }
    }
    let mut result = original.to_owned();
    for (range, replacement, _) in replacements {
        result.replace_range(range, replacement);
    }
    Ok(result)
}

fn structural_key(
    source: &RpgMakerSource,
    steps: &[RpgMakerLocationStep],
    location: &RpgMakerLocation,
) -> Result<StructuralKey, RpgMakerWriteBackDocumentRewriteFailure> {
    let Some((last, list_steps)) = steps.split_last() else {
        return Err(mutation_failure(location, "事件命令位置缺少数组索引"));
    };
    let RpgMakerLocationStep::ArrayIndex(start_index) = last else {
        return Err(mutation_failure(location, "事件命令位置末步不是数组索引"));
    };
    Ok(StructuralKey {
        source: source.clone(),
        list_steps: list_steps.to_vec(),
        start_index: *start_index,
    })
}

fn event_structural_key(
    mutation: &ReplaceEventBodyMutation,
) -> Result<StructuralKey, RpgMakerWriteBackDocumentRewriteFailure> {
    let RpgMakerLocation::Value { source, steps } = mutation.group_location() else {
        return Err(mutation_failure(
            mutation.group_location(),
            "事件正文 group_location 不是 Value 位置",
        ));
    };
    structural_key(source, steps, mutation.group_location())
}

fn dialogue_structural_key(
    mutation: &ReplaceDialogueMutation,
) -> Result<StructuralKey, RpgMakerWriteBackDocumentRewriteFailure> {
    let RpgMakerLocation::Value { source, steps } = mutation.group_location() else {
        return Err(mutation_failure(
            mutation.group_location(),
            "对话 group_location 不是 Value 位置",
        ));
    };
    structural_key(source, steps, mutation.group_location())
}

fn apply_comment_mutations(
    documents: &mut MutableDocuments,
    key: &StructuralKey,
    mutations: Vec<&SetTextMutation>,
) -> Result<(), RpgMakerWriteBackDocumentRewriteFailure> {
    let representative = mutations[0].exact_location();
    let (document, id) = documents.document_mut(&key.source, representative)?;
    let list = event_list_mut(document, &key.list_steps, representative)?;
    let end = comment_block_end(list, key.start_index, representative)?;
    let originals = list[key.start_index..end].to_vec();
    let mut lines = Vec::with_capacity(originals.len());
    for command in &originals {
        lines.push(command_text(command, 108, 408, representative)?.to_owned());
    }
    let replaced = replace_tag_values(&lines.join("\n"), &mutations, TagKind::Comment)?;
    let rebuilt_lines: Vec<_> = replaced.split('\n').collect();
    let mut rebuilt = Vec::with_capacity(rebuilt_lines.len());
    for (index, line) in rebuilt_lines.into_iter().enumerate() {
        let template = originals
            .get(index)
            .unwrap_or_else(|| originals.last().expect("注释块至少有一条命令"));
        rebuilt.push(rewrite_command(
            template,
            if index == 0 { 108 } else { 408 },
            line,
            representative,
        )?);
    }
    list.splice(key.start_index..end, rebuilt);
    documents.mark_document_changed(id);
    Ok(())
}

fn comment_block_end(
    list: &[Value],
    start: usize,
    location: &RpgMakerLocation,
) -> Result<usize, RpgMakerWriteBackDocumentRewriteFailure> {
    let first = list
        .get(start)
        .ok_or_else(|| mutation_failure(location, "CommentTag 起始命令索引越界"))?;
    if command_code(first, location)? != 108 {
        return Err(mutation_failure(location, "CommentTag 起始命令不是 108"));
    }
    command_text(first, 108, 408, location)?;
    let mut end = start + 1;
    while let Some(command) = list.get(end) {
        if command_code(command, location)? != 408 {
            break;
        }
        command_text(command, 108, 408, location)?;
        end += 1;
    }
    Ok(end)
}

fn apply_event_body_mutation(
    documents: &mut MutableDocuments,
    key: &StructuralKey,
    mutation: &ReplaceEventBodyMutation,
) -> Result<(), RpgMakerWriteBackDocumentRewriteFailure> {
    let location = mutation.group_location();
    let (document, id) = documents.document_mut(&key.source, location)?;
    let list = event_list_mut(document, &key.list_steps, location)?;
    let (header_code, body_code) = (105, 405);
    let header = list
        .get(key.start_index)
        .ok_or_else(|| mutation_failure(location, "事件正文起始命令索引越界"))?;
    if command_code(header, location)? != header_code {
        return Err(mutation_failure(
            location,
            "事件正文起始命令码与正文类型不一致",
        ));
    }

    let body_start = key.start_index + 1;
    let body_end = body_start + mutation.segments().len();
    if body_end > list.len() {
        return Err(mutation_failure(location, "事件正文段数超过冻结命令列表"));
    }
    if let Some(command) = list.get(body_end)
        && command_code(command, location)? == body_code
    {
        return Err(mutation_failure(
            location,
            "Mutation 没有覆盖完整冻结事件正文块",
        ));
    }

    let originals = list[body_start..body_end].to_vec();
    let mut rebuilt = Vec::new();
    for (offset, (segment, command)) in mutation.segments().iter().zip(originals.iter()).enumerate()
    {
        validate_event_segment_location(
            segment.exact_location(),
            &key.source,
            &key.list_steps,
            body_start + offset,
        )?;
        if command_code(command, segment.exact_location())? != body_code {
            return Err(mutation_failure(
                segment.exact_location(),
                "冻结正文命令码与 Mutation 类型不一致",
            ));
        }
        let original = command_text(command, body_code, body_code, segment.exact_location())?;
        if original != segment.expected_original() {
            return Err(mutation_failure(
                segment.exact_location(),
                "冻结正文与 expected_original 不一致",
            ));
        }
        match segment.action() {
            EventBodyMutationAction::KeepOriginal => rebuilt.push(command.clone()),
            EventBodyMutationAction::ReplaceWithLines(lines) => {
                for line in lines {
                    rebuilt.push(rewrite_command(
                        command,
                        body_code,
                        line,
                        segment.exact_location(),
                    )?);
                }
            }
        }
    }
    list.splice(body_start..body_end, rebuilt);
    documents.mark_document_changed(id);
    Ok(())
}

fn apply_dialogue_mutation(
    documents: &mut MutableDocuments,
    key: &StructuralKey,
    mutation: &ReplaceDialogueMutation,
) -> Result<(), RpgMakerWriteBackDocumentRewriteFailure> {
    let location = mutation.group_location();
    let (document, id) = documents.document_mut(&key.source, location)?;
    let list = event_list_mut(document, &key.list_steps, location)?;
    let recipe = mutation.recipe();
    if recipe.group_location() != location {
        return Err(mutation_failure(
            location,
            "对话 Mutation 与物化配方的组位置不一致",
        ));
    }

    let header = list
        .get(key.start_index)
        .ok_or_else(|| mutation_failure(location, "对话起始命令索引越界"))?;
    if command_code(header, location)? != 101 {
        return Err(mutation_failure(location, "对话起始命令不是 101"));
    }

    let body_start = key.start_index + 1;
    let body_end = body_start + recipe.lines().len();
    if body_end > list.len() {
        return Err(mutation_failure(location, "对话配方段数超过冻结命令列表"));
    }
    if let Some(command) = list.get(body_end)
        && command_code(command, location)? == 401
    {
        return Err(mutation_failure(location, "对话配方没有覆盖完整 401 块"));
    }

    // 先在局部副本上验证并物化全部目标；任何一步失败都不会修改候选文档。
    let mut rebuilt_header = header.clone();
    if let Some(direct_speaker) = recipe.direct_speaker() {
        validate_dialogue_parameter_location(
            direct_speaker.physical_location(),
            &key.source,
            &key.list_steps,
            key.start_index,
            4,
        )?;
        let current = command_parameter_text(header, 4, direct_speaker.physical_location())?;
        if current != direct_speaker.expected_raw() {
            return Err(mutation_failure(
                direct_speaker.physical_location(),
                "冻结 Speaker 与 expected_raw 不一致",
            ));
        }
        set_command_parameter_text(
            &mut rebuilt_header,
            4,
            mutation
                .speaker()
                .expect("受信对话 Mutation 必须提供直接 Speaker"),
            direct_speaker.physical_location(),
        )?;
    }

    let originals = &list[body_start..body_end];
    let mut rebuilt_body = Vec::new();
    for (offset, (line_recipe, command)) in recipe.lines().iter().zip(originals.iter()).enumerate()
    {
        validate_dialogue_parameter_location(
            line_recipe.physical_location(),
            &key.source,
            &key.list_steps,
            body_start + offset,
            0,
        )?;
        if command_code(command, line_recipe.physical_location())? != 401 {
            return Err(mutation_failure(
                line_recipe.physical_location(),
                "冻结对话正文命令不是 401",
            ));
        }
        let current = command_text(command, 401, 401, line_recipe.physical_location())?;
        if current != line_recipe.expected_raw() {
            return Err(mutation_failure(
                line_recipe.physical_location(),
                "冻结对话正文与 expected_raw 不一致",
            ));
        }
        let rendered = render_dialogue_line(mutation, line_recipe.parts())?;
        for text in rendered {
            rebuilt_body.push(rewrite_command(
                command,
                401,
                &text,
                line_recipe.physical_location(),
            )?);
        }
    }

    list[key.start_index] = rebuilt_header;
    list.splice(body_start..body_end, rebuilt_body);
    documents.mark_document_changed(id);
    Ok(())
}

fn render_dialogue_line(
    mutation: &ReplaceDialogueMutation,
    parts: &[DialogueLinePart],
) -> Result<Vec<String>, RpgMakerWriteBackDocumentRewriteFailure> {
    let location = mutation.group_location();
    let body_position = parts
        .iter()
        .position(|part| matches!(part, DialogueLinePart::BodySlot { .. }));
    if body_position.is_some_and(|index| index + 1 != parts.len()) {
        return Err(mutation_failure(
            location,
            "对话 BodySlot 必须是物理行的最后一部分",
        ));
    }

    let mut prefix = String::new();
    for part in parts.iter().take(body_position.unwrap_or(parts.len())) {
        match part {
            DialogueLinePart::Literal(value) => prefix.push_str(value),
            DialogueLinePart::SpeakerSlot => prefix.push_str(
                mutation
                    .speaker()
                    .expect("受信对话 Mutation 必须提供内嵌 Speaker"),
            ),
            DialogueLinePart::BodySlot { .. } => unreachable!("已在 BodySlot 前停止"),
        }
    }

    let Some(body_position) = body_position else {
        return Ok(vec![prefix]);
    };
    let DialogueLinePart::BodySlot { index } = parts[body_position] else {
        unreachable!()
    };
    let lines = mutation
        .body_lines(index)
        .expect("受信对话 Mutation 必须覆盖 BodySlot");
    let mut rendered = Vec::with_capacity(lines.len());
    for (line_index, line) in lines.iter().enumerate() {
        if line_index == 0 {
            rendered.push(format!("{prefix}{line}"));
        } else {
            rendered.push(line.clone());
        }
    }
    Ok(rendered)
}

fn validate_dialogue_parameter_location(
    location: &RpgMakerLocation,
    source: &RpgMakerSource,
    list_steps: &[RpgMakerLocationStep],
    command_index: usize,
    parameter_index: usize,
) -> Result<(), RpgMakerWriteBackDocumentRewriteFailure> {
    let RpgMakerLocation::Value {
        source: target_source,
        steps,
    } = location
    else {
        return Err(mutation_failure(location, "对话参数位置不是 Value"));
    };
    let mut expected = list_steps.to_vec();
    expected.extend([
        RpgMakerLocationStep::index(command_index),
        RpgMakerLocationStep::key("parameters"),
        RpgMakerLocationStep::index(parameter_index),
    ]);
    if target_source != source || steps != &expected {
        return Err(mutation_failure(location, "对话参数位置不属于物化的冻结块"));
    }
    Ok(())
}

fn validate_event_segment_location(
    location: &RpgMakerLocation,
    source: &RpgMakerSource,
    list_steps: &[RpgMakerLocationStep],
    command_index: usize,
) -> Result<(), RpgMakerWriteBackDocumentRewriteFailure> {
    let RpgMakerLocation::Value {
        source: segment_source,
        steps,
    } = location
    else {
        return Err(mutation_failure(location, "事件正文段位置不是 Value"));
    };
    let mut expected = list_steps.to_vec();
    expected.extend([
        RpgMakerLocationStep::index(command_index),
        RpgMakerLocationStep::key("parameters"),
        RpgMakerLocationStep::index(0),
    ]);
    if segment_source != source || steps != &expected {
        return Err(mutation_failure(
            location,
            "事件正文段位置不属于指定冻结正文块",
        ));
    }
    Ok(())
}

fn event_list_mut<'a>(
    document: &'a mut Value,
    list_steps: &[RpgMakerLocationStep],
    location: &RpgMakerLocation,
) -> Result<&'a mut Vec<Value>, RpgMakerWriteBackDocumentRewriteFailure> {
    value_at_structural_steps_mut(document, list_steps, location)?
        .as_array_mut()
        .ok_or_else(|| mutation_failure(location, "事件 list 位置不是数组"))
}

fn value_at_structural_steps_mut<'a>(
    mut value: &'a mut Value,
    steps: &[RpgMakerLocationStep],
    location: &RpgMakerLocation,
) -> Result<&'a mut Value, RpgMakerWriteBackDocumentRewriteFailure> {
    for step in steps {
        value = match step {
            RpgMakerLocationStep::ObjectKey(key) => value
                .as_object_mut()
                .and_then(|object| object.get_mut(key))
                .ok_or_else(|| mutation_failure(location, "结构路径对象字段不存在"))?,
            RpgMakerLocationStep::ArrayIndex(index) => value
                .as_array_mut()
                .and_then(|array| array.get_mut(*index))
                .ok_or_else(|| mutation_failure(location, "结构路径数组索引越界"))?,
            RpgMakerLocationStep::DecodeJsonString => {
                return Err(mutation_failure(
                    location,
                    "事件或标签容器路径不能包含 DecodeJsonString",
                ));
            }
        };
    }
    Ok(value)
}

fn command_code(
    command: &Value,
    location: &RpgMakerLocation,
) -> Result<i64, RpgMakerWriteBackDocumentRewriteFailure> {
    command
        .as_object()
        .and_then(|object| object.get("code"))
        .and_then(Value::as_i64)
        .ok_or_else(|| mutation_failure(location, "事件命令不是带整数 code 的对象"))
}

fn command_text<'a>(
    command: &'a Value,
    first_code: i64,
    continuation_code: i64,
    location: &RpgMakerLocation,
) -> Result<&'a str, RpgMakerWriteBackDocumentRewriteFailure> {
    let code = command_code(command, location)?;
    if code != first_code && code != continuation_code {
        return Err(mutation_failure(location, "事件命令码不属于目标文本块"));
    }
    command
        .as_object()
        .and_then(|object| object.get("parameters"))
        .and_then(Value::as_array)
        .and_then(|parameters| parameters.first())
        .and_then(Value::as_str)
        .ok_or_else(|| mutation_failure(location, "事件文本命令缺少字符串 parameters[0]"))
}

fn command_parameter_text<'a>(
    command: &'a Value,
    parameter_index: usize,
    location: &RpgMakerLocation,
) -> Result<&'a str, RpgMakerWriteBackDocumentRewriteFailure> {
    command
        .as_object()
        .and_then(|object| object.get("parameters"))
        .and_then(Value::as_array)
        .and_then(|parameters| parameters.get(parameter_index))
        .and_then(Value::as_str)
        .ok_or_else(|| mutation_failure(location, "事件命令目标参数不是字符串"))
}

fn set_command_parameter_text(
    command: &mut Value,
    parameter_index: usize,
    text: &str,
    location: &RpgMakerLocation,
) -> Result<(), RpgMakerWriteBackDocumentRewriteFailure> {
    let parameter = command
        .as_object_mut()
        .and_then(|object| object.get_mut("parameters"))
        .and_then(Value::as_array_mut)
        .and_then(|parameters| parameters.get_mut(parameter_index))
        .ok_or_else(|| mutation_failure(location, "事件命令目标参数不存在"))?;
    if !parameter.is_string() {
        return Err(mutation_failure(location, "事件命令目标参数不是字符串"));
    }
    *parameter = Value::String(text.to_owned());
    Ok(())
}

fn rewrite_command(
    template: &Value,
    code: i64,
    text: &str,
    location: &RpgMakerLocation,
) -> Result<Value, RpgMakerWriteBackDocumentRewriteFailure> {
    let mut command = template.clone();
    let object = command
        .as_object_mut()
        .ok_or_else(|| mutation_failure(location, "事件命令模板不是对象"))?;
    object.insert("code".to_owned(), Value::from(code));
    let parameters = object
        .get_mut("parameters")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| mutation_failure(location, "事件命令模板缺少 parameters 数组"))?;
    let first = parameters
        .first_mut()
        .ok_or_else(|| mutation_failure(location, "事件命令模板缺少 parameters[0]"))?;
    *first = Value::String(text.to_owned());
    Ok(command)
}

fn serialize_rewritten_documents(
    project_name: ProjectName,
    workspace_root: PathBuf,
    mut documents: MutableDocuments,
) -> Result<RpgMakerRewrittenDocuments, RpgMakerWriteBackDocumentRewriteFailure> {
    let mut files = Vec::new();
    for id in documents.changed_documents {
        let value = documents.documents.remove(&id).ok_or_else(|| {
            RpgMakerWriteBackDocumentRewriteFailure::MissingChangedDocument { id: id.clone() }
        })?;
        let relative_path = relative_document_path(&id);
        let mut bytes = serde_json::to_vec_pretty(&value).map_err(|source| {
            RpgMakerWriteBackDocumentRewriteFailure::SerializeDocument {
                path: relative_path.clone(),
                source,
            }
        })?;
        bytes.push(b'\n');
        files.push(RpgMakerRewrittenFile::new(relative_path, bytes)?);
    }
    if documents.plugins_changed {
        let mut values = Vec::with_capacity(documents.plugins.len());
        for (expected_index, (stored_index, fields)) in documents.plugins.into_iter().enumerate() {
            if stored_index != expected_index {
                return Err(
                    RpgMakerWriteBackDocumentRewriteFailure::InvalidPluginOrder {
                        expected_index,
                        stored_index,
                    },
                );
            }
            values.push(Value::Object(fields));
        }
        let path = PathBuf::from("js").join("plugins.js");
        let json = serde_json::to_string_pretty(&values).map_err(|source| {
            RpgMakerWriteBackDocumentRewriteFailure::SerializeDocument {
                path: path.clone(),
                source,
            }
        })?;
        let mut bytes = Vec::with_capacity("var $plugins = ;\n".len() + json.len());
        bytes.extend_from_slice(b"var $plugins = ");
        bytes.extend_from_slice(json.as_bytes());
        bytes.extend_from_slice(b";\n");
        files.push(RpgMakerRewrittenFile::new(path, bytes)?);
    }
    RpgMakerRewrittenDocuments::new(project_name, workspace_root, files)
}

fn relative_document_path(id: &RpgMakerDocumentId) -> PathBuf {
    match id {
        RpgMakerDocumentId::Data(file) => PathBuf::from("data").join(file.file_name()),
        RpgMakerDocumentId::DataFile(file) => PathBuf::from("data").join(file.as_str()),
        RpgMakerDocumentId::Map(map_id) => {
            PathBuf::from("data").join(format!("Map{map_id:03}.json"))
        }
    }
}

fn mutation_failure(
    location: &RpgMakerLocation,
    message: impl Into<String>,
) -> RpgMakerWriteBackDocumentRewriteFailure {
    RpgMakerWriteBackDocumentRewriteFailure::InvalidMutation {
        location: Box::new(location.clone()),
        message: message.into(),
    }
}

/// 文档读取、CPU 调度或纯改写阶段的技术失败。
#[derive(Debug)]
pub(crate) enum RpgMakerWriteBackDocumentRewritingError<R, C> {
    ReadDocuments(R),
    ScheduleRewrite(CpuTaskExecutionError<C>),
    Rewrite(RpgMakerWriteBackDocumentRewriteFailure),
}

impl<R, C> fmt::Display for RpgMakerWriteBackDocumentRewritingError<R, C>
where
    R: fmt::Display,
    C: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadDocuments(source) => {
                write!(formatter, "读取冻结 RPG Maker 文档失败：{source}")
            }
            Self::ScheduleRewrite(source) => {
                write!(formatter, "调度 RPG Maker 文档改写失败：{source}")
            }
            Self::Rewrite(source) => write!(formatter, "改写 RPG Maker 文档失败：{source}"),
        }
    }
}

impl<R, C> Error for RpgMakerWriteBackDocumentRewritingError<R, C>
where
    R: Error + 'static,
    C: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadDocuments(source) => Some(source),
            Self::ScheduleRewrite(source) => Some(source),
            Self::Rewrite(source) => Some(source),
        }
    }
}

/// 已读取文档与 Mutation Plan 无法共同建立完整候选。
#[derive(Debug)]
pub(crate) enum RpgMakerWriteBackDocumentRewriteFailure {
    InvalidMutation {
        location: Box<RpgMakerLocation>,
        message: String,
    },
    DecodeNestedJson {
        location: Box<RpgMakerLocation>,
        source: serde_json::Error,
    },
    EncodeNestedJson {
        location: Box<RpgMakerLocation>,
        source: serde_json::Error,
    },
    SerializeDocument {
        path: PathBuf,
        source: serde_json::Error,
    },
    InvalidOutputPath {
        path: PathBuf,
    },
    DuplicateOutputPath {
        path: PathBuf,
    },
    MissingChangedDocument {
        id: RpgMakerDocumentId,
    },
    InvalidPluginOrder {
        expected_index: usize,
        stored_index: usize,
    },
}

impl fmt::Display for RpgMakerWriteBackDocumentRewriteFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMutation { location, message } => {
                write!(formatter, "Mutation {location} 无法应用：{message}")
            }
            Self::DecodeNestedJson { location, source } => {
                write!(
                    formatter,
                    "Mutation {location} 的嵌套 JSON 无法解码：{source}"
                )
            }
            Self::EncodeNestedJson { location, source } => {
                write!(
                    formatter,
                    "Mutation {location} 的嵌套 JSON 无法重新编码：{source}"
                )
            }
            Self::SerializeDocument { path, source } => {
                write!(formatter, "无法序列化候选文档 {}：{source}", path.display())
            }
            Self::InvalidOutputPath { path } => {
                write!(
                    formatter,
                    "候选文件路径不在 data/js 安全子树内：{}",
                    path.display()
                )
            }
            Self::DuplicateOutputPath { path } => {
                write!(formatter, "候选文件路径重复：{}", path.display())
            }
            Self::MissingChangedDocument { id } => {
                write!(formatter, "已修改文档在序列化前丢失：{id:?}")
            }
            Self::InvalidPluginOrder {
                expected_index,
                stored_index,
            } => write!(
                formatter,
                "插件记录顺序损坏：数组位置 {expected_index} 保存了索引 {stored_index}"
            ),
        }
    }
}

impl Error for RpgMakerWriteBackDocumentRewriteFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::DecodeNestedJson { source, .. }
            | Self::EncodeNestedJson { source, .. }
            | Self::SerializeDocument { source, .. } => Some(source),
            Self::InvalidMutation { .. }
            | Self::InvalidOutputPath { .. }
            | Self::DuplicateOutputPath { .. }
            | Self::MissingChangedDocument { .. }
            | Self::InvalidPluginOrder { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::rpg_maker::extract::document::PluginConfiguration;
    use crate::rpg_maker::model::{
        DialogueLinePart, DialogueLineRecipe, DialogueWriteRecipe, DirectSpeakerTarget,
    };
    use crate::rpg_maker::project::test_layout_profile;
    use crate::rpg_maker::text::StandardDataFile;
    use crate::rpg_maker::write_back::standard::{
        EventBodyMutationSegment, StandardWriteBackMutation,
    };

    #[derive(Clone, Copy, Debug)]
    struct FakeError;

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fake")
        }
    }

    impl Error for FakeError {}

    struct PanickingReader;

    impl RpgMakerProjectDocumentReader for PanickingReader {
        type Error = FakeError;

        async fn read(
            &self,
            _project: &OpenedProject,
            _selection: RpgMakerDocumentSelection,
        ) -> Result<RpgMakerProjectDocuments, Self::Error> {
            panic!("空 Mutation Plan 不得读取文档")
        }
    }

    struct FailingReader;

    impl RpgMakerProjectDocumentReader for FailingReader {
        type Error = FakeError;

        async fn read(
            &self,
            _project: &OpenedProject,
            _selection: RpgMakerDocumentSelection,
        ) -> Result<RpgMakerProjectDocuments, Self::Error> {
            Err(FakeError)
        }
    }

    struct StaticReader(RpgMakerProjectDocuments);

    impl RpgMakerProjectDocumentReader for StaticReader {
        type Error = FakeError;

        async fn read(
            &self,
            _project: &OpenedProject,
            _selection: RpgMakerDocumentSelection,
        ) -> Result<RpgMakerProjectDocuments, Self::Error> {
            Ok(self.0.clone())
        }
    }

    struct PanickingCpu;

    impl CpuTaskExecutor for PanickingCpu {
        type Error = FakeError;

        async fn execute<T, F>(&self, _task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            panic!("空 Mutation Plan 不得调度 CPU")
        }
    }

    struct UnavailableCpu;

    impl CpuTaskExecutor for UnavailableCpu {
        type Error = FakeError;

        async fn execute<T, F>(&self, _task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            Err(CpuTaskExecutionError::Unavailable(FakeError))
        }
    }

    #[tokio::test]
    async fn empty_plan_returns_project_bound_empty_candidate_without_dependencies() {
        let project = project();
        let service = RpgMakerWriteBackDocumentRewritingService::new(PanickingReader, PanickingCpu);

        let candidate = service
            .rewrite(&project, StandardWriteBackMutationPlan::empty())
            .await
            .expect("空计划应该直接成功");

        assert_eq!(candidate.project_name(), project.name());
        assert_eq!(candidate.workspace_root(), project.workspace_root());
        assert!(candidate.files().is_empty());
    }

    #[tokio::test]
    async fn reader_and_cpu_failures_keep_their_service_stages() {
        let source = RpgMakerSource::data(StandardDataFile::Items);
        let location = RpgMakerLocation::value(
            source,
            vec![
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("name"),
            ],
        );
        let mutation_plan = || plan(vec![set_text(location.clone(), "原文", "译文")]);
        let project = project();

        let read_error =
            RpgMakerWriteBackDocumentRewritingService::new(FailingReader, PanickingCpu)
                .rewrite(&project, mutation_plan())
                .await
                .expect_err("文档读取失败必须保留阶段");
        assert!(matches!(
            read_error,
            RpgMakerWriteBackDocumentRewritingError::ReadDocuments(FakeError)
        ));

        let documents = RpgMakerProjectDocuments::new(
            BTreeMap::from([(
                RpgMakerDocumentId::Data(StandardDataFile::Items),
                json!([null, {"name": "原文"}]),
            )]),
            Vec::new(),
        );
        let cpu_error =
            RpgMakerWriteBackDocumentRewritingService::new(StaticReader(documents), UnavailableCpu)
                .rewrite(&project, mutation_plan())
                .await
                .expect_err("CPU 不可用必须保留调度阶段");
        assert!(matches!(
            cpu_error,
            RpgMakerWriteBackDocumentRewritingError::ScheduleRewrite(
                CpuTaskExecutionError::Unavailable(FakeError)
            )
        ));
    }

    #[test]
    fn selection_uses_exact_map_ids_and_plugin_identity_without_all_maps() {
        let map_source = RpgMakerSource::map(42);
        let plugin_source = RpgMakerSource::plugin_parameter(1, "Quest", "Config");
        let plan = plan(vec![
            set_text(
                RpgMakerLocation::value(map_source, vec![RpgMakerLocationStep::key("displayName")]),
                "旧地图",
                "新地图",
            ),
            set_text(
                RpgMakerLocation::value(plugin_source, Vec::new()),
                "旧参数",
                "新参数",
            ),
        ]);

        let selection = selection_for_plan(&plan);

        assert_eq!(selection.map_ids(), &BTreeSet::from([42]));
        assert!(!selection.includes_all_maps());
        assert!(selection.includes_plugins());
    }

    #[test]
    fn rewrites_plain_nested_note_and_plugin_values_with_complete_files() {
        let source = RpgMakerSource::data(StandardDataFile::Actors);
        let document = json!([
            null,
            {
                "name": "勇者",
                "note": "<Tag:旧一><Tag:旧二>",
                "nested": "{\"inner\":{\"text\":\"原文\",\"kept\":7}}",
                "unknown": {"kept": true}
            }
        ]);
        let plugin_fields = serde_json::from_value(json!({
            "name": "QuestMenu",
            "status": true,
            "description": "保留",
            "parameters": {
                "Config": "{\"entries\":[{\"title\":\"任务\",\"kept\":true}],\"deep\":\"{\\\"text\\\":\\\"深层原文\\\"}\"}"
            },
            "future": {"kept": true}
        }))
        .expect("插件记录应该是对象");
        let documents = RpgMakerProjectDocuments::new(
            BTreeMap::from([(RpgMakerDocumentId::Data(StandardDataFile::Actors), document)]),
            vec![PluginConfiguration::new(0, plugin_fields)],
        );
        let actor = |tail: Vec<RpgMakerLocationStep>| {
            let mut steps = vec![RpgMakerLocationStep::index(1)];
            steps.extend(tail);
            RpgMakerLocation::value(source.clone(), steps)
        };
        let plugin_source = RpgMakerSource::plugin_parameter(0, "QuestMenu", "Config");
        let plan = plan(vec![
            set_text(
                actor(vec![RpgMakerLocationStep::key("name")]),
                "勇者",
                "英雄",
            ),
            set_text(
                actor(vec![
                    RpgMakerLocationStep::key("nested"),
                    RpgMakerLocationStep::DecodeJsonString,
                    RpgMakerLocationStep::key("inner"),
                    RpgMakerLocationStep::key("text"),
                ]),
                "原文",
                "译文",
            ),
            set_text(
                RpgMakerLocation::note_tag(
                    source.clone(),
                    vec![RpgMakerLocationStep::index(1)],
                    "Tag",
                    0,
                ),
                "旧一",
                "新一",
            ),
            set_text(
                RpgMakerLocation::note_tag(source, vec![RpgMakerLocationStep::index(1)], "Tag", 1),
                "旧二",
                "新二",
            ),
            set_text(
                RpgMakerLocation::value(
                    plugin_source.clone(),
                    vec![
                        RpgMakerLocationStep::DecodeJsonString,
                        RpgMakerLocationStep::key("entries"),
                        RpgMakerLocationStep::index(0),
                        RpgMakerLocationStep::key("title"),
                    ],
                ),
                "任务",
                "委托",
            ),
            set_text(
                RpgMakerLocation::value(
                    plugin_source,
                    vec![
                        RpgMakerLocationStep::DecodeJsonString,
                        RpgMakerLocationStep::key("deep"),
                        RpgMakerLocationStep::DecodeJsonString,
                        RpgMakerLocationStep::key("text"),
                    ],
                ),
                "深层原文",
                "深层译文",
            ),
        ]);

        let candidate = rewrite_documents(project_name(), workspace_root(), documents, plan)
            .expect("完整合法计划应该成功");

        assert_eq!(candidate.files().len(), 2);
        let actor_text = file_text(&candidate, Path::new("data/Actors.json"));
        assert!(actor_text.ends_with('\n'));
        assert_appears_in_order(
            actor_text,
            &["\"name\":", "\"note\":", "\"nested\":", "\"unknown\":"],
        );
        let actor_json: Value = serde_json::from_str(actor_text).expect("Actors 候选应该是 JSON");
        assert_eq!(actor_json[1]["name"], "英雄");
        assert_eq!(actor_json[1]["note"], "<Tag:新一><Tag:新二>");
        assert_eq!(actor_json[1]["unknown"]["kept"], true);
        let nested: Value = serde_json::from_str(
            actor_json[1]["nested"]
                .as_str()
                .expect("nested 应保持 JSON 字符串"),
        )
        .expect("nested 应能重新解码");
        assert_eq!(nested["inner"]["text"], "译文");
        assert_eq!(nested["inner"]["kept"], 7);

        let plugins_text = file_text(&candidate, Path::new("js/plugins.js"));
        assert!(plugins_text.starts_with("var $plugins = ["));
        assert!(plugins_text.ends_with(";\n"));
        assert_appears_in_order(
            plugins_text,
            &[
                "\"name\":",
                "\"status\":",
                "\"description\":",
                "\"parameters\":",
                "\"future\":",
            ],
        );
        let plugin_json = plugins_text
            .strip_prefix("var $plugins = ")
            .and_then(|text| text.strip_suffix(";\n"))
            .expect("plugins.js 应使用规范外壳");
        let plugins: Value = serde_json::from_str(plugin_json).expect("插件候选应该是 JSON");
        assert_eq!(plugins[0]["description"], "保留");
        assert_eq!(plugins[0]["future"]["kept"], true);
        let config: Value = serde_json::from_str(
            plugins[0]["parameters"]["Config"]
                .as_str()
                .expect("Config 应保持字符串"),
        )
        .expect("Config 应能重新解码");
        assert_eq!(config["entries"][0]["title"], "委托");
        assert_eq!(config["entries"][0]["kept"], true);
        let deep: Value = serde_json::from_str(
            config["deep"]
                .as_str()
                .expect("deep 应保持嵌套 JSON 字符串"),
        )
        .expect("deep 应能再次解码");
        assert_eq!(deep["text"], "深层译文");
    }

    #[test]
    fn applies_structural_mutations_in_descending_frozen_index_order() {
        let source = RpgMakerSource::map(7);
        let list_steps = vec![RpgMakerLocationStep::key("list")];
        let document = json!({
            "list": [
                {"code":101,"indent":0,"parameters":["",0,0,2,"莉莉"],"headerUnknown":true},
                {"code":401,"indent":1,"parameters":["甲"],"lineUnknown":"A"},
                {"code":401,"indent":2,"parameters":["乙"],"lineUnknown":"B"},
                {"code":108,"indent":3,"parameters":["<Tag:旧"],"commentUnknown":"first"},
                {"code":408,"indent":4,"parameters":["值><Tag:二>"],"commentUnknown":"continued"},
                {"code":0,"indent":0,"parameters":[]}
            ],
            "unknown": {"kept": true}
        });
        let documents = RpgMakerProjectDocuments::new(
            BTreeMap::from([(RpgMakerDocumentId::Map(7), document)]),
            Vec::new(),
        );
        let comment_steps = [list_steps.clone(), vec![RpgMakerLocationStep::index(3)]].concat();
        let group_steps = [list_steps.clone(), vec![RpgMakerLocationStep::index(0)]].concat();
        let segment_location = |index| {
            let mut steps = list_steps.clone();
            steps.extend([
                RpgMakerLocationStep::index(index),
                RpgMakerLocationStep::key("parameters"),
                RpgMakerLocationStep::index(0),
            ]);
            RpgMakerLocation::value(source.clone(), steps)
        };
        let group_location = RpgMakerLocation::value(source.clone(), group_steps);
        let speaker_location = {
            let mut steps = list_steps.clone();
            steps.extend([
                RpgMakerLocationStep::index(0),
                RpgMakerLocationStep::key("parameters"),
                RpgMakerLocationStep::index(4),
            ]);
            RpgMakerLocation::value(source.clone(), steps)
        };
        let recipe = DialogueWriteRecipe::new(
            group_location,
            Some(DirectSpeakerTarget::new(speaker_location, "莉莉")),
            vec![
                DialogueLineRecipe::new(
                    segment_location(1),
                    "甲",
                    vec![DialogueLinePart::BodySlot { index: 0 }],
                )
                .expect("第一行配方应合法"),
                DialogueLineRecipe::new(
                    segment_location(2),
                    "乙",
                    vec![DialogueLinePart::BodySlot { index: 1 }],
                )
                .expect("第二行配方应合法"),
            ],
        )
        .expect("对话配方应合法");
        let body = ReplaceDialogueMutation::new(
            recipe,
            Some("莉莉译".to_owned()),
            BTreeMap::from([
                (0, vec!["甲一".to_owned(), "甲二".to_owned()]),
                (1, vec!["乙".to_owned()]),
            ]),
        )
        .expect("对话测试计划应该合法");
        let plan = plan(vec![
            set_text(
                RpgMakerLocation::comment_tag(source.clone(), comment_steps.clone(), "Tag", 0),
                "旧\n值",
                "新",
            ),
            set_text(
                RpgMakerLocation::comment_tag(source, comment_steps, "Tag", 1),
                "二",
                "第二",
            ),
            StandardWriteBackMutation::ReplaceDialogue(body),
        ]);

        let candidate = rewrite_documents(project_name(), workspace_root(), documents, plan)
            .expect("同一列表的降序结构修改应该成功");

        let map: Value = serde_json::from_str(file_text(&candidate, Path::new("data/Map007.json")))
            .expect("Map 候选应该是 JSON");
        let list = map["list"].as_array().expect("事件 list 应保持数组");
        assert_eq!(
            list.iter()
                .map(|command| command["code"].as_i64().unwrap())
                .collect::<Vec<_>>(),
            vec![101, 401, 401, 401, 108, 0]
        );
        assert_eq!(list[1]["parameters"][0], "甲一");
        assert_eq!(list[2]["parameters"][0], "甲二");
        assert_eq!(list[1]["indent"], 1);
        assert_eq!(list[2]["indent"], 1);
        assert_eq!(list[1]["lineUnknown"], "A");
        assert_eq!(list[2]["lineUnknown"], "A");
        assert_eq!(list[3]["parameters"][0], "乙");
        assert_eq!(list[3]["indent"], 2);
        assert_eq!(list[3]["lineUnknown"], "B");
        assert_eq!(list[0]["parameters"][4], "莉莉译");
        assert_eq!(list[4]["parameters"][0], "<Tag:新><Tag:第二>");
        assert_eq!(list[4]["commentUnknown"], "first");
        assert_eq!(map["unknown"]["kept"], true);
    }

    #[test]
    fn rebuilds_mv_inline_speaker_once_and_keeps_each_body_hard_boundary() {
        let source = RpgMakerSource::map(8);
        let list_steps = vec![RpgMakerLocationStep::key("list")];
        let group_location = RpgMakerLocation::value(
            source.clone(),
            [list_steps.clone(), vec![RpgMakerLocationStep::index(0)]].concat(),
        );
        let line_location = |command_index| {
            RpgMakerLocation::value(
                source.clone(),
                [
                    list_steps.clone(),
                    vec![
                        RpgMakerLocationStep::index(command_index),
                        RpgMakerLocationStep::key("parameters"),
                        RpgMakerLocationStep::index(0),
                    ],
                ]
                .concat(),
            )
        };
        let recipe = DialogueWriteRecipe::new(
            group_location,
            None,
            vec![
                DialogueLineRecipe::new(
                    line_location(1),
                    "\\C[18]\\n<Alice:>\\n<Alice:>Hello",
                    vec![
                        DialogueLinePart::Literal("\\C[18]\\n<".to_owned()),
                        DialogueLinePart::SpeakerSlot,
                        DialogueLinePart::Literal(":>\\n<".to_owned()),
                        DialogueLinePart::SpeakerSlot,
                        DialogueLinePart::Literal(":>".to_owned()),
                        DialogueLinePart::BodySlot { index: 0 },
                    ],
                )
                .expect("内嵌姓名行配方应合法"),
                DialogueLineRecipe::new(
                    line_location(2),
                    "Tail",
                    vec![DialogueLinePart::BodySlot { index: 1 }],
                )
                .expect("第二正文行配方应合法"),
            ],
        )
        .expect("MV 对话配方应合法");
        let mutation = ReplaceDialogueMutation::new(
            recipe,
            Some("爱丽丝".to_owned()),
            BTreeMap::from([
                (0, vec!["第一行".to_owned(), "第二行".to_owned()]),
                (1, vec!["尾行".to_owned()]),
            ]),
        )
        .expect("MV 对话 Mutation 应合法");
        let documents = RpgMakerProjectDocuments::new(
            BTreeMap::from([(
                RpgMakerDocumentId::Map(8),
                json!({
                    "list": [
                        {"code":101,"indent":0,"parameters":["",0,0,2],"headerUnknown":true},
                        {"code":401,"indent":1,"parameters":["\\C[18]\\n<Alice:>\\n<Alice:>Hello"],"lineUnknown":"first"},
                        {"code":401,"indent":2,"parameters":["Tail"],"lineUnknown":"tail"},
                        {"code":0,"indent":0,"parameters":[]}
                    ]
                }),
            )]),
            Vec::new(),
        );
        let candidate = rewrite_documents(
            project_name(),
            workspace_root(),
            documents,
            plan(vec![StandardWriteBackMutation::ReplaceDialogue(mutation)]),
        )
        .expect("MV inline 对话应能原子重建");
        let map: Value = serde_json::from_str(file_text(&candidate, Path::new("data/Map008.json")))
            .expect("候选 Map 应为 JSON");
        let list = map["list"].as_array().expect("事件列表应存在");
        assert_eq!(list.len(), 5);
        assert_eq!(list[0]["parameters"].as_array().unwrap().len(), 4);
        assert_eq!(
            list[1]["parameters"][0],
            "\\C[18]\\n<爱丽丝:>\\n<爱丽丝:>第一行"
        );
        assert_eq!(list[2]["parameters"][0], "第二行");
        assert_eq!(list[3]["parameters"][0], "尾行");
        assert_eq!(list[1]["lineUnknown"], "first");
        assert_eq!(list[2]["lineUnknown"], "first");
        assert_eq!(list[3]["lineUnknown"], "tail");
    }

    #[test]
    fn exact_first_line_speaker_and_block_drift_have_explicit_outcomes() {
        let source = RpgMakerSource::map(9);
        let list_steps = vec![RpgMakerLocationStep::key("list")];
        let group_location = RpgMakerLocation::value(
            source.clone(),
            [list_steps.clone(), vec![RpgMakerLocationStep::index(0)]].concat(),
        );
        let line_location = |command_index| {
            RpgMakerLocation::value(
                source.clone(),
                [
                    list_steps.clone(),
                    vec![
                        RpgMakerLocationStep::index(command_index),
                        RpgMakerLocationStep::key("parameters"),
                        RpgMakerLocationStep::index(0),
                    ],
                ]
                .concat(),
            )
        };
        let recipe = || {
            DialogueWriteRecipe::new(
                group_location.clone(),
                None,
                vec![
                    DialogueLineRecipe::new(
                        line_location(1),
                        "バニー淫魔",
                        vec![DialogueLinePart::SpeakerSlot],
                    )
                    .unwrap(),
                    DialogueLineRecipe::new(
                        line_location(2),
                        "台词",
                        vec![DialogueLinePart::BodySlot { index: 0 }],
                    )
                    .unwrap(),
                ],
            )
            .unwrap()
        };
        let mutation = || {
            ReplaceDialogueMutation::new(
                recipe(),
                Some("兔女郎魅魔".to_owned()),
                BTreeMap::from([(0, vec!["译文".to_owned()])]),
            )
            .unwrap()
        };
        let documents = |second_line: &str, append_401: bool| {
            let mut list = vec![
                json!({"code":101,"indent":0,"parameters":["",0,0,2]}),
                json!({"code":401,"indent":0,"parameters":["バニー淫魔"]}),
                json!({"code":401,"indent":0,"parameters":[second_line]}),
            ];
            if append_401 {
                list.push(json!({"code":401,"indent":0,"parameters":["新增行"]}));
            }
            list.push(json!({"code":0,"indent":0,"parameters":[]}));
            RpgMakerProjectDocuments::new(
                BTreeMap::from([(RpgMakerDocumentId::Map(9), json!({"list":list}))]),
                Vec::new(),
            )
        };

        let candidate = rewrite_documents(
            project_name(),
            workspace_root(),
            documents("台词", false),
            plan(vec![StandardWriteBackMutation::ReplaceDialogue(mutation())]),
        )
        .expect("精确首行 Speaker 应可写回");
        let map: Value =
            serde_json::from_str(file_text(&candidate, Path::new("data/Map009.json"))).unwrap();
        assert_eq!(map["list"][1]["parameters"][0], "兔女郎魅魔");
        assert_eq!(map["list"][2]["parameters"][0], "译文");

        for (documents, expected_message) in [
            (documents("来源已变化", false), "expected_raw"),
            (documents("台词", true), "完整 401 块"),
        ] {
            let error = rewrite_documents(
                project_name(),
                workspace_root(),
                documents,
                plan(vec![StandardWriteBackMutation::ReplaceDialogue(mutation())]),
            )
            .expect_err("来源或块长度漂移必须失败");
            assert!(matches!(
                error,
                RpgMakerWriteBackDocumentRewriteFailure::InvalidMutation { message, .. }
                    if message.contains(expected_message)
            ));
        }
    }

    #[test]
    fn rebuilds_405_scrolling_body_and_rejects_the_wrong_native_code() {
        let source = RpgMakerSource::data(StandardDataFile::CommonEvents);
        let list_steps = vec![
            RpgMakerLocationStep::index(1),
            RpgMakerLocationStep::key("list"),
        ];
        let mut group_steps = list_steps.clone();
        group_steps.push(RpgMakerLocationStep::index(0));
        let mut segment_steps = list_steps.clone();
        segment_steps.extend([
            RpgMakerLocationStep::index(1),
            RpgMakerLocationStep::key("parameters"),
            RpgMakerLocationStep::index(0),
        ]);
        let mutation = ReplaceEventBodyMutation::new(
            RpgMakerLocation::value(source.clone(), group_steps),
            vec![EventBodyMutationSegment::replace_for_test(
                RpgMakerLocation::value(source, segment_steps),
                "滚动原文",
                vec!["滚动译文一".to_owned(), "滚动译文二".to_owned()],
            )],
        )
        .expect("滚动文本 Mutation 应该合法");
        let mutation_plan = plan(vec![StandardWriteBackMutation::ReplaceEventBody(mutation)]);
        let documents_with_code = |body_code| {
            RpgMakerProjectDocuments::new(
                BTreeMap::from([(
                    RpgMakerDocumentId::Data(StandardDataFile::CommonEvents),
                    json!([
                        null,
                        {
                            "list": [
                                {"code":105,"indent":2,"parameters":[2,false],"headerUnknown":true},
                                {"code":body_code,"indent":4,"parameters":["滚动原文"],"bodyUnknown":"kept"},
                                {"code":0,"indent":0,"parameters":[]}
                            ]
                        }
                    ]),
                )]),
                Vec::new(),
            )
        };

        let candidate = rewrite_documents(
            project_name(),
            workspace_root(),
            documents_with_code(405),
            mutation_plan.clone(),
        )
        .expect("规范 105/405 应成功重建");
        let common_events: Value =
            serde_json::from_str(file_text(&candidate, Path::new("data/CommonEvents.json")))
                .expect("CommonEvents 候选应该是 JSON");
        let list = common_events[1]["list"]
            .as_array()
            .expect("滚动文本 list 应保持数组");
        assert_eq!(
            list.iter()
                .map(|command| command["code"].as_i64().unwrap())
                .collect::<Vec<_>>(),
            vec![105, 405, 405, 0]
        );
        assert_eq!(list[0]["headerUnknown"], true);
        assert_eq!(list[1]["parameters"][0], "滚动译文一");
        assert_eq!(list[2]["parameters"][0], "滚动译文二");
        for command in &list[1..=2] {
            assert_eq!(command["indent"], 4);
            assert_eq!(command["bodyUnknown"], "kept");
        }

        let error = rewrite_documents(
            project_name(),
            workspace_root(),
            documents_with_code(401),
            mutation_plan,
        )
        .expect_err("滚动正文不能接受 401 命令");
        assert!(matches!(
            error,
            RpgMakerWriteBackDocumentRewriteFailure::InvalidMutation { message, .. }
                if message.contains("命令码")
        ));
    }

    #[test]
    fn rejects_a_malformed_command_after_an_event_body() {
        let source = RpgMakerSource::data(StandardDataFile::CommonEvents);
        let list_steps = vec![
            RpgMakerLocationStep::index(1),
            RpgMakerLocationStep::key("list"),
        ];
        let mut group_steps = list_steps.clone();
        group_steps.push(RpgMakerLocationStep::index(0));
        let mut segment_steps = list_steps;
        segment_steps.extend([
            RpgMakerLocationStep::index(1),
            RpgMakerLocationStep::key("parameters"),
            RpgMakerLocationStep::index(0),
        ]);
        let mutation = ReplaceEventBodyMutation::new(
            RpgMakerLocation::value(source.clone(), group_steps),
            vec![EventBodyMutationSegment::replace_for_test(
                RpgMakerLocation::value(source, segment_steps),
                "滚动原文",
                vec!["滚动译文".to_owned()],
            )],
        )
        .expect("滚动文本 Mutation 应该合法");
        let documents = RpgMakerProjectDocuments::new(
            BTreeMap::from([(
                RpgMakerDocumentId::Data(StandardDataFile::CommonEvents),
                json!([
                    null,
                    {
                        "list": [
                            {"code":105,"indent":0,"parameters":[2,false]},
                            {"code":405,"indent":0,"parameters":["滚动原文"]},
                            {"indent":0,"parameters":[]}
                        ]
                    }
                ]),
            )]),
            Vec::new(),
        );

        let error = rewrite_documents(
            project_name(),
            workspace_root(),
            documents,
            plan(vec![StandardWriteBackMutation::ReplaceEventBody(mutation)]),
        )
        .expect_err("无法证明正文块边界时必须拒绝改写");

        assert!(matches!(
            error,
            RpgMakerWriteBackDocumentRewriteFailure::InvalidMutation { message, .. }
                if message.contains("code")
        ));
    }

    #[test]
    fn plugin_lookup_never_falls_back_from_the_structured_index() {
        let plugins = vec![
            PluginConfiguration::new(
                0,
                serde_json::from_value(json!({
                    "name": "Wrong",
                    "parameters": {"Title": "旧"}
                }))
                .unwrap(),
            ),
            PluginConfiguration::new(
                1,
                serde_json::from_value(json!({
                    "name": "Expected",
                    "parameters": {"Title": "旧"}
                }))
                .unwrap(),
            ),
        ];
        let documents = RpgMakerProjectDocuments::new(BTreeMap::new(), plugins);
        let location = RpgMakerLocation::value(
            RpgMakerSource::plugin_parameter(0, "Expected", "Title"),
            Vec::new(),
        );

        let error = rewrite_documents(
            project_name(),
            workspace_root(),
            documents,
            plan(vec![set_text(location, "旧", "新")]),
        )
        .expect_err("插件身份不匹配不得按名称回退");

        assert!(matches!(
            error,
            RpgMakerWriteBackDocumentRewriteFailure::InvalidMutation { message, .. }
                if message.contains("name")
        ));
    }

    #[test]
    fn original_mismatch_and_noncanonical_event_segment_are_rejected() {
        let source = RpgMakerSource::data(StandardDataFile::Items);
        let documents = RpgMakerProjectDocuments::new(
            BTreeMap::from([(
                RpgMakerDocumentId::Data(StandardDataFile::Items),
                json!([null, {"name": "原文"}]),
            )]),
            Vec::new(),
        );
        let location = RpgMakerLocation::value(
            source,
            vec![
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("name"),
            ],
        );

        let error = rewrite_documents(
            project_name(),
            workspace_root(),
            documents,
            plan(vec![set_text(location, "错误原文", "译文")]),
        )
        .expect_err("expected_original 不匹配必须失败");

        assert!(matches!(
            error,
            RpgMakerWriteBackDocumentRewriteFailure::InvalidMutation { message, .. }
                if message.contains("expected_original")
        ));
    }

    #[test]
    fn candidate_paths_reject_parent_components_and_non_rpg_maker_roots() {
        assert!(
            RpgMakerRewrittenFile::new(PathBuf::from("../data/Actors.json"), Vec::new()).is_err()
        );
        assert!(RpgMakerRewrittenFile::new(PathBuf::from("other/file.json"), Vec::new()).is_err());
        assert!(RpgMakerRewrittenFile::new(PathBuf::from("data/Actors.json"), Vec::new()).is_ok());
    }

    #[test]
    fn rewrite_future_is_send() {
        fn assert_send(_: impl Send) {}

        let service = RpgMakerWriteBackDocumentRewritingService::new(PanickingReader, PanickingCpu);
        let project = project();
        assert_send(service.rewrite(&project, StandardWriteBackMutationPlan::empty()));
    }

    fn set_text(
        location: RpgMakerLocation,
        expected: impl Into<String>,
        replacement: impl Into<String>,
    ) -> StandardWriteBackMutation {
        StandardWriteBackMutation::SetText(SetTextMutation::for_test(
            location,
            expected,
            replacement,
        ))
    }

    fn plan(mutations: Vec<StandardWriteBackMutation>) -> StandardWriteBackMutationPlan {
        StandardWriteBackMutationPlan::new(mutations).expect("测试 Mutation Plan 应该合法")
    }

    fn file_text<'a>(candidate: &'a RpgMakerRewrittenDocuments, path: &Path) -> &'a str {
        let bytes = candidate
            .files()
            .iter()
            .find(|file| file.relative_path() == path)
            .unwrap_or_else(|| panic!("候选中缺少 {}", path.display()))
            .bytes();
        std::str::from_utf8(bytes).expect("候选文件应该是 UTF-8")
    }

    fn assert_appears_in_order(text: &str, needles: &[&str]) {
        let mut offset = 0;
        for needle in needles {
            let relative = text[offset..]
                .find(needle)
                .unwrap_or_else(|| panic!("候选文本缺少顺序标记 {needle}"));
            offset += relative + needle.len();
        }
    }

    fn project_name() -> ProjectName {
        "demo".parse().expect("测试项目名应该合法")
    }

    fn workspace_root() -> PathBuf {
        PathBuf::from("C:/projects/demo")
    }

    fn project() -> OpenedProject {
        OpenedProject::new(
            project_name(),
            workspace_root(),
            workspace_root().join("project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            test_layout_profile(),
        )
    }
}
