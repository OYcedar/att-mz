//! 把 RPG Maker Mutation Plan 应用到冻结 RPG Maker 文档的候选生成服务。

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use super::WriteBackProgressPhase;
use super::planner::{
    ReplaceChoicesMutation, ReplaceDialogueMutation, ReplaceEventBodyMutation,
    RpgMakerWriteBackDocumentRewriter, RpgMakerWriteBackMutation, RpgMakerWriteBackMutationPlan,
    SetTextMutation,
};
use crate::diagnostic::{
    Diagnostic, DiagnosticReport, FileSystemOrdinalKeyPhase, IoFailure, ReportedFailure,
    RpgMakerComputeFailure, RpgMakerDocumentConsumer, RpgMakerIssue, RpgMakerJsonFailureKind,
    RpgMakerWriteBackDocumentRewriteProblem, RpgMakerWriteBackMutationViolation, SafePath,
    StateEffect,
};
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::json::{
    StackSafeJsonError, StackSafeJsonValue, clone_value, drop_value, from_str as parse_json,
    to_string as encode_json, to_string_pretty as encode_json_pretty,
    to_vec_pretty as encode_json_pretty_bytes,
};
use crate::progress::{NoopProgressObserver, ProgressObserver, ProgressSnapshot};
use crate::project_name::ProjectName;
use crate::rpg_maker::extract::document::{
    RpgMakerDocumentId, RpgMakerDocumentSelection, RpgMakerProjectDocumentReader,
    RpgMakerProjectDocumentReadingDiagnostic, RpgMakerProjectDocuments,
};
use crate::rpg_maker::model::DialogueLinePart;
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::structured_path::{
    StructuredPathAccessError, StructuredPathCodec, StructuredPathDecoder, StructuredPathError,
    edit_structured_path, split_at_decode_boundary,
    value_at_plain_steps_mut as shared_value_at_plain_steps_mut,
};
use crate::rpg_maker::text::{RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource};
use crate::runtime::cpu::CpuExecutorUnavailable;
use crate::windows_path::{WindowsOrdinalCaseKey, WindowsOrdinalCaseKeyError};

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
        files: Vec<RpgMakerRewrittenFile>,
    ) -> Result<Self, RpgMakerWriteBackDocumentRewriteFailure> {
        let mut files = files;
        files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        let mut seen_paths = HashMap::with_capacity(files.len());
        for file in &files {
            let key = windows_output_path_key(&file.relative_path)?;
            if let Some(first_path) = seen_paths.insert(key, file.relative_path.clone()) {
                return Err(
                    RpgMakerWriteBackDocumentRewriteFailure::DuplicateOutputPath {
                        path: first_path,
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

fn windows_output_path_key(
    path: &Path,
) -> Result<Vec<WindowsOrdinalCaseKey>, RpgMakerWriteBackDocumentRewriteFailure> {
    path.components()
        .map(|component| {
            let Component::Normal(name) = component else {
                unreachable!("候选输出路径已经过结构校验")
            };
            WindowsOrdinalCaseKey::from_os_str(name).map_err(|source| {
                RpgMakerWriteBackDocumentRewriteFailure::OutputPathCaseKey {
                    path: path.to_path_buf(),
                    source,
                }
            })
        })
        .collect()
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
    let mut has_file_name = false;
    for component in components {
        if !matches!(component, Component::Normal(_)) {
            return Err(RpgMakerWriteBackDocumentRewriteFailure::InvalidOutputPath {
                path: path.to_path_buf(),
            });
        }
        has_file_name = true;
    }
    if !has_file_name {
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
    progress: Arc<dyn ProgressObserver<WriteBackProgressPhase>>,
}

impl<R, C> RpgMakerWriteBackDocumentRewritingService<R, C> {
    pub(crate) fn new(document_reader: R, cpu_executor: C) -> Self {
        Self {
            document_reader,
            cpu_executor,
            progress: Arc::new(NoopProgressObserver),
        }
    }

    /// 为文档改写绑定同步、不可失败的业务进度观察者。
    pub(crate) fn with_progress<Q>(mut self, progress: Q) -> Self
    where
        Q: ProgressObserver<WriteBackProgressPhase> + 'static,
    {
        self.progress = Arc::new(progress);
        self
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
        plan: RpgMakerWriteBackMutationPlan,
    ) -> Result<Self::RewrittenDocuments, Self::Error> {
        let progress = Arc::clone(&self.progress);
        if plan.mutations().is_empty() {
            progress.observe(ProgressSnapshot::determinate(
                WriteBackProgressPhase::RewritingDocuments,
                0,
                0,
            ));
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
        let prepared = self
            .cpu_executor
            .execute(move || prepare_rewrite_jobs(project_name, workspace_root, documents, plan))
            .await
            .map_err(RpgMakerWriteBackDocumentRewritingError::ScheduleRewrite)?;
        let PreparedDocumentRewrite {
            project_name,
            workspace_root,
            jobs,
        } = prepared.map_err(RpgMakerWriteBackDocumentRewritingError::Rewrite)?;
        let total_documents = u64::try_from(jobs.len()).expect("写回文档数量必须可表示为 u64");
        progress.observe(ProgressSnapshot::determinate(
            WriteBackProgressPhase::RewritingDocuments,
            0,
            total_documents,
        ));
        let completed_documents = Arc::new(Mutex::new(0_u64));
        let jobs = jobs
            .into_iter()
            .map(|job| ProgressTrackedRewriteJob {
                job,
                progress: Arc::clone(&progress),
                completed_documents: Arc::clone(&completed_documents),
                total_documents,
            })
            .collect();
        let rewritten = self
            .cpu_executor
            .execute_ordered_map(jobs, rewrite_document_with_progress)
            .await
            .map_err(RpgMakerWriteBackDocumentRewritingError::ScheduleRewrite)?;
        self.cpu_executor
            .execute(move || {
                let files = rewritten
                    .into_iter()
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .flatten()
                    .collect();
                RpgMakerRewrittenDocuments::new(project_name, workspace_root, files)
            })
            .await
            .map_err(RpgMakerWriteBackDocumentRewritingError::ScheduleRewrite)?
            .map_err(RpgMakerWriteBackDocumentRewritingError::Rewrite)
    }
}

fn selection_for_plan(plan: &RpgMakerWriteBackMutationPlan) -> RpgMakerDocumentSelection {
    let mut selection = RpgMakerDocumentSelection::empty();
    for mutation in plan.mutations() {
        match mutation {
            RpgMakerWriteBackMutation::SetText(mutation) => {
                select_source(&mut selection, mutation.exact_location().source());
            }
            RpgMakerWriteBackMutation::ReplaceDialogue(mutation) => {
                select_source(&mut selection, mutation.group_location().source());
                if let Some(speaker) = mutation.recipe().direct_speaker() {
                    select_source(&mut selection, speaker.physical_location().source());
                }
                for line in mutation.recipe().lines() {
                    select_source(&mut selection, line.physical_location().source());
                }
            }
            RpgMakerWriteBackMutation::ReplaceChoices(mutation) => {
                select_source(&mut selection, mutation.group_location().source());
                for recipe in mutation.recipes() {
                    select_source(&mut selection, recipe.target().source());
                }
            }
            RpgMakerWriteBackMutation::ReplaceEventBody(mutation) => {
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
    documents: BTreeMap<RpgMakerDocumentId, StackSafeJsonValue>,
    plugins: Vec<(usize, StackSafeJsonValue)>,
    plugins_prefix: String,
    changed_documents: BTreeSet<RpgMakerDocumentId>,
    plugins_changed: bool,
}

impl MutableDocuments {
    fn from_document(id: RpgMakerDocumentId, value: StackSafeJsonValue) -> Self {
        Self {
            documents: BTreeMap::from([(id, value)]),
            plugins: Vec::new(),
            plugins_prefix: String::new(),
            changed_documents: BTreeSet::new(),
            plugins_changed: false,
        }
    }

    fn from_plugins(plugins: Vec<(usize, StackSafeJsonValue)>, plugins_prefix: String) -> Self {
        Self {
            documents: BTreeMap::new(),
            plugins,
            plugins_prefix,
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
            mutation_failure(
                location,
                WriteBackMutationViolation::PluginParameterAsDocument,
            )
        })?;
        let document = self.documents.get_mut(&id).ok_or_else(|| {
            mutation_failure(
                location,
                WriteBackMutationViolation::RequestedDocumentMissing,
            )
        })?;
        Ok((&mut *document, id))
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

trait TextValueMutation {
    fn exact_location(&self) -> &RpgMakerLocation;
    fn expected_original(&self) -> &str;
    fn replacement(&self) -> &str;
}

impl TextValueMutation for SetTextMutation {
    fn exact_location(&self) -> &RpgMakerLocation {
        self.exact_location()
    }

    fn expected_original(&self) -> &str {
        self.expected_original()
    }

    fn replacement(&self) -> &str {
        self.replacement()
    }
}

#[derive(Clone, Copy)]
struct IndexedValueMutation<'a> {
    ordinal: usize,
    mutation: &'a dyn TextValueMutation,
    steps: &'a [RpgMakerLocationStep],
}

struct IndexedMutationFailure {
    ordinal: usize,
    source: RpgMakerWriteBackDocumentRewriteFailure,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StructuralKey {
    source: RpgMakerSource,
    list_steps: Vec<RpgMakerLocationStep>,
    start_index: usize,
}

enum StructuralOperation<'a> {
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
            Self::Event { key, .. } | Self::Dialogue { key, .. } => key,
        }
    }

    fn location(&self) -> &RpgMakerLocation {
        match self {
            Self::Event { mutation, .. } => mutation.group_location(),
            Self::Dialogue { mutation, .. } => mutation.group_location(),
        }
    }
}

struct StructuralReplacement {
    start: usize,
    end: usize,
    values: Vec<StackSafeJsonValue>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum PhysicalDocumentKey {
    Json(RpgMakerDocumentId),
    Plugins,
}

struct DocumentRewriteJob {
    documents: MutableDocuments,
    mutations: Vec<RpgMakerWriteBackMutation>,
}

struct ProgressTrackedRewriteJob {
    job: DocumentRewriteJob,
    progress: Arc<dyn ProgressObserver<WriteBackProgressPhase>>,
    completed_documents: Arc<Mutex<u64>>,
    total_documents: u64,
}

struct PreparedDocumentRewrite {
    project_name: ProjectName,
    workspace_root: PathBuf,
    jobs: Vec<DocumentRewriteJob>,
}

fn prepare_rewrite_jobs(
    project_name: ProjectName,
    workspace_root: PathBuf,
    documents: RpgMakerProjectDocuments,
    plan: RpgMakerWriteBackMutationPlan,
) -> Result<PreparedDocumentRewrite, RpgMakerWriteBackDocumentRewriteFailure> {
    validate_structural_conflicts(plan.mutations())?;
    let mut partitions = BTreeMap::<PhysicalDocumentKey, Vec<RpgMakerWriteBackMutation>>::new();
    for mutation in plan.into_mutations() {
        let key = mutation_document_key(&mutation)?;
        partitions.entry(key).or_default().push(mutation);
    }

    let (mut json_documents, plugins, plugins_prefix) = documents.into_parts();
    let mut plugins = Some(
        plugins
            .into_iter()
            .map(|configuration| configuration.into_parts())
            .collect(),
    );
    let mut jobs = Vec::with_capacity(partitions.len());
    for (key, mutations) in partitions {
        let representative = mutation_location(
            mutations
                .first()
                .expect("每个物理文档分区必须至少包含一项 Mutation"),
        );
        let documents = match key {
            PhysicalDocumentKey::Json(id) => {
                let value = json_documents.remove(&id).ok_or_else(|| {
                    mutation_failure(
                        representative,
                        WriteBackMutationViolation::RequestedDocumentMissing,
                    )
                })?;
                MutableDocuments::from_document(id, value)
            }
            PhysicalDocumentKey::Plugins => MutableDocuments::from_plugins(
                plugins.take().expect("plugins.js 只能形成一个物理文档分区"),
                plugins_prefix.clone(),
            ),
        };
        jobs.push(DocumentRewriteJob {
            documents,
            mutations,
        });
    }

    Ok(PreparedDocumentRewrite {
        project_name,
        workspace_root,
        jobs,
    })
}

fn mutation_document_key(
    mutation: &RpgMakerWriteBackMutation,
) -> Result<PhysicalDocumentKey, RpgMakerWriteBackDocumentRewriteFailure> {
    let (location, other_locations): (&RpgMakerLocation, Vec<&RpgMakerLocation>) = match mutation {
        RpgMakerWriteBackMutation::SetText(mutation) => (mutation.exact_location(), Vec::new()),
        RpgMakerWriteBackMutation::ReplaceDialogue(mutation) => {
            let mut locations = mutation
                .recipe()
                .lines()
                .iter()
                .map(|line| line.physical_location())
                .collect::<Vec<_>>();
            if let Some(speaker) = mutation.recipe().direct_speaker() {
                locations.push(speaker.physical_location());
            }
            (mutation.group_location(), locations)
        }
        RpgMakerWriteBackMutation::ReplaceChoices(mutation) => (
            mutation.group_location(),
            mutation
                .recipes()
                .iter()
                .map(|recipe| recipe.target())
                .collect(),
        ),
        RpgMakerWriteBackMutation::ReplaceEventBody(mutation) => (
            mutation.group_location(),
            mutation
                .segments()
                .iter()
                .map(|segment| segment.exact_location())
                .collect(),
        ),
    };
    let key = physical_document_key(location.source());
    for other in other_locations {
        if physical_document_key(other.source()) != key {
            return Err(mutation_failure(
                other,
                WriteBackMutationViolation::CrossDocumentMutation,
            ));
        }
    }
    Ok(key)
}

fn physical_document_key(source: &RpgMakerSource) -> PhysicalDocumentKey {
    document_id(source).map_or(PhysicalDocumentKey::Plugins, PhysicalDocumentKey::Json)
}

fn mutation_location(mutation: &RpgMakerWriteBackMutation) -> &RpgMakerLocation {
    match mutation {
        RpgMakerWriteBackMutation::SetText(mutation) => mutation.exact_location(),
        RpgMakerWriteBackMutation::ReplaceDialogue(mutation) => mutation.group_location(),
        RpgMakerWriteBackMutation::ReplaceChoices(mutation) => mutation.group_location(),
        RpgMakerWriteBackMutation::ReplaceEventBody(mutation) => mutation.group_location(),
    }
}

fn validate_structural_conflicts(
    mutations: &[RpgMakerWriteBackMutation],
) -> Result<(), RpgMakerWriteBackDocumentRewriteFailure> {
    let mut structural = Vec::<(StructuralKey, &RpgMakerLocation)>::new();
    for mutation in mutations {
        match mutation {
            RpgMakerWriteBackMutation::SetText(_) => {}
            RpgMakerWriteBackMutation::ReplaceDialogue(mutation) => structural.push((
                dialogue_structural_key(mutation)?,
                mutation.group_location(),
            )),
            RpgMakerWriteBackMutation::ReplaceChoices(_) => {}
            RpgMakerWriteBackMutation::ReplaceEventBody(mutation) => {
                structural.push((event_structural_key(mutation)?, mutation.group_location()))
            }
        }
    }
    structural.sort_by(|left, right| {
        left.0
            .source
            .cmp(&right.0.source)
            .then_with(|| left.0.list_steps.cmp(&right.0.list_steps))
            .then_with(|| right.0.start_index.cmp(&left.0.start_index))
    });
    for pair in structural.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(mutation_failure(
                pair[0].1,
                WriteBackMutationViolation::DuplicateStructuralTarget,
            ));
        }
    }
    Ok(())
}

fn rewrite_document(
    job: DocumentRewriteJob,
) -> Result<Vec<RpgMakerRewrittenFile>, RpgMakerWriteBackDocumentRewriteFailure> {
    let DocumentRewriteJob {
        mut documents,
        mutations,
    } = job;
    apply_mutations(&mut documents, &mutations)?;
    serialize_rewritten_files(documents)
}

fn rewrite_document_with_progress(
    tracked: ProgressTrackedRewriteJob,
) -> Result<Vec<RpgMakerRewrittenFile>, RpgMakerWriteBackDocumentRewriteFailure> {
    let result = rewrite_document(tracked.job);
    if result.is_ok() {
        let mut completed = tracked
            .completed_documents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *completed += 1;
        tracked.progress.observe(ProgressSnapshot::determinate(
            WriteBackProgressPhase::RewritingDocuments,
            *completed,
            tracked.total_documents,
        ));
    }
    result
}

fn apply_mutations(
    documents: &mut MutableDocuments,
    mutations: &[RpgMakerWriteBackMutation],
) -> Result<(), RpgMakerWriteBackDocumentRewriteFailure> {
    let mut value_mutations = Vec::new();
    let mut event_mutations = Vec::new();
    let mut dialogue_mutations = Vec::new();
    let mut choices_mutations = Vec::new();

    for (ordinal, mutation) in mutations.iter().enumerate() {
        match mutation {
            RpgMakerWriteBackMutation::SetText(mutation) => {
                value_mutations.push((ordinal, mutation as &dyn TextValueMutation))
            }
            RpgMakerWriteBackMutation::ReplaceDialogue(mutation) => {
                dialogue_mutations.push(mutation)
            }
            RpgMakerWriteBackMutation::ReplaceChoices(mutation) => choices_mutations.push(mutation),
            RpgMakerWriteBackMutation::ReplaceEventBody(mutation) => event_mutations.push(mutation),
        }
    }

    apply_value_mutations(documents, &value_mutations)?;

    // 选项只修改既有值，但必须在任何事件列表 splice 前按整组验收并同步 102/402。
    for mutation in choices_mutations {
        apply_choices_mutation(documents, mutation)?;
    }

    let mut structural = Vec::new();
    for mutation in event_mutations {
        let key = event_structural_key(mutation)?;
        structural.push(StructuralOperation::Event { key, mutation });
    }
    for mutation in dialogue_mutations {
        let key = dialogue_structural_key(mutation)?;
        structural.push(StructuralOperation::Dialogue { key, mutation });
    }
    structural.sort_by(compare_structural_operations);
    apply_structural_operations(documents, structural)?;

    Ok(())
}

#[cfg(test)]
fn rewrite_documents(
    project_name: ProjectName,
    workspace_root: PathBuf,
    documents: RpgMakerProjectDocuments,
    plan: RpgMakerWriteBackMutationPlan,
) -> Result<RpgMakerRewrittenDocuments, RpgMakerWriteBackDocumentRewriteFailure> {
    let PreparedDocumentRewrite {
        project_name,
        workspace_root,
        jobs,
    } = prepare_rewrite_jobs(project_name, workspace_root, documents, plan)?;
    let files = jobs
        .into_iter()
        .map(rewrite_document)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();
    RpgMakerRewrittenDocuments::new(project_name, workspace_root, files)
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

fn apply_structural_operations(
    documents: &mut MutableDocuments,
    operations: Vec<StructuralOperation<'_>>,
) -> Result<(), RpgMakerWriteBackDocumentRewriteFailure> {
    let mut groups = BTreeMap::<ContainerKey, Vec<StructuralOperation<'_>>>::new();
    for operation in operations {
        let key = operation.key();
        groups
            .entry(ContainerKey {
                source: key.source.clone(),
                steps: key.list_steps.clone(),
            })
            .or_default()
            .push(operation);
    }

    for (key, operations) in groups {
        let representative = operations[0].location().clone();
        let has_decode_boundary = key
            .steps
            .iter()
            .any(|step| matches!(step, RpgMakerLocationStep::DecodeJsonString));
        if has_decode_boundary {
            return Err(mutation_failure(
                &representative,
                WriteBackMutationViolation::DecodeBoundaryInEventContainer,
            ));
        }

        let (document, id) = documents.document_mut(&key.source, &representative)?;
        edit_value_at_steps(document, &key.steps, &representative, |value| {
            let list = value.as_array_mut().ok_or_else(|| {
                mutation_failure(
                    &representative,
                    WriteBackMutationViolation::EventListNotArray,
                )
            })?;
            let mut replacements = Vec::<StructuralReplacement>::with_capacity(operations.len());
            for operation in operations {
                let operation_location = operation.location().clone();
                let replacement = match operation {
                    StructuralOperation::Event { key, mutation } => {
                        prepare_event_body_replacement(list, &key, mutation)?
                    }
                    StructuralOperation::Dialogue { key, mutation } => {
                        prepare_dialogue_replacement(list, &key, mutation)?
                    }
                };
                if let Some(previous) = replacements.last()
                    && replacement.end > previous.start
                {
                    return Err(mutation_failure(
                        &operation_location,
                        WriteBackMutationViolation::OverlappingFrozenRanges,
                    ));
                }
                replacements.push(replacement);
            }
            rebuild_structural_list(list, replacements);
            Ok(())
        })?;
        documents.mark_document_changed(id);
    }
    Ok(())
}

fn rebuild_structural_list(list: &mut Vec<Value>, mut replacements: Vec<StructuralReplacement>) {
    debug_assert!(!replacements.is_empty());
    replacements.reverse();
    debug_assert!(
        replacements
            .windows(2)
            .all(|pair| pair[0].end <= pair[1].start)
    );

    let removed = replacements
        .iter()
        .map(|replacement| replacement.end - replacement.start)
        .sum::<usize>();
    let inserted = replacements
        .iter()
        .map(|replacement| replacement.values.len())
        .sum::<usize>();
    let capacity = list
        .len()
        .checked_sub(removed)
        .and_then(|remaining| remaining.checked_add(inserted))
        .expect("受检结构替换后的列表长度必须可用 usize 表达");
    let mut original = std::mem::take(list).into_iter();
    let mut rebuilt = Vec::with_capacity(capacity);
    let mut cursor = 0;
    for replacement in replacements {
        rebuilt.extend(original.by_ref().take(replacement.start - cursor));
        for _ in replacement.start..replacement.end {
            drop_value(original.next().expect("受检结构替换范围必须位于原列表内"));
        }
        rebuilt.extend(
            replacement
                .values
                .into_iter()
                .map(StackSafeJsonValue::into_inner),
        );
        cursor = replacement.end;
    }
    rebuilt.extend(original);
    debug_assert_eq!(rebuilt.len(), capacity);
    *list = rebuilt;

    #[cfg(test)]
    STRUCTURAL_LIST_REBUILD_COUNT.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
thread_local! {
    static STRUCTURAL_LIST_REBUILD_COUNT: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_structural_list_rebuild_count() {
    STRUCTURAL_LIST_REBUILD_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
fn structural_list_rebuild_count() -> usize {
    STRUCTURAL_LIST_REBUILD_COUNT.with(std::cell::Cell::get)
}

fn apply_value_mutations(
    documents: &mut MutableDocuments,
    mutations: &[(usize, &dyn TextValueMutation)],
) -> Result<(), RpgMakerWriteBackDocumentRewriteFailure> {
    let mut decoded_groups = BTreeMap::<ContainerKey, Vec<IndexedValueMutation<'_>>>::new();
    let mut earliest_failure = None;
    for &(ordinal, mutation) in mutations {
        let source = mutation.exact_location().source();
        let steps = mutation.exact_location().steps();
        if let Some((container_steps, remaining_steps)) = split_at_decode_boundary(steps) {
            decoded_groups
                .entry(ContainerKey {
                    source: source.clone(),
                    steps: container_steps.to_vec(),
                })
                .or_default()
                .push(IndexedValueMutation {
                    ordinal,
                    mutation,
                    steps: remaining_steps,
                });
        } else if let Err(source) = apply_value_mutation(documents, mutation) {
            retain_earliest_failure(
                &mut earliest_failure,
                IndexedMutationFailure { ordinal, source },
            );
        }
    }

    for (key, mutations) in decoded_groups {
        if let Err(failure) = apply_decoded_value_group(documents, &key, &mutations) {
            retain_earliest_failure(&mut earliest_failure, failure);
        }
    }

    match earliest_failure {
        Some(failure) => Err(failure.source),
        None => Ok(()),
    }
}

fn apply_decoded_value_group(
    documents: &mut MutableDocuments,
    key: &ContainerKey,
    mutations: &[IndexedValueMutation<'_>],
) -> Result<(), IndexedMutationFailure> {
    let representative = earliest_indexed_mutation(mutations);
    let location = representative.mutation.exact_location();
    match &key.source {
        RpgMakerSource::Data(_) | RpgMakerSource::DataFile(_) | RpgMakerSource::Map(_) => {
            let id;
            {
                let (document, document_id) = documents
                    .document_mut(&key.source, location)
                    .map_err(|source| IndexedMutationFailure {
                        ordinal: representative.ordinal,
                        source,
                    })?;
                id = document_id;
                let container =
                    value_at_plain_steps_mut(document, &key.steps, location).map_err(|source| {
                        IndexedMutationFailure {
                            ordinal: representative.ordinal,
                            source,
                        }
                    })?;
                apply_decoded_json_mutations(container, mutations)?;
            }
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
                location,
            )
            .map_err(|source| IndexedMutationFailure {
                ordinal: representative.ordinal,
                source,
            })?;
            let container =
                value_at_plain_steps_mut(parameter, &key.steps, location).map_err(|source| {
                    IndexedMutationFailure {
                        ordinal: representative.ordinal,
                        source,
                    }
                })?;
            apply_decoded_json_mutations(container, mutations)?;
            documents.plugins_changed = true;
        }
    }
    Ok(())
}

fn apply_decoded_json_mutations(
    container: &mut Value,
    mutations: &[IndexedValueMutation<'_>],
) -> Result<(), IndexedMutationFailure> {
    let representative = earliest_indexed_mutation(mutations);
    let location = representative.mutation.exact_location();
    let raw = container.as_str().ok_or_else(|| IndexedMutationFailure {
        ordinal: representative.ordinal,
        source: mutation_failure(location, WriteBackMutationViolation::DecodeTargetNotString),
    })?;
    let decoded = decode_nested_json(raw, location).map_err(|source| IndexedMutationFailure {
        ordinal: representative.ordinal,
        source,
    })?;
    let decoded = apply_mutations_within_decoded_value(decoded, mutations)?;
    let encoded =
        encode_nested_json(&decoded, location).map_err(|source| IndexedMutationFailure {
            ordinal: representative.ordinal,
            source,
        })?;
    *container = Value::String(encoded);
    Ok(())
}

fn apply_mutations_within_decoded_value(
    value: StackSafeJsonValue,
    mutations: &[IndexedValueMutation<'_>],
) -> Result<StackSafeJsonValue, IndexedMutationFailure> {
    struct ParentLink {
        steps: Vec<RpgMakerLocationStep>,
        ordinal: usize,
        location: RpgMakerLocation,
    }

    struct Frame<'a> {
        value: StackSafeJsonValue,
        children: std::vec::IntoIter<(Vec<RpgMakerLocationStep>, Vec<IndexedValueMutation<'a>>)>,
        earliest_failure: Option<IndexedMutationFailure>,
        parent: Option<ParentLink>,
    }

    fn prepare_frame<'a>(
        mut value: StackSafeJsonValue,
        mutations: Vec<IndexedValueMutation<'a>>,
    ) -> Frame<'a> {
        let mut nested_groups =
            BTreeMap::<Vec<RpgMakerLocationStep>, Vec<IndexedValueMutation<'a>>>::new();
        let mut earliest_failure = None;
        for mutation in mutations {
            if let Some((container_steps, remaining_steps)) =
                split_at_decode_boundary(mutation.steps)
            {
                nested_groups
                    .entry(container_steps.to_vec())
                    .or_default()
                    .push(IndexedValueMutation {
                        ordinal: mutation.ordinal,
                        mutation: mutation.mutation,
                        steps: remaining_steps,
                    });
            } else if let Err(source) = replace_string_at(
                &mut value,
                mutation.steps,
                mutation.mutation.expected_original(),
                mutation.mutation.replacement(),
                mutation.mutation.exact_location(),
            ) {
                retain_earliest_failure(
                    &mut earliest_failure,
                    IndexedMutationFailure {
                        ordinal: mutation.ordinal,
                        source,
                    },
                );
            }
        }
        Frame {
            value,
            children: nested_groups.into_iter().collect::<Vec<_>>().into_iter(),
            earliest_failure,
            parent: None,
        }
    }

    let mut frames = vec![prepare_frame(value, mutations.to_vec())];
    loop {
        let next_child = frames
            .last_mut()
            .expect("解码工作栈在根完成前必须非空")
            .children
            .next();
        if let Some((steps, mutations)) = next_child {
            let representative = earliest_indexed_mutation(&mutations);
            let location = representative.mutation.exact_location().clone();
            let decoded = {
                let frame = frames.last_mut().expect("刚取得子任务的父 frame 必须存在");
                value_at_plain_steps_mut(&mut frame.value, &steps, &location)
                    .map_err(|source| IndexedMutationFailure {
                        ordinal: representative.ordinal,
                        source,
                    })
                    .and_then(|container| {
                        container
                            .as_str()
                            .ok_or_else(|| IndexedMutationFailure {
                                ordinal: representative.ordinal,
                                source: mutation_failure(
                                    &location,
                                    WriteBackMutationViolation::DecodeTargetNotString,
                                ),
                            })
                            .and_then(|raw| {
                                decode_nested_json(raw, &location).map_err(|source| {
                                    IndexedMutationFailure {
                                        ordinal: representative.ordinal,
                                        source,
                                    }
                                })
                            })
                    })
            };
            match decoded {
                Ok(decoded) => {
                    let mut child = prepare_frame(decoded, mutations);
                    child.parent = Some(ParentLink {
                        steps,
                        ordinal: representative.ordinal,
                        location,
                    });
                    frames.push(child);
                }
                Err(failure) => retain_earliest_failure(
                    &mut frames
                        .last_mut()
                        .expect("失败子任务的父 frame 必须存在")
                        .earliest_failure,
                    failure,
                ),
            }
            continue;
        }

        let completed = frames.pop().expect("待完成 frame 必须存在");
        let outcome = match completed.earliest_failure {
            Some(failure) => Err(failure),
            None => Ok(completed.value),
        };
        let Some(parent) = completed.parent else {
            return outcome;
        };
        let parent_frame = frames.last_mut().expect("非根解码 frame 必须拥有父 frame");
        match outcome.and_then(|value| {
            encode_nested_json(&value, &parent.location).map_err(|source| IndexedMutationFailure {
                ordinal: parent.ordinal,
                source,
            })
        }) {
            Ok(encoded) => {
                let assignment = value_at_plain_steps_mut(
                    &mut parent_frame.value,
                    &parent.steps,
                    &parent.location,
                )
                .map_err(|source| IndexedMutationFailure {
                    ordinal: parent.ordinal,
                    source,
                });
                match assignment {
                    Ok(target) => *target = Value::String(encoded),
                    Err(failure) => {
                        retain_earliest_failure(&mut parent_frame.earliest_failure, failure)
                    }
                }
            }
            Err(failure) => retain_earliest_failure(&mut parent_frame.earliest_failure, failure),
        }
    }
}

fn value_at_plain_steps_mut<'a>(
    value: &'a mut Value,
    steps: &[RpgMakerLocationStep],
    location: &RpgMakerLocation,
) -> Result<&'a mut Value, RpgMakerWriteBackDocumentRewriteFailure> {
    shared_value_at_plain_steps_mut(value, steps)
        .map_err(|error| structured_path_access_failure(error, location))
}

fn earliest_indexed_mutation<'a>(
    mutations: &[IndexedValueMutation<'a>],
) -> IndexedValueMutation<'a> {
    mutations
        .iter()
        .min_by_key(|mutation| mutation.ordinal)
        .copied()
        .expect("共享 DecodeJsonString 容器必须至少包含一个 Mutation")
}

fn retain_earliest_failure(
    current: &mut Option<IndexedMutationFailure>,
    candidate: IndexedMutationFailure,
) {
    if current
        .as_ref()
        .is_none_or(|current| candidate.ordinal < current.ordinal)
    {
        *current = Some(candidate);
    }
}

fn apply_value_mutation(
    documents: &mut MutableDocuments,
    mutation: &dyn TextValueMutation,
) -> Result<(), RpgMakerWriteBackDocumentRewriteFailure> {
    let source = mutation.exact_location().source();
    let steps = mutation.exact_location().steps();
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
    plugins: &'a mut [(usize, StackSafeJsonValue)],
    plugin_index: usize,
    plugin_name: &str,
    parameter_name: &str,
    location: &RpgMakerLocation,
) -> Result<&'a mut Value, RpgMakerWriteBackDocumentRewriteFailure> {
    let Some((stored_index, fields)) = plugins.get_mut(plugin_index) else {
        return Err(mutation_failure(
            location,
            WriteBackMutationViolation::PluginIndexMissing,
        ));
    };
    if *stored_index != plugin_index {
        return Err(mutation_failure(
            location,
            WriteBackMutationViolation::PluginIndexMismatch,
        ));
    }
    let actual_name = fields.get("name").and_then(Value::as_str);
    if actual_name != Some(plugin_name) {
        return Err(mutation_failure(
            location,
            WriteBackMutationViolation::PluginNameMismatch,
        ));
    }
    let parameters = fields
        .as_object_mut()
        .and_then(|fields| fields.get_mut("parameters"))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            mutation_failure(
                location,
                WriteBackMutationViolation::PluginParametersNotObject,
            )
        })?;
    parameters.get_mut(parameter_name).ok_or_else(|| {
        mutation_failure(location, WriteBackMutationViolation::PluginParameterMissing)
    })
}

fn replace_string_at(
    value: &mut Value,
    steps: &[RpgMakerLocationStep],
    expected: &str,
    replacement: &str,
    location: &RpgMakerLocation,
) -> Result<(), RpgMakerWriteBackDocumentRewriteFailure> {
    edit_value_at_steps(value, steps, location, |value| {
        let actual = value.as_str().ok_or_else(|| {
            mutation_failure(location, WriteBackMutationViolation::TargetNotString)
        })?;
        if actual != expected {
            return Err(mutation_failure(
                location,
                WriteBackMutationViolation::ExpectedOriginalMismatch,
            ));
        }
        *value = Value::String(replacement.to_owned());
        Ok(())
    })
}

#[cfg(test)]
thread_local! {
    static NESTED_JSON_CODEC_COUNTS: std::cell::Cell<(usize, usize)> =
        const { std::cell::Cell::new((0, 0)) };
}

fn decode_nested_json(
    raw: &str,
    location: &RpgMakerLocation,
) -> Result<StackSafeJsonValue, RpgMakerWriteBackDocumentRewriteFailure> {
    #[cfg(test)]
    NESTED_JSON_CODEC_COUNTS.with(|counts| {
        let (decoded, encoded) = counts.get();
        counts.set((decoded + 1, encoded));
    });

    parse_json(raw).map_err(
        |source| RpgMakerWriteBackDocumentRewriteFailure::DecodeNestedJson {
            location: Box::new(location.clone()),
            source,
        },
    )
}

fn encode_nested_json(
    value: &Value,
    location: &RpgMakerLocation,
) -> Result<String, RpgMakerWriteBackDocumentRewriteFailure> {
    #[cfg(test)]
    NESTED_JSON_CODEC_COUNTS.with(|counts| {
        let (decoded, encoded) = counts.get();
        counts.set((decoded, encoded + 1));
    });

    encode_json(value).map_err(
        |source| RpgMakerWriteBackDocumentRewriteFailure::EncodeNestedJson {
            location: Box::new(location.clone()),
            source,
        },
    )
}

#[cfg(test)]
fn reset_nested_json_codec_counts() {
    NESTED_JSON_CODEC_COUNTS.with(|counts| counts.set((0, 0)));
}

#[cfg(test)]
fn nested_json_codec_counts() -> (usize, usize) {
    NESTED_JSON_CODEC_COUNTS.with(std::cell::Cell::get)
}

/// 沿结构化路径编辑一个值，并在每个 `DecodeJsonString` 边界成功返回后逐层写回。
///
/// 解码后的值只存在于当前调用的局部副本中；任一深层定位、验收或重编码失败时，
/// 对应的外层 JSON string 都不会被替换。
fn edit_value_at_steps<R, F>(
    value: &mut Value,
    steps: &[RpgMakerLocationStep],
    location: &RpgMakerLocation,
    edit: F,
) -> Result<R, RpgMakerWriteBackDocumentRewriteFailure>
where
    F: FnOnce(&mut Value) -> Result<R, RpgMakerWriteBackDocumentRewriteFailure>,
{
    let mut codec = RewriteStructuredPathCodec { location };
    edit_structured_path(value, steps, &mut codec, edit)
        .map_err(|error| rewrite_structured_path_error(error, location))?
}

struct RewriteStructuredPathCodec<'a> {
    location: &'a RpgMakerLocation,
}

impl StructuredPathDecoder for RewriteStructuredPathCodec<'_> {
    type Value = Value;
    type Owned = StackSafeJsonValue;
    type DecodeError = RpgMakerWriteBackDocumentRewriteFailure;

    fn decode(&mut self, source: &str) -> Result<Self::Owned, Self::DecodeError> {
        decode_nested_json(source, self.location)
    }

    fn value(owned: &Self::Owned) -> &Self::Value {
        owned
    }

    fn value_mut(owned: &mut Self::Owned) -> &mut Self::Value {
        owned
    }
}

impl StructuredPathCodec for RewriteStructuredPathCodec<'_> {
    type EncodeError = RpgMakerWriteBackDocumentRewriteFailure;

    fn encode(&mut self, value: &Self::Value) -> Result<String, Self::EncodeError> {
        encode_nested_json(value, self.location)
    }
}

fn rewrite_structured_path_error(
    error: StructuredPathError<
        RpgMakerWriteBackDocumentRewriteFailure,
        RpgMakerWriteBackDocumentRewriteFailure,
    >,
    location: &RpgMakerLocation,
) -> RpgMakerWriteBackDocumentRewriteFailure {
    match error {
        StructuredPathError::Access(error) => structured_path_access_failure(error, location),
        StructuredPathError::ExpectedEncodedJsonString => {
            mutation_failure(location, WriteBackMutationViolation::DecodeTargetNotString)
        }
        StructuredPathError::Decode(source) | StructuredPathError::Encode(source) => source,
    }
}

fn structured_path_access_failure(
    error: StructuredPathAccessError,
    location: &RpgMakerLocation,
) -> RpgMakerWriteBackDocumentRewriteFailure {
    match error {
        StructuredPathAccessError::ExpectedObject | StructuredPathAccessError::MissingObjectKey => {
            mutation_failure(
                location,
                WriteBackMutationViolation::ObjectPathMissingOrWrongType,
            )
        }
        StructuredPathAccessError::ExpectedArray | StructuredPathAccessError::MissingArrayIndex => {
            mutation_failure(
                location,
                WriteBackMutationViolation::ArrayPathMissingOrWrongType,
            )
        }
        StructuredPathAccessError::UnexpectedDecodeBoundary => mutation_failure(
            location,
            WriteBackMutationViolation::UnexpectedDecodeBoundary,
        ),
    }
}

fn structural_key(
    source: &RpgMakerSource,
    steps: &[RpgMakerLocationStep],
    location: &RpgMakerLocation,
) -> Result<StructuralKey, RpgMakerWriteBackDocumentRewriteFailure> {
    let Some((last, list_steps)) = steps.split_last() else {
        return Err(mutation_failure(
            location,
            WriteBackMutationViolation::MissingCommandArrayIndex,
        ));
    };
    let RpgMakerLocationStep::ArrayIndex(start_index) = last else {
        return Err(mutation_failure(
            location,
            WriteBackMutationViolation::CommandPathNotArrayIndex,
        ));
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
    structural_key(
        mutation.group_location().source(),
        mutation.group_location().steps(),
        mutation.group_location(),
    )
}

fn dialogue_structural_key(
    mutation: &ReplaceDialogueMutation,
) -> Result<StructuralKey, RpgMakerWriteBackDocumentRewriteFailure> {
    structural_key(
        mutation.group_location().source(),
        mutation.group_location().steps(),
        mutation.group_location(),
    )
}

fn apply_choices_mutation(
    documents: &mut MutableDocuments,
    mutation: &ReplaceChoicesMutation,
) -> Result<(), RpgMakerWriteBackDocumentRewriteFailure> {
    let location = mutation.group_location();
    let source = location.source();
    let steps = location.steps();
    let key = structural_key(source, steps, location)?;
    let (document, id) = documents.document_mut(source, location)?;
    let list = event_list_mut(document, &key.list_steps, location)?;
    let header = list.get(key.start_index).ok_or_else(|| {
        mutation_failure(location, WriteBackMutationViolation::ChoiceStartOutOfBounds)
    })?;
    if command_code(header, location)? != 102 {
        return Err(mutation_failure(
            location,
            WriteBackMutationViolation::ChoiceStartNot102,
        ));
    }
    let header_indent = command_indent(header, location)?;
    let current_choices = command_parameter_array(header, 0, location)?;
    if current_choices.len() != mutation.source_lines().len()
        || current_choices
            .iter()
            .zip(mutation.source_lines())
            .any(|(value, expected)| value.as_str() != Some(expected))
    {
        return Err(mutation_failure(
            location,
            WriteBackMutationViolation::FrozenChoicesMismatch,
        ));
    }

    let mut branches = BTreeMap::<usize, usize>::new();
    let mut command_index = key.start_index + 1;
    let mut found_end = false;
    while let Some(command) = list.get(command_index) {
        let code = command_code(command, location)?;
        let indent = command_indent(command, location)?;
        if code == 404 && indent == header_indent {
            found_end = true;
            break;
        }
        if code == 402 && indent == header_indent {
            let parameters = command_parameters(command, location)?;
            let choice_index = parameters
                .first()
                .and_then(Value::as_u64)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| {
                    mutation_failure(location, WriteBackMutationViolation::InvalidChoiceIndex)
                })?;
            let label = parameters.get(1).and_then(Value::as_str).ok_or_else(|| {
                mutation_failure(location, WriteBackMutationViolation::ChoiceLabelNotString)
            })?;
            if mutation
                .source_lines()
                .get(choice_index)
                .map(String::as_str)
                != Some(label)
            {
                return Err(mutation_failure(
                    location,
                    WriteBackMutationViolation::ChoiceLabelMismatch,
                ));
            }
            if branches.insert(choice_index, command_index).is_some() {
                return Err(mutation_failure(
                    location,
                    WriteBackMutationViolation::DuplicateChoiceIndex,
                ));
            }
        }
        command_index += 1;
    }
    if !found_end {
        return Err(mutation_failure(
            location,
            WriteBackMutationViolation::MissingChoiceEnd,
        ));
    }
    if branches.len() != mutation.source_lines().len()
        || (0..mutation.source_lines().len()).any(|index| !branches.contains_key(&index))
    {
        return Err(mutation_failure(
            location,
            WriteBackMutationViolation::IncompleteChoiceCoverage,
        ));
    }

    let mut expected_targets = BTreeSet::new();
    for choice_index in 0..mutation.source_lines().len() {
        expected_targets.insert(choice_value_location(
            source,
            &key.list_steps,
            key.start_index,
            0,
            Some(choice_index),
        ));
        let branch_index = *branches.get(&choice_index).expect("已经验证分支索引完整");
        expected_targets.insert(choice_value_location(
            source,
            &key.list_steps,
            branch_index,
            1,
            None,
        ));
    }
    let actual_targets = mutation
        .recipes()
        .iter()
        .map(|recipe| recipe.target().clone())
        .collect::<BTreeSet<_>>();
    if actual_targets != expected_targets {
        return Err(mutation_failure(
            location,
            WriteBackMutationViolation::ChoiceRecipeTargetMismatch,
        ));
    }

    // 所有冻结事实都已验证；以下只在局部副本中物化，再一次性替换原值。
    let mut rebuilt_header = StackSafeJsonValue::new(clone_value(header));
    let header_choices = command_parameter_array_mut(&mut rebuilt_header, 0, location)?;
    *header_choices = mutation
        .replacement_lines()
        .iter()
        .cloned()
        .map(Value::String)
        .collect();
    let mut rebuilt_branches = Vec::with_capacity(branches.len());
    for (choice_index, command_index) in branches {
        let mut command = StackSafeJsonValue::new(clone_value(&list[command_index]));
        set_command_parameter_text(
            &mut command,
            1,
            &mutation.replacement_lines()[choice_index],
            location,
        )?;
        rebuilt_branches.push((command_index, command));
    }

    drop_value(std::mem::replace(
        &mut list[key.start_index],
        rebuilt_header.into_inner(),
    ));
    for (command_index, command) in rebuilt_branches {
        drop_value(std::mem::replace(
            &mut list[command_index],
            command.into_inner(),
        ));
    }
    documents.mark_document_changed(id);
    Ok(())
}

fn choice_value_location(
    source: &RpgMakerSource,
    list_steps: &[RpgMakerLocationStep],
    command_index: usize,
    parameter_index: usize,
    array_index: Option<usize>,
) -> RpgMakerLocation {
    let mut steps = list_steps.to_vec();
    steps.extend([
        RpgMakerLocationStep::index(command_index),
        RpgMakerLocationStep::key("parameters"),
        RpgMakerLocationStep::index(parameter_index),
    ]);
    if let Some(array_index) = array_index {
        steps.push(RpgMakerLocationStep::index(array_index));
    }
    RpgMakerLocation::value(source.clone(), steps)
}

fn prepare_event_body_replacement(
    list: &[Value],
    key: &StructuralKey,
    mutation: &ReplaceEventBodyMutation,
) -> Result<StructuralReplacement, RpgMakerWriteBackDocumentRewriteFailure> {
    let location = mutation.group_location();
    let (header_code, body_code) = (105, 405);
    let header = list.get(key.start_index).ok_or_else(|| {
        mutation_failure(
            location,
            WriteBackMutationViolation::EventBodyStartOutOfBounds,
        )
    })?;
    if command_code(header, location)? != header_code {
        return Err(mutation_failure(
            location,
            WriteBackMutationViolation::EventBodyCodeMismatch,
        ));
    }

    let body_start = key.start_index + 1;
    let body_end = body_start + mutation.segments().len();
    if body_end > list.len() {
        return Err(mutation_failure(
            location,
            WriteBackMutationViolation::EventBodyTooLong,
        ));
    }
    if let Some(command) = list.get(body_end)
        && command_code(command, location)? == body_code
    {
        return Err(mutation_failure(
            location,
            WriteBackMutationViolation::IncompleteEventBodyCoverage,
        ));
    }

    let originals = &list[body_start..body_end];
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
                WriteBackMutationViolation::FrozenBodyCodeMismatch,
            ));
        }
        let original = command_text(command, body_code, body_code, segment.exact_location())?;
        if original != segment.expected_original() {
            return Err(mutation_failure(
                segment.exact_location(),
                WriteBackMutationViolation::FrozenBodyMismatch,
            ));
        }
        for line in segment.replacement_lines() {
            rebuilt.push(StackSafeJsonValue::new(rewrite_command(
                command,
                body_code,
                line,
                segment.exact_location(),
            )?));
        }
    }
    let mut values = Vec::with_capacity(rebuilt.len() + 1);
    values.push(StackSafeJsonValue::new(clone_value(header)));
    values.extend(rebuilt);
    Ok(StructuralReplacement {
        start: key.start_index,
        end: body_end,
        values,
    })
}

fn prepare_dialogue_replacement(
    list: &[Value],
    key: &StructuralKey,
    mutation: &ReplaceDialogueMutation,
) -> Result<StructuralReplacement, RpgMakerWriteBackDocumentRewriteFailure> {
    let location = mutation.group_location();
    let recipe = mutation.recipe();
    if recipe.group_location() != location {
        return Err(mutation_failure(
            location,
            WriteBackMutationViolation::DialogueRecipeLocationMismatch,
        ));
    }

    let header = list.get(key.start_index).ok_or_else(|| {
        mutation_failure(
            location,
            WriteBackMutationViolation::DialogueStartOutOfBounds,
        )
    })?;
    if command_code(header, location)? != 101 {
        return Err(mutation_failure(
            location,
            WriteBackMutationViolation::DialogueStartNot101,
        ));
    }

    let body_start = key.start_index + 1;
    let body_end = body_start + recipe.lines().len();
    if body_end > list.len() {
        return Err(mutation_failure(
            location,
            WriteBackMutationViolation::DialogueRecipeTooLong,
        ));
    }
    if let Some(command) = list.get(body_end)
        && command_code(command, location)? == 401
    {
        return Err(mutation_failure(
            location,
            WriteBackMutationViolation::IncompleteDialogueCoverage,
        ));
    }

    // 先在局部副本上验证并物化全部目标；任何一步失败都不会修改候选文档。
    let mut rebuilt_header = StackSafeJsonValue::new(clone_value(header));
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
                WriteBackMutationViolation::FrozenSpeakerMismatch,
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
                WriteBackMutationViolation::FrozenDialogueCodeMismatch,
            ));
        }
        let current = command_text(command, 401, 401, line_recipe.physical_location())?;
        if current != line_recipe.expected_raw() {
            return Err(mutation_failure(
                line_recipe.physical_location(),
                WriteBackMutationViolation::FrozenDialogueBodyMismatch,
            ));
        }
    }

    let mut structural_templates = Vec::new();
    let mut body_templates = Vec::new();
    let mut encountered_body = false;
    for (line_recipe, command) in recipe.lines().iter().zip(originals.iter()) {
        let body_index = dialogue_body_index(line_recipe.parts(), line_recipe.physical_location())?;
        match body_index {
            Some(source_line_index) => {
                encountered_body = true;
                body_templates.push((source_line_index, line_recipe, command));
            }
            None if encountered_body => {
                return Err(mutation_failure(
                    line_recipe.physical_location(),
                    WriteBackMutationViolation::StructureAfterBody,
                ));
            }
            None => structural_templates.push((line_recipe, command)),
        }
    }
    body_templates.sort_by_key(|(source_line_index, _, _)| *source_line_index);

    let mut rebuilt_body = Vec::new();
    for (line_recipe, command) in structural_templates {
        let text = render_dialogue_prefix(
            line_recipe.parts(),
            mutation.speaker(),
            line_recipe.physical_location(),
        )?;
        rebuilt_body.push(StackSafeJsonValue::new(rewrite_command(
            command,
            401,
            &text,
            line_recipe.physical_location(),
        )?));
    }

    if let Some(lines) = mutation.body_lines() {
        body_templates.last().ok_or_else(|| {
            mutation_failure(
                location,
                WriteBackMutationViolation::TranslationWithoutBodyRecipe,
            )
        })?;
        for (output_index, line) in lines.iter().enumerate() {
            let (_, line_recipe, command) = body_templates
                .get(output_index)
                .unwrap_or_else(|| body_templates.last().expect("已经确认正文模板至少有一项"));
            let mut text = String::new();
            if output_index == 0 {
                text.push_str(&render_dialogue_prefix(
                    line_recipe.parts(),
                    mutation.speaker(),
                    line_recipe.physical_location(),
                )?);
            }
            text.push_str(line);
            rebuilt_body.push(StackSafeJsonValue::new(rewrite_command(
                command,
                401,
                &text,
                line_recipe.physical_location(),
            )?));
        }
    } else {
        for (_, line_recipe, command) in body_templates {
            let source_prefix = render_dialogue_prefix(
                line_recipe.parts(),
                mutation.source_speaker(),
                line_recipe.physical_location(),
            )?;
            let body = line_recipe
                .expected_raw()
                .strip_prefix(&source_prefix)
                .ok_or_else(|| {
                    mutation_failure(
                        line_recipe.physical_location(),
                        WriteBackMutationViolation::FrozenBodyMissingSpeakerShell,
                    )
                })?;
            let mut text = render_dialogue_prefix(
                line_recipe.parts(),
                mutation.speaker(),
                line_recipe.physical_location(),
            )?;
            text.push_str(body);
            rebuilt_body.push(StackSafeJsonValue::new(rewrite_command(
                command,
                401,
                &text,
                line_recipe.physical_location(),
            )?));
        }
    }

    let mut values = Vec::with_capacity(rebuilt_body.len() + 1);
    values.push(rebuilt_header);
    values.extend(rebuilt_body);
    Ok(StructuralReplacement {
        start: key.start_index,
        end: body_end,
        values,
    })
}

fn dialogue_body_index(
    parts: &[DialogueLinePart],
    location: &RpgMakerLocation,
) -> Result<Option<usize>, RpgMakerWriteBackDocumentRewriteFailure> {
    let body_position = parts
        .iter()
        .position(|part| matches!(part, DialogueLinePart::BodyLine { .. }));
    if body_position.is_some_and(|index| index + 1 != parts.len()) {
        return Err(mutation_failure(
            location,
            WriteBackMutationViolation::BodyLineNotLast,
        ));
    }
    Ok(body_position.map(|position| {
        let DialogueLinePart::BodyLine { source_line_index } = parts[position] else {
            unreachable!()
        };
        source_line_index
    }))
}

fn render_dialogue_prefix(
    parts: &[DialogueLinePart],
    speaker: Option<&str>,
    location: &RpgMakerLocation,
) -> Result<String, RpgMakerWriteBackDocumentRewriteFailure> {
    let mut prefix = String::new();
    for part in parts {
        match part {
            DialogueLinePart::Literal(value) => prefix.push_str(value),
            DialogueLinePart::SpeakerSlot => prefix.push_str(speaker.ok_or_else(|| {
                mutation_failure(location, WriteBackMutationViolation::MissingEmbeddedSpeaker)
            })?),
            DialogueLinePart::BodyLine { .. } => break,
        }
    }
    Ok(prefix)
}

fn validate_dialogue_parameter_location(
    location: &RpgMakerLocation,
    source: &RpgMakerSource,
    list_steps: &[RpgMakerLocationStep],
    command_index: usize,
    parameter_index: usize,
) -> Result<(), RpgMakerWriteBackDocumentRewriteFailure> {
    let target_source = location.source();
    let steps = location.steps();
    let mut expected = list_steps.to_vec();
    expected.extend([
        RpgMakerLocationStep::index(command_index),
        RpgMakerLocationStep::key("parameters"),
        RpgMakerLocationStep::index(parameter_index),
    ]);
    if target_source != source || steps != expected {
        return Err(mutation_failure(
            location,
            WriteBackMutationViolation::DialogueParameterOutsideBlock,
        ));
    }
    Ok(())
}

fn validate_event_segment_location(
    location: &RpgMakerLocation,
    source: &RpgMakerSource,
    list_steps: &[RpgMakerLocationStep],
    command_index: usize,
) -> Result<(), RpgMakerWriteBackDocumentRewriteFailure> {
    let segment_source = location.source();
    let steps = location.steps();
    let mut expected = list_steps.to_vec();
    expected.extend([
        RpgMakerLocationStep::index(command_index),
        RpgMakerLocationStep::key("parameters"),
        RpgMakerLocationStep::index(0),
    ]);
    if segment_source != source || steps != expected {
        return Err(mutation_failure(
            location,
            WriteBackMutationViolation::EventBodySegmentOutsideBlock,
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
        .ok_or_else(|| mutation_failure(location, WriteBackMutationViolation::EventListNotArray))
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
                .ok_or_else(|| {
                    mutation_failure(
                        location,
                        WriteBackMutationViolation::StructuralObjectFieldMissing,
                    )
                })?,
            RpgMakerLocationStep::ArrayIndex(index) => value
                .as_array_mut()
                .and_then(|array| array.get_mut(*index))
                .ok_or_else(|| {
                    mutation_failure(
                        location,
                        WriteBackMutationViolation::StructuralArrayIndexOutOfBounds,
                    )
                })?,
            RpgMakerLocationStep::DecodeJsonString => {
                return Err(mutation_failure(
                    location,
                    WriteBackMutationViolation::DecodeBoundaryInStructuralPath,
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
        .ok_or_else(|| {
            mutation_failure(
                location,
                WriteBackMutationViolation::CommandCodeMissingOrInvalid,
            )
        })
}

fn command_indent(
    command: &Value,
    location: &RpgMakerLocation,
) -> Result<i64, RpgMakerWriteBackDocumentRewriteFailure> {
    command
        .as_object()
        .and_then(|object| object.get("indent"))
        .and_then(Value::as_i64)
        .ok_or_else(|| {
            mutation_failure(
                location,
                WriteBackMutationViolation::CommandIndentMissingOrInvalid,
            )
        })
}

fn command_parameters<'a>(
    command: &'a Value,
    location: &RpgMakerLocation,
) -> Result<&'a [Value], RpgMakerWriteBackDocumentRewriteFailure> {
    command
        .as_object()
        .and_then(|object| object.get("parameters"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| {
            mutation_failure(
                location,
                WriteBackMutationViolation::CommandParametersMissing,
            )
        })
}

fn command_parameter_array<'a>(
    command: &'a Value,
    parameter_index: usize,
    location: &RpgMakerLocation,
) -> Result<&'a Vec<Value>, RpgMakerWriteBackDocumentRewriteFailure> {
    command_parameters(command, location)?
        .get(parameter_index)
        .and_then(Value::as_array)
        .ok_or_else(|| {
            mutation_failure(
                location,
                WriteBackMutationViolation::CommandParameterNotArray,
            )
        })
}

fn command_parameter_array_mut<'a>(
    command: &'a mut Value,
    parameter_index: usize,
    location: &RpgMakerLocation,
) -> Result<&'a mut Vec<Value>, RpgMakerWriteBackDocumentRewriteFailure> {
    command
        .as_object_mut()
        .and_then(|object| object.get_mut("parameters"))
        .and_then(Value::as_array_mut)
        .and_then(|parameters| parameters.get_mut(parameter_index))
        .and_then(Value::as_array_mut)
        .ok_or_else(|| {
            mutation_failure(
                location,
                WriteBackMutationViolation::CommandParameterNotArray,
            )
        })
}

fn command_text<'a>(
    command: &'a Value,
    first_code: i64,
    continuation_code: i64,
    location: &RpgMakerLocation,
) -> Result<&'a str, RpgMakerWriteBackDocumentRewriteFailure> {
    let code = command_code(command, location)?;
    if code != first_code && code != continuation_code {
        return Err(mutation_failure(
            location,
            WriteBackMutationViolation::CommandCodeOutsideTextBlock,
        ));
    }
    command
        .as_object()
        .and_then(|object| object.get("parameters"))
        .and_then(Value::as_array)
        .and_then(|parameters| parameters.first())
        .and_then(Value::as_str)
        .ok_or_else(|| mutation_failure(location, WriteBackMutationViolation::CommandTextMissing))
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
        .ok_or_else(|| {
            mutation_failure(
                location,
                WriteBackMutationViolation::CommandParameterNotString,
            )
        })
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
        .ok_or_else(|| {
            mutation_failure(
                location,
                WriteBackMutationViolation::CommandParameterMissing,
            )
        })?;
    if !parameter.is_string() {
        return Err(mutation_failure(
            location,
            WriteBackMutationViolation::CommandParameterNotString,
        ));
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
    let mut command = StackSafeJsonValue::new(clone_value(template));
    let object = command.as_object_mut().ok_or_else(|| {
        mutation_failure(
            location,
            WriteBackMutationViolation::CommandTemplateNotObject,
        )
    })?;
    object.insert("code".to_owned(), Value::from(code));
    let parameters = object
        .get_mut("parameters")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| {
            mutation_failure(
                location,
                WriteBackMutationViolation::CommandTemplateParametersMissing,
            )
        })?;
    let first = parameters.first_mut().ok_or_else(|| {
        mutation_failure(
            location,
            WriteBackMutationViolation::CommandTemplateTextMissing,
        )
    })?;
    *first = Value::String(text.to_owned());
    Ok(command.into_inner())
}

fn serialize_rewritten_files(
    mut documents: MutableDocuments,
) -> Result<Vec<RpgMakerRewrittenFile>, RpgMakerWriteBackDocumentRewriteFailure> {
    let mut files = Vec::new();
    for id in documents.changed_documents {
        let value = documents.documents.remove(&id).ok_or_else(|| {
            RpgMakerWriteBackDocumentRewriteFailure::MissingChangedDocument { id: id.clone() }
        })?;
        let relative_path = relative_document_path(&id);
        let mut bytes = encode_json_pretty_bytes(&value).map_err(|source| {
            RpgMakerWriteBackDocumentRewriteFailure::SerializeDocument {
                path: relative_path.clone(),
                source,
            }
        })?;
        bytes.push(b'\n');
        files.push(RpgMakerRewrittenFile::new(relative_path, bytes)?);
    }
    if documents.plugins_changed {
        let mut values =
            StackSafeJsonValue::new(Value::Array(Vec::with_capacity(documents.plugins.len())));
        for (expected_index, (stored_index, fields)) in documents.plugins.into_iter().enumerate() {
            if stored_index != expected_index {
                return Err(
                    RpgMakerWriteBackDocumentRewriteFailure::InvalidPluginOrder {
                        expected_index,
                        stored_index,
                    },
                );
            }
            values
                .as_array_mut()
                .expect("plugins.js 候选根始终是 array")
                .push(fields.into_inner());
        }
        let path = PathBuf::from("js").join("plugins.js");
        let json = encode_json_pretty(&values).map_err(|source| {
            RpgMakerWriteBackDocumentRewriteFailure::SerializeDocument {
                path: path.clone(),
                source,
            }
        })?;
        let mut bytes = Vec::with_capacity(
            documents.plugins_prefix.len() + "var $plugins = ;\n".len() + json.len(),
        );
        bytes.extend_from_slice(documents.plugins_prefix.as_bytes());
        bytes.extend_from_slice(b"var $plugins = ");
        bytes.extend_from_slice(json.as_bytes());
        bytes.extend_from_slice(b";\n");
        files.push(RpgMakerRewrittenFile::new(path, bytes)?);
    }
    Ok(files)
}

fn relative_document_path(id: &RpgMakerDocumentId) -> PathBuf {
    match id {
        RpgMakerDocumentId::Data(file) => PathBuf::from("data").join(file.file_name()),
        RpgMakerDocumentId::DataFile(file) => PathBuf::from("data").join(file.as_str()),
        RpgMakerDocumentId::Map(map_id) => PathBuf::from("data").join(map_id.file_name()),
    }
}

fn mutation_failure(
    location: &RpgMakerLocation,
    violation: WriteBackMutationViolation,
) -> RpgMakerWriteBackDocumentRewriteFailure {
    RpgMakerWriteBackDocumentRewriteFailure::InvalidMutation {
        location: Box::new(location.clone()),
        violation,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WriteBackMutationViolation {
    PluginParameterAsDocument,
    RequestedDocumentMissing,
    CrossDocumentMutation,
    DuplicateStructuralTarget,
    DecodeBoundaryInEventContainer,
    EventListNotArray,
    OverlappingFrozenRanges,
    DecodeTargetNotString,
    PluginIndexMissing,
    PluginIndexMismatch,
    PluginNameMismatch,
    PluginParametersNotObject,
    PluginParameterMissing,
    TargetNotString,
    ExpectedOriginalMismatch,
    ObjectPathMissingOrWrongType,
    ArrayPathMissingOrWrongType,
    UnexpectedDecodeBoundary,
    MissingCommandArrayIndex,
    CommandPathNotArrayIndex,
    ChoiceStartOutOfBounds,
    ChoiceStartNot102,
    FrozenChoicesMismatch,
    InvalidChoiceIndex,
    ChoiceLabelNotString,
    ChoiceLabelMismatch,
    DuplicateChoiceIndex,
    MissingChoiceEnd,
    IncompleteChoiceCoverage,
    ChoiceRecipeTargetMismatch,
    EventBodyStartOutOfBounds,
    EventBodyCodeMismatch,
    EventBodyTooLong,
    IncompleteEventBodyCoverage,
    FrozenBodyCodeMismatch,
    FrozenBodyMismatch,
    DialogueRecipeLocationMismatch,
    DialogueStartOutOfBounds,
    DialogueStartNot101,
    DialogueRecipeTooLong,
    IncompleteDialogueCoverage,
    FrozenSpeakerMismatch,
    FrozenDialogueCodeMismatch,
    FrozenDialogueBodyMismatch,
    StructureAfterBody,
    TranslationWithoutBodyRecipe,
    FrozenBodyMissingSpeakerShell,
    BodyLineNotLast,
    MissingEmbeddedSpeaker,
    DialogueParameterOutsideBlock,
    EventBodySegmentOutsideBlock,
    StructuralObjectFieldMissing,
    StructuralArrayIndexOutOfBounds,
    DecodeBoundaryInStructuralPath,
    CommandCodeMissingOrInvalid,
    CommandIndentMissingOrInvalid,
    CommandParametersMissing,
    CommandParameterNotArray,
    CommandCodeOutsideTextBlock,
    CommandTextMissing,
    CommandParameterNotString,
    CommandParameterMissing,
    CommandTemplateNotObject,
    CommandTemplateParametersMissing,
    CommandTemplateTextMissing,
}

impl fmt::Display for WriteBackMutationViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::PluginParameterAsDocument => "插件参数位置不能作为 RPG Maker JSON 文档地址",
            Self::RequestedDocumentMissing => "文档读取器没有返回 Mutation 请求的文档",
            Self::CrossDocumentMutation => "一项原子 Mutation 跨越了不同物理文档",
            Self::DuplicateStructuralTarget => "两个结构修改指向同一冻结事件命令",
            Self::DecodeBoundaryInEventContainer => "事件容器路径不能包含 DecodeJsonString",
            Self::EventListNotArray => "事件 list 位置不是数组",
            Self::OverlappingFrozenRanges => "两个结构修改的冻结命令范围发生重叠",
            Self::DecodeTargetNotString => "DecodeJsonString 的目标不是字符串",
            Self::PluginIndexMissing => "plugins.js 中不存在指定插件索引",
            Self::PluginIndexMismatch => "插件记录索引与数组位置不一致",
            Self::PluginNameMismatch => "插件索引处的 name 与结构化位置不一致",
            Self::PluginParametersNotObject => "插件记录的 parameters 不是对象",
            Self::PluginParameterMissing => "插件参数名在指定插件记录中不存在",
            Self::TargetNotString => "目标值不是字符串",
            Self::ExpectedOriginalMismatch => "目标字符串与 expected_original 不一致",
            Self::ObjectPathMissingOrWrongType => "对象路径不存在或父值不是对象",
            Self::ArrayPathMissingOrWrongType => "数组路径越界或父值不是数组",
            Self::UnexpectedDecodeBoundary => "普通路径片段意外包含 DecodeJsonString",
            Self::MissingCommandArrayIndex => "事件命令位置缺少数组索引",
            Self::CommandPathNotArrayIndex => "事件命令位置末步不是数组索引",
            Self::ChoiceStartOutOfBounds => "选项起始命令索引越界",
            Self::ChoiceStartNot102 => "选项起始命令不是 102",
            Self::FrozenChoicesMismatch => "冻结 102.parameters[0] 与选项原文不一致",
            Self::InvalidChoiceIndex => "同层 402.parameters[0] 不是有效选项索引",
            Self::ChoiceLabelNotString => "同层 402.parameters[1] 不是字符串",
            Self::ChoiceLabelMismatch => "同层 402 标签与冻结 102 选项不一致",
            Self::DuplicateChoiceIndex => "同层 402 重复选项索引",
            Self::MissingChoiceEnd => "选项块缺少同层 404 结束命令",
            Self::IncompleteChoiceCoverage => "选项块没有完整覆盖全部同层 402",
            Self::ChoiceRecipeTargetMismatch => "选项配方目标与冻结 102/402 块不一致",
            Self::EventBodyStartOutOfBounds => "事件正文起始命令索引越界",
            Self::EventBodyCodeMismatch => "事件正文起始命令码与正文类型不一致",
            Self::EventBodyTooLong => "事件正文段数超过冻结命令列表",
            Self::IncompleteEventBodyCoverage => "Mutation 没有覆盖完整冻结事件正文块",
            Self::FrozenBodyCodeMismatch => "冻结正文命令码与 Mutation 类型不一致",
            Self::FrozenBodyMismatch => "冻结正文与 expected_original 不一致",
            Self::DialogueRecipeLocationMismatch => "对话 Mutation 与物化配方的组位置不一致",
            Self::DialogueStartOutOfBounds => "对话起始命令索引越界",
            Self::DialogueStartNot101 => "对话起始命令不是 101",
            Self::DialogueRecipeTooLong => "对话配方段数超过冻结命令列表",
            Self::IncompleteDialogueCoverage => "对话配方没有覆盖完整 401 块",
            Self::FrozenSpeakerMismatch => "冻结 Speaker 与 expected_raw 不一致",
            Self::FrozenDialogueCodeMismatch => "冻结对话正文命令不是 401",
            Self::FrozenDialogueBodyMismatch => "冻结对话正文与 expected_raw 不一致",
            Self::StructureAfterBody => "对话结构行不能位于正文行之后",
            Self::TranslationWithoutBodyRecipe => "对话 Mutation 提供正文译文但配方没有 BodyLine",
            Self::FrozenBodyMissingSpeakerShell => "冻结对话正文不以物化 Speaker shell 开头",
            Self::BodyLineNotLast => "对话 BodyLine 必须是物理行的最后一部分",
            Self::MissingEmbeddedSpeaker => "内嵌 SpeakerSlot 缺少 Speaker",
            Self::DialogueParameterOutsideBlock => "对话参数位置不属于物化的冻结块",
            Self::EventBodySegmentOutsideBlock => "事件正文段位置不属于指定冻结正文块",
            Self::StructuralObjectFieldMissing => "结构路径对象字段不存在",
            Self::StructuralArrayIndexOutOfBounds => "结构路径数组索引越界",
            Self::DecodeBoundaryInStructuralPath => "事件或标签容器路径不能包含 DecodeJsonString",
            Self::CommandCodeMissingOrInvalid => "事件命令不是带整数 code 的对象",
            Self::CommandIndentMissingOrInvalid => "事件命令不是带整数 indent 的对象",
            Self::CommandParametersMissing => "事件命令缺少 parameters 数组",
            Self::CommandParameterNotArray => "事件命令目标参数不是数组",
            Self::CommandCodeOutsideTextBlock => "事件命令码不属于目标文本块",
            Self::CommandTextMissing => "事件文本命令缺少字符串 parameters[0]",
            Self::CommandParameterNotString => "事件命令目标参数不是字符串",
            Self::CommandParameterMissing => "事件命令目标参数不存在",
            Self::CommandTemplateNotObject => "事件命令模板不是对象",
            Self::CommandTemplateParametersMissing => "事件命令模板缺少 parameters 数组",
            Self::CommandTemplateTextMissing => "事件命令模板缺少 parameters[0]",
        })
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

impl<R> RpgMakerWriteBackDocumentRewritingError<R, CpuExecutorUnavailable>
where
    R: RpgMakerProjectDocumentReadingDiagnostic + Error + Send + Sync + 'static,
{
    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        match self {
            Self::ReadDocuments(source) => {
                source.document_reading_diagnostic_report(RpgMakerDocumentConsumer::WriteBack)
            }
            Self::ScheduleRewrite(source) => rewrite_compute_report(source),
            Self::Rewrite(source) => source.diagnostic_report(),
        }
    }

    pub(crate) fn into_reported_failure(self) -> ReportedFailure {
        let report = self.diagnostic_report();
        ReportedFailure::new(report, self)
    }
}

/// 已读取文档与 Mutation Plan 无法共同建立完整候选。
#[derive(Debug)]
pub(crate) enum RpgMakerWriteBackDocumentRewriteFailure {
    InvalidMutation {
        location: Box<RpgMakerLocation>,
        violation: WriteBackMutationViolation,
    },
    DecodeNestedJson {
        location: Box<RpgMakerLocation>,
        source: StackSafeJsonError,
    },
    EncodeNestedJson {
        location: Box<RpgMakerLocation>,
        source: StackSafeJsonError,
    },
    SerializeDocument {
        path: PathBuf,
        source: StackSafeJsonError,
    },
    InvalidOutputPath {
        path: PathBuf,
    },
    DuplicateOutputPath {
        path: PathBuf,
    },
    OutputPathCaseKey {
        path: PathBuf,
        source: WindowsOrdinalCaseKeyError,
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
            Self::InvalidMutation {
                location,
                violation,
            } => {
                write!(formatter, "Mutation {location} 无法应用：{violation}")
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
            Self::OutputPathCaseKey { path, source } => write!(
                formatter,
                "无法建立候选文件路径 {} 的 Windows 非大小写身份：{source}",
                path.display()
            ),
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
            Self::OutputPathCaseKey { source, .. } => Some(source),
            Self::InvalidMutation { .. }
            | Self::InvalidOutputPath { .. }
            | Self::DuplicateOutputPath { .. }
            | Self::MissingChangedDocument { .. }
            | Self::InvalidPluginOrder { .. } => None,
        }
    }
}

impl RpgMakerWriteBackDocumentRewriteFailure {
    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        let problem = match self {
            Self::InvalidMutation {
                location,
                violation,
            } => RpgMakerWriteBackDocumentRewriteProblem::InvalidMutation {
                location: location.diagnostic_location(),
                violation: diagnostic_mutation_violation(*violation),
            },
            Self::DecodeNestedJson { location, source } => {
                RpgMakerWriteBackDocumentRewriteProblem::DecodeNestedJson {
                    location: location.diagnostic_location(),
                    category: rewrite_json_failure(source),
                    line: source.line(),
                    column: source.column(),
                }
            }
            Self::EncodeNestedJson { location, source } => {
                RpgMakerWriteBackDocumentRewriteProblem::EncodeNestedJson {
                    location: location.diagnostic_location(),
                    category: rewrite_json_failure(source),
                    line: source.line(),
                    column: source.column(),
                }
            }
            Self::SerializeDocument { path, source } => {
                RpgMakerWriteBackDocumentRewriteProblem::SerializeDocument {
                    path: SafePath::new(path),
                    category: rewrite_json_failure(source),
                    line: source.line(),
                    column: source.column(),
                }
            }
            Self::InvalidOutputPath { path } => {
                RpgMakerWriteBackDocumentRewriteProblem::InvalidOutputPath {
                    path: SafePath::new(path),
                }
            }
            Self::DuplicateOutputPath { path } => {
                RpgMakerWriteBackDocumentRewriteProblem::DuplicateOutputPath {
                    path: SafePath::new(path),
                }
            }
            Self::OutputPathCaseKey { path, source } => match source {
                WindowsOrdinalCaseKeyError::InputTooLarge { maximum, observed } => {
                    RpgMakerWriteBackDocumentRewriteProblem::OrdinalCaseKeyInputTooLarge {
                        path: SafePath::new(path),
                        observed: *observed,
                        maximum: *maximum,
                    }
                }
                WindowsOrdinalCaseKeyError::WindowsApi { phase, source } => {
                    RpgMakerWriteBackDocumentRewriteProblem::OrdinalCaseKeyIo {
                        path: SafePath::new(path),
                        phase: match phase {
                            crate::windows_path::WindowsOrdinalCaseKeyPhase::Measure => {
                                FileSystemOrdinalKeyPhase::Measure
                            }
                            crate::windows_path::WindowsOrdinalCaseKeyPhase::Map => {
                                FileSystemOrdinalKeyPhase::Map
                            }
                        },
                        failure: IoFailure::from_error(source),
                    }
                }
            },
            Self::MissingChangedDocument { id } => {
                RpgMakerWriteBackDocumentRewriteProblem::MissingChangedDocument {
                    path: SafePath::new(relative_document_path(id)),
                }
            }
            Self::InvalidPluginOrder {
                expected_index,
                stored_index,
            } => RpgMakerWriteBackDocumentRewriteProblem::PluginOrderMismatch {
                expected_index: *expected_index,
                stored_index: *stored_index,
            },
        };
        rewrite_report(problem)
    }
}

fn rewrite_report(problem: RpgMakerWriteBackDocumentRewriteProblem) -> DiagnosticReport {
    DiagnosticReport::new(
        StateEffect::Unchanged,
        Diagnostic::rpg_maker(RpgMakerIssue::write_back_document_rewrite(problem)),
    )
}

fn rewrite_compute_report(
    source: &CpuTaskExecutionError<CpuExecutorUnavailable>,
) -> DiagnosticReport {
    let failure = match source {
        CpuTaskExecutionError::Cancelled => RpgMakerComputeFailure::Cancelled,
        CpuTaskExecutionError::Unavailable(CpuExecutorUnavailable::ShuttingDown) => {
            RpgMakerComputeFailure::ExecutorClosed
        }
        CpuTaskExecutionError::Unavailable(CpuExecutorUnavailable::StatePoisoned) => {
            RpgMakerComputeFailure::StatePoisoned
        }
        CpuTaskExecutionError::TaskPanicked => RpgMakerComputeFailure::WorkerPanicked,
    };
    rewrite_report(RpgMakerWriteBackDocumentRewriteProblem::RewriteCompute { failure })
}

fn rewrite_json_failure(source: &StackSafeJsonError) -> RpgMakerJsonFailureKind {
    match source.diagnostic_category() {
        crate::json_diagnostic::JsonErrorCategory::Io => RpgMakerJsonFailureKind::Io,
        crate::json_diagnostic::JsonErrorCategory::Syntax => RpgMakerJsonFailureKind::Syntax,
        crate::json_diagnostic::JsonErrorCategory::Data => RpgMakerJsonFailureKind::Data,
        crate::json_diagnostic::JsonErrorCategory::Eof => RpgMakerJsonFailureKind::Eof,
        crate::json_diagnostic::JsonErrorCategory::DuplicateObjectKey => {
            RpgMakerJsonFailureKind::DuplicateObjectKey
        }
    }
}

macro_rules! map_mutation_violation {
    ($value:expr; $($variant:ident),+ $(,)?) => {
        match $value {
            $(WriteBackMutationViolation::$variant =>
                RpgMakerWriteBackMutationViolation::$variant,)+
        }
    };
}

fn diagnostic_mutation_violation(
    violation: WriteBackMutationViolation,
) -> RpgMakerWriteBackMutationViolation {
    map_mutation_violation!(
        violation;
        PluginParameterAsDocument,
        RequestedDocumentMissing,
        CrossDocumentMutation,
        DuplicateStructuralTarget,
        DecodeBoundaryInEventContainer,
        EventListNotArray,
        OverlappingFrozenRanges,
        DecodeTargetNotString,
        PluginIndexMissing,
        PluginIndexMismatch,
        PluginNameMismatch,
        PluginParametersNotObject,
        PluginParameterMissing,
        TargetNotString,
        ExpectedOriginalMismatch,
        ObjectPathMissingOrWrongType,
        ArrayPathMissingOrWrongType,
        UnexpectedDecodeBoundary,
        MissingCommandArrayIndex,
        CommandPathNotArrayIndex,
        ChoiceStartOutOfBounds,
        ChoiceStartNot102,
        FrozenChoicesMismatch,
        InvalidChoiceIndex,
        ChoiceLabelNotString,
        ChoiceLabelMismatch,
        DuplicateChoiceIndex,
        MissingChoiceEnd,
        IncompleteChoiceCoverage,
        ChoiceRecipeTargetMismatch,
        EventBodyStartOutOfBounds,
        EventBodyCodeMismatch,
        EventBodyTooLong,
        IncompleteEventBodyCoverage,
        FrozenBodyCodeMismatch,
        FrozenBodyMismatch,
        DialogueRecipeLocationMismatch,
        DialogueStartOutOfBounds,
        DialogueStartNot101,
        DialogueRecipeTooLong,
        IncompleteDialogueCoverage,
        FrozenSpeakerMismatch,
        FrozenDialogueCodeMismatch,
        FrozenDialogueBodyMismatch,
        StructureAfterBody,
        TranslationWithoutBodyRecipe,
        FrozenBodyMissingSpeakerShell,
        BodyLineNotLast,
        MissingEmbeddedSpeaker,
        DialogueParameterOutsideBlock,
        EventBodySegmentOutsideBlock,
        StructuralObjectFieldMissing,
        StructuralArrayIndexOutOfBounds,
        DecodeBoundaryInStructuralPath,
        CommandCodeMissingOrInvalid,
        CommandIndentMissingOrInvalid,
        CommandParametersMissing,
        CommandParameterNotArray,
        CommandCodeOutsideTextBlock,
        CommandTextMissing,
        CommandParameterNotString,
        CommandParameterMissing,
        CommandTemplateNotObject,
        CommandTemplateParametersMissing,
        CommandTemplateTextMissing,
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;
    use crate::diagnostic::render_diagnostic_report;
    use crate::i18n::{UiLocale, UiLocalizer};
    use crate::lossless_json::LosslessJsonError;
    use crate::rpg_maker::extract::document::{
        PluginConfiguration, parse_json_document_for_test, parse_plugins_document_for_test,
    };
    use crate::rpg_maker::model::{
        DialogueLinePart, DialogueLineRecipe, DialogueWriteRecipe, DirectSpeakerTarget,
        DirectTextPart, DirectTextRecipe, TextUnitRole,
    };
    use crate::rpg_maker::text::{MapId, StandardDataFile};
    use crate::rpg_maker::write_back::planner::{
        EventBodyMutationSegment, RpgMakerWriteBackMutation,
    };

    #[test]
    fn missing_changed_document_diagnostic_keeps_exact_safe_document_identity() {
        let cases = [
            (
                RpgMakerDocumentId::Data(StandardDataFile::Items),
                PathBuf::from("data").join("Items.json"),
            ),
            (
                RpgMakerDocumentId::DataFile(
                    crate::rpg_maker::text::DataFileName::parse("QuestData.json")
                        .expect("自定义 data 文件名应有效"),
                ),
                PathBuf::from("data").join("QuestData.json"),
            ),
            (
                RpgMakerDocumentId::Map(MapId::new(7).expect("地图 ID 应有效")),
                PathBuf::from("data").join("Map007.json"),
            ),
        ];

        for (id, expected_path) in cases {
            let report = RpgMakerWriteBackDocumentRewriteFailure::MissingChangedDocument { id }
                .diagnostic_report();
            assert_eq!(
                report.primary().code(),
                "rpg_maker.write_back.rewrite.missing_changed_document"
            );
            assert_eq!(report.effect(), StateEffect::Unchanged);
            assert_eq!(
                serde_json::to_value(&report).expect("写回诊断应可序列化")["primary"]["issue"]["details"]
                    ["problem"]["problem"]["path"],
                serde_json::json!(expected_path)
            );
        }
    }

    #[test]
    fn nested_json_diagnostics_distinguish_operations_without_copying_json_body() {
        const JSON_BODY: &str = "SENTINEL_REWRITER_JSON_BODY_6e92";

        let location = RpgMakerLocation::value(
            crate::rpg_maker::text::RpgMakerSource::map(3),
            vec![crate::rpg_maker::text::RpgMakerLocationStep::key(
                "parameters",
            )],
        );
        let backend_error = || {
            let source = format!("{{\"{JSON_BODY}\":");
            StackSafeJsonError::Backend(
                serde_json::from_str::<serde_json::Value>(&source).expect_err("测试 JSON 应不完整"),
            )
        };
        let cases = [
            (
                RpgMakerWriteBackDocumentRewriteFailure::DecodeNestedJson {
                    location: Box::new(location.clone()),
                    source: backend_error(),
                },
                "rpg_maker.write_back.rewrite.decode_nested_json",
                &["\"category\":\"eof\"", "\"line\":1", "\"column\":"][..],
                "map:3:key:parameters",
            ),
            (
                RpgMakerWriteBackDocumentRewriteFailure::EncodeNestedJson {
                    location: Box::new(location),
                    source: StackSafeJsonError::Syntax {
                        source: LosslessJsonError::DuplicateObjectKey { byte_offset: 17 },
                        line: 2,
                        column: 9,
                    },
                },
                "rpg_maker.write_back.rewrite.encode_nested_json",
                &[
                    "\"category\":\"duplicate_object_key\"",
                    "\"line\":2",
                    "\"column\":9",
                ][..],
                "map:3:key:parameters",
            ),
            (
                RpgMakerWriteBackDocumentRewriteFailure::SerializeDocument {
                    path: PathBuf::from("data/Items.json"),
                    source: backend_error(),
                },
                "rpg_maker.write_back.rewrite.serialize_document",
                &["\"kind\":\"serialize_document\"", "\"category\":\"eof\""][..],
                "data/Items.json",
            ),
        ];

        for (source, expected_code, expected_json_facts, expected_cli_object) in cases {
            let report = source.diagnostic_report();
            assert_eq!(report.primary().code(), expected_code);
            let json = serde_json::to_string(&report).expect("安全诊断应可序列化");
            let cli =
                render_diagnostic_report(&report, &UiLocalizer::new(UiLocale::SimplifiedChinese));
            assert!(
                !json.contains(JSON_BODY),
                "JSONL 不应复制 JSON 正文：{json}"
            );
            assert!(!cli.contains(JSON_BODY), "CLI 不应复制 JSON 正文：{cli}");
            for fact in expected_json_facts {
                assert!(json.contains(fact), "JSONL 缺少 {fact}: {json}");
            }
            assert!(
                cli.contains(expected_cli_object),
                "CLI 缺少可读对象 {expected_cli_object}: {cli}"
            );
            for internal in ["json_category=", "line=", "column=", expected_code] {
                assert!(
                    !cli.contains(internal),
                    "CLI 不得显示内部诊断字段 {internal}: {cli}"
                );
            }
        }
    }

    #[derive(Clone, Default)]
    struct RecordingProgress(Arc<Mutex<Vec<ProgressSnapshot<WriteBackProgressPhase>>>>);

    impl ProgressObserver<WriteBackProgressPhase> for RecordingProgress {
        fn observe(&self, snapshot: ProgressSnapshot<WriteBackProgressPhase>) {
            self.0.lock().expect("进度记录锁不应中毒").push(snapshot);
        }
    }

    impl RecordingProgress {
        fn snapshots(&self) -> Vec<ProgressSnapshot<WriteBackProgressPhase>> {
            self.0.lock().expect("进度记录锁不应中毒").clone()
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct FakeError;

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fake")
        }
    }

    impl Error for FakeError {}

    impl RpgMakerProjectDocumentReadingDiagnostic for FakeError {
        fn document_reading_diagnostic_report(
            &self,
            consumer: RpgMakerDocumentConsumer,
        ) -> DiagnosticReport {
            assert_eq!(consumer, RpgMakerDocumentConsumer::WriteBack);
            rewrite_report(
                RpgMakerWriteBackDocumentRewriteProblem::MissingChangedDocument {
                    path: SafePath::new("fake_write_back_document_reader"),
                },
            )
        }
    }

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

    struct InlineCpu;

    impl CpuTaskExecutor for InlineCpu {
        type Error = FakeError;

        async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            Ok(task())
        }
    }

    #[test]
    fn document_read_failure_uses_explicit_write_back_responsibility() {
        let error: RpgMakerWriteBackDocumentRewritingError<
            FakeError,
            crate::runtime::cpu::CpuExecutorUnavailable,
        > = RpgMakerWriteBackDocumentRewritingError::ReadDocuments(FakeError);

        let report = error.diagnostic_report();

        assert_eq!(
            report.primary().code(),
            "rpg_maker.write_back.rewrite.missing_changed_document"
        );
        assert_eq!(
            report.primary().stage(),
            crate::diagnostic::DiagnosticStage::WriteBack
        );
    }

    #[tokio::test]
    async fn empty_plan_returns_project_bound_empty_candidate_without_dependencies() {
        let project = project();
        let progress = RecordingProgress::default();
        let service = RpgMakerWriteBackDocumentRewritingService::new(PanickingReader, PanickingCpu)
            .with_progress(progress.clone());

        let candidate = service
            .rewrite(&project, RpgMakerWriteBackMutationPlan::empty())
            .await
            .expect("空计划应该直接成功");

        assert_eq!(candidate.project_name(), project.name());
        assert_eq!(candidate.workspace_root(), project.workspace_root());
        assert!(candidate.files().is_empty());
        assert_eq!(
            progress.snapshots(),
            vec![ProgressSnapshot::determinate(
                WriteBackProgressPhase::RewritingDocuments,
                0,
                0,
            )]
        );
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

    #[tokio::test]
    async fn document_progress_counts_only_successfully_rewritten_physical_documents() {
        let project = project();
        let items_source = RpgMakerSource::data(StandardDataFile::Items);
        let actors_source = RpgMakerSource::data(StandardDataFile::Actors);
        let documents = RpgMakerProjectDocuments::new(
            BTreeMap::from([
                (
                    RpgMakerDocumentId::Data(StandardDataFile::Items),
                    json!([null, {"name": "道具"}]),
                ),
                (
                    RpgMakerDocumentId::Data(StandardDataFile::Actors),
                    json!([null, {"name": "角色"}]),
                ),
            ]),
            Vec::new(),
        );
        let progress = RecordingProgress::default();
        let service =
            RpgMakerWriteBackDocumentRewritingService::new(StaticReader(documents), InlineCpu)
                .with_progress(progress.clone());

        service
            .rewrite(
                &project,
                plan(vec![
                    set_text(
                        RpgMakerLocation::value(
                            items_source,
                            vec![
                                RpgMakerLocationStep::index(1),
                                RpgMakerLocationStep::key("name"),
                            ],
                        ),
                        "道具",
                        "Item",
                    ),
                    set_text(
                        RpgMakerLocation::value(
                            actors_source,
                            vec![
                                RpgMakerLocationStep::index(1),
                                RpgMakerLocationStep::key("name"),
                            ],
                        ),
                        "角色",
                        "Actor",
                    ),
                ]),
            )
            .await
            .expect("两个物理文档都应成功改写");

        assert_eq!(
            progress.snapshots(),
            vec![
                ProgressSnapshot::determinate(WriteBackProgressPhase::RewritingDocuments, 0, 2,),
                ProgressSnapshot::determinate(WriteBackProgressPhase::RewritingDocuments, 1, 2,),
                ProgressSnapshot::determinate(WriteBackProgressPhase::RewritingDocuments, 2, 2,),
            ]
        );

        let invalid_documents = RpgMakerProjectDocuments::new(
            BTreeMap::from([(
                RpgMakerDocumentId::Data(StandardDataFile::Items),
                json!([null, {"name": "已变化"}]),
            )]),
            Vec::new(),
        );
        let failed_progress = RecordingProgress::default();
        let failed_service = RpgMakerWriteBackDocumentRewritingService::new(
            StaticReader(invalid_documents),
            InlineCpu,
        )
        .with_progress(failed_progress.clone());

        failed_service
            .rewrite(
                &project,
                plan(vec![set_text(
                    RpgMakerLocation::value(
                        RpgMakerSource::data(StandardDataFile::Items),
                        vec![
                            RpgMakerLocationStep::index(1),
                            RpgMakerLocationStep::key("name"),
                        ],
                    ),
                    "道具",
                    "Item",
                )]),
            )
            .await
            .expect_err("原文不匹配的物理文档必须失败");
        assert_eq!(
            failed_progress.snapshots(),
            vec![ProgressSnapshot::determinate(
                WriteBackProgressPhase::RewritingDocuments,
                0,
                1,
            )]
        );
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

        assert_eq!(
            selection.map_ids(),
            &BTreeSet::from([MapId::new(42).unwrap()])
        );
        assert!(!selection.includes_all_maps());
        assert!(selection.includes_plugins());
    }

    #[test]
    fn rewrites_plain_nested_angle_bracket_and_plugin_values_with_complete_files() {
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
                actor(vec![RpgMakerLocationStep::key("note")]),
                "<Tag:旧一><Tag:旧二>",
                "<Tag:新一><Tag:新二>>",
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
        assert_eq!(actor_json[1]["note"], "<Tag:新一><Tag:新二>>");
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
    fn shared_decoded_containers_are_parsed_and_serialized_once_per_level() {
        let source = RpgMakerSource::data(StandardDataFile::Actors);
        let deep =
            serde_json::to_string(&json!({"a": "一", "b": "二"})).expect("深层测试容器应该可编码");
        let payload = serde_json::to_string(&json!({
            "left": "甲",
            "right": "乙",
            "deep": deep,
        }))
        .expect("外层测试容器应该可编码");
        let documents = RpgMakerProjectDocuments::new(
            BTreeMap::from([(
                RpgMakerDocumentId::Data(StandardDataFile::Actors),
                json!([null, {"payload": payload}]),
            )]),
            Vec::new(),
        );
        let location = |tail: Vec<RpgMakerLocationStep>| {
            let mut steps = vec![
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("payload"),
                RpgMakerLocationStep::DecodeJsonString,
            ];
            steps.extend(tail);
            RpgMakerLocation::value(source.clone(), steps)
        };

        reset_nested_json_codec_counts();
        let candidate = rewrite_documents(
            project_name(),
            workspace_root(),
            documents,
            plan(vec![
                set_text(
                    location(vec![RpgMakerLocationStep::key("left")]),
                    "甲",
                    "左",
                ),
                set_text(
                    location(vec![RpgMakerLocationStep::key("right")]),
                    "乙",
                    "右",
                ),
                set_text(
                    location(vec![
                        RpgMakerLocationStep::key("deep"),
                        RpgMakerLocationStep::DecodeJsonString,
                        RpgMakerLocationStep::key("a"),
                    ]),
                    "一",
                    "壹",
                ),
                set_text(
                    location(vec![
                        RpgMakerLocationStep::key("deep"),
                        RpgMakerLocationStep::DecodeJsonString,
                        RpgMakerLocationStep::key("b"),
                    ]),
                    "二",
                    "贰",
                ),
            ]),
        )
        .expect("共享 decoded container 的全部修改应该成功");

        assert_eq!(
            nested_json_codec_counts(),
            (2, 2),
            "外层和深层容器都只能各解码、编码一次"
        );
        let actors: Value =
            serde_json::from_str(file_text(&candidate, Path::new("data/Actors.json")))
                .expect("Actors 候选应该是 JSON");
        let payload: Value = serde_json::from_str(
            actors[1]["payload"]
                .as_str()
                .expect("payload 应保持 JSON 字符串"),
        )
        .expect("payload 应能重新解码");
        assert_eq!(payload["left"], "左");
        assert_eq!(payload["right"], "右");
        let deep: Value =
            serde_json::from_str(payload["deep"].as_str().expect("deep 应保持 JSON 字符串"))
                .expect("deep 应能重新解码");
        assert_eq!(deep["a"], "壹");
        assert_eq!(deep["b"], "贰");
    }

    #[test]
    fn decoded_container_batching_keeps_the_first_value_failure_by_plan_order() {
        let source = RpgMakerSource::data(StandardDataFile::Actors);
        let payload = serde_json::to_string(&json!({"left": "甲", "right": "乙"}))
            .expect("测试容器应该可编码");
        let documents = RpgMakerProjectDocuments::new(
            BTreeMap::from([(
                RpgMakerDocumentId::Data(StandardDataFile::Actors),
                json!([null, {"plain": "当前", "payload": payload}]),
            )]),
            Vec::new(),
        );
        let plain_location = RpgMakerLocation::value(
            source.clone(),
            vec![
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("plain"),
            ],
        );
        let decoded_location = |key| {
            RpgMakerLocation::value(
                source.clone(),
                vec![
                    RpgMakerLocationStep::index(1),
                    RpgMakerLocationStep::key("payload"),
                    RpgMakerLocationStep::DecodeJsonString,
                    RpgMakerLocationStep::key(key),
                ],
            )
        };
        let plan = plan(vec![
            set_text(decoded_location("left"), "甲", "左"),
            set_text(plain_location.clone(), "漂移的普通原文", "普通译文"),
            set_text(decoded_location("right"), "漂移的容器原文", "右"),
        ]);

        let error = rewrite_documents(project_name(), workspace_root(), documents, plan)
            .expect_err("存在两个验收失败时必须选择计划中较早的普通值错误");

        assert!(matches!(
            error,
            RpgMakerWriteBackDocumentRewriteFailure::InvalidMutation { location, .. }
                if location.as_ref() == &plain_location
        ));
    }

    #[test]
    fn rewrites_complete_plugin_text_through_two_decode_boundaries() {
        let source = RpgMakerSource::data(StandardDataFile::Items);
        let document = json!([
            null,
            {
                "payload": " { \"entry\" : \"{\\\"note\\\":\\\"<Help:旧帮助>\\\",\\\"kept\\\":7}\", \"title\" : \"勇者\" } ",
                "outside": true
            }
        ]);
        let text_steps = vec![
            RpgMakerLocationStep::index(1),
            RpgMakerLocationStep::key("payload"),
            RpgMakerLocationStep::DecodeJsonString,
            RpgMakerLocationStep::key("entry"),
            RpgMakerLocationStep::DecodeJsonString,
            RpgMakerLocationStep::key("note"),
        ];
        let documents = RpgMakerProjectDocuments::new(
            BTreeMap::from([(RpgMakerDocumentId::Data(StandardDataFile::Items), document)]),
            Vec::new(),
        );

        let candidate = rewrite_documents(
            project_name(),
            workspace_root(),
            documents,
            plan(vec![set_text(
                RpgMakerLocation::value(source, text_steps),
                "<Help:旧帮助>",
                "<Help:新帮助>>",
            )]),
        )
        .expect("双层 decoded Value 应可逐字写回");

        let items: Value =
            serde_json::from_str(file_text(&candidate, Path::new("data/Items.json")))
                .expect("Items 候选应为 JSON");
        assert_eq!(
            items[1]["payload"],
            r#"{"entry":"{\"note\":\"<Help:新帮助>>\",\"kept\":7}","title":"勇者"}"#
        );
        assert_eq!(items[1]["outside"], true);
    }

    #[test]
    fn rewrites_complete_108_408_values_inside_decoded_list() {
        let source = RpgMakerSource::map(8);
        let document = json!({
            "payload": " { \"events\" : [{\"list\":[{\"code\":108,\"indent\":2,\"parameters\":[\"<Quest:第一\"],\"unknown\":\"first\"},{\"code\":408,\"indent\":2,\"parameters\":[\"行>\"],\"unknown\":\"continued\"},{\"code\":0,\"indent\":2,\"parameters\":[]}]}], \"kept\" : \"勇者\" } ",
            "outside": 9
        });
        let command_steps = vec![
            RpgMakerLocationStep::key("payload"),
            RpgMakerLocationStep::DecodeJsonString,
            RpgMakerLocationStep::key("events"),
            RpgMakerLocationStep::index(0),
            RpgMakerLocationStep::key("list"),
        ];
        let text_location = |command_index| {
            let mut steps = command_steps.clone();
            steps.extend([
                RpgMakerLocationStep::index(command_index),
                RpgMakerLocationStep::key("parameters"),
                RpgMakerLocationStep::index(0),
            ]);
            RpgMakerLocation::value(source.clone(), steps)
        };
        let documents = RpgMakerProjectDocuments::new(
            BTreeMap::from([(RpgMakerDocumentId::Map(MapId::new(8).unwrap()), document)]),
            Vec::new(),
        );

        let candidate = rewrite_documents(
            project_name(),
            workspace_root(),
            documents,
            plan(vec![
                set_text(text_location(0), "<Quest:第一", "<Quest:已译"),
                set_text(text_location(1), "行>", "完成>>"),
            ]),
        )
        .expect("decoded 108/408 的完整值应可写回");

        let map: Value = serde_json::from_str(file_text(&candidate, Path::new("data/Map008.json")))
            .expect("Map 候选应为 JSON");
        let payload = map["payload"]
            .as_str()
            .expect("外层 payload 应保持 JSON string");
        assert_eq!(
            payload,
            r#"{"events":[{"list":[{"code":108,"indent":2,"parameters":["<Quest:已译"],"unknown":"first"},{"code":408,"indent":2,"parameters":["完成>>"],"unknown":"continued"},{"code":0,"indent":2,"parameters":[]}]}],"kept":"勇者"}"#
        );
        assert_eq!(map["outside"], 9);
    }

    #[test]
    fn failed_deep_validation_does_not_commit_any_decoded_container_level() {
        let location = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Items),
            vec![
                RpgMakerLocationStep::key("payload"),
                RpgMakerLocationStep::DecodeJsonString,
                RpgMakerLocationStep::key("entry"),
                RpgMakerLocationStep::DecodeJsonString,
                RpgMakerLocationStep::key("note"),
            ],
        );
        let mut document = json!({
            "payload": " { \"entry\" : \"{\\\"note\\\":\\\"<Help:原文>\\\",\\\"kept\\\":7}\", \"outerKept\" : true } "
        });
        let original = document.clone();
        let error = edit_value_at_steps(&mut document, location.steps(), &location, |value| {
            let text = value.as_str().ok_or_else(|| {
                mutation_failure(&location, WriteBackMutationViolation::TargetNotString)
            })?;
            if text != "<Help:漂移原文>" {
                return Err(mutation_failure(
                    &location,
                    WriteBackMutationViolation::ExpectedOriginalMismatch,
                ));
            }
            Ok(())
        })
        .expect_err("深层 expected_original 漂移必须失败");

        assert!(matches!(
            error,
            RpgMakerWriteBackDocumentRewriteFailure::InvalidMutation {
                violation: WriteBackMutationViolation::ExpectedOriginalMismatch,
                ..
            }
        ));
        assert_eq!(document, original, "失败不得提交任何一层 decoded string");
    }

    #[test]
    fn applies_value_mutations_before_structural_rebuild() {
        let source = RpgMakerSource::map(7);
        let list_steps = vec![RpgMakerLocationStep::key("list")];
        let document = json!({
            "list": [
                {"code":101,"indent":0,"parameters":["",0,0,2,"莉莉"],"headerUnknown":true},
                {"code":401,"indent":1,"parameters":["甲"],"lineUnknown":"A"},
                {"code":401,"indent":2,"parameters":["乙"],"lineUnknown":"B"},
                {"code":108,"indent":3,"parameters":["<Tag:旧"],"commentUnknown":"first"},
                {"code":408,"indent":3,"parameters":["值><Tag:二>"],"commentUnknown":"continued"},
                {"code":0,"indent":0,"parameters":[]}
            ],
            "unknown": {"kept": true}
        });
        let documents = RpgMakerProjectDocuments::new(
            BTreeMap::from([(RpgMakerDocumentId::Map(MapId::new(7).unwrap()), document)]),
            Vec::new(),
        );
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
                    vec![DialogueLinePart::BodyLine {
                        source_line_index: 0,
                    }],
                )
                .expect("第一行配方应合法"),
                DialogueLineRecipe::new(
                    segment_location(2),
                    "乙",
                    vec![DialogueLinePart::BodyLine {
                        source_line_index: 1,
                    }],
                )
                .expect("第二行配方应合法"),
            ],
        )
        .expect("对话配方应合法");
        let body = ReplaceDialogueMutation::new(
            recipe,
            Some("莉莉".to_owned()),
            Some("莉莉译".to_owned()),
            Some(vec!["甲一".to_owned(), "甲二".to_owned(), "乙".to_owned()]),
        )
        .expect("对话测试计划应该合法");
        let plan = plan(vec![
            set_text(segment_location(3), "<Tag:旧", "<Tag:新"),
            set_text(segment_location(4), "值><Tag:二>", "值><Tag:第二>>"),
            RpgMakerWriteBackMutation::ReplaceDialogue(body),
        ]);

        reset_structural_list_rebuild_count();
        let candidate = rewrite_documents(project_name(), workspace_root(), documents, plan)
            .expect("同一列表的降序结构修改应该成功");
        assert_eq!(
            structural_list_rebuild_count(),
            1,
            "对话结构修改必须只线性重建一次"
        );

        let map: Value = serde_json::from_str(file_text(&candidate, Path::new("data/Map007.json")))
            .expect("Map 候选应该是 JSON");
        let list = map["list"].as_array().expect("事件 list 应保持数组");
        assert_eq!(
            list.iter()
                .map(|command| command["code"].as_i64().unwrap())
                .collect::<Vec<_>>(),
            vec![101, 401, 401, 401, 108, 408, 0]
        );
        assert_eq!(list[1]["parameters"][0], "甲一");
        assert_eq!(list[2]["parameters"][0], "甲二");
        assert_eq!(list[1]["indent"], 1);
        assert_eq!(list[2]["indent"], 2);
        assert_eq!(list[1]["lineUnknown"], "A");
        assert_eq!(list[2]["lineUnknown"], "B");
        assert_eq!(list[3]["parameters"][0], "乙");
        assert_eq!(list[3]["indent"], 2);
        assert_eq!(list[3]["lineUnknown"], "B");
        assert_eq!(list[0]["parameters"][4], "莉莉译");
        assert_eq!(list[4]["parameters"][0], "<Tag:新");
        assert_eq!(list[4]["commentUnknown"], "first");
        assert_eq!(list[5]["parameters"][0], "值><Tag:第二>>");
        assert_eq!(list[5]["commentUnknown"], "continued");
        assert_eq!(map["unknown"]["kept"], true);
    }

    #[test]
    fn linear_structural_rebuild_matches_descending_splice_for_random_and_boundary_ranges() {
        fn assert_equivalent(original: Vec<Value>, specs: &[(usize, usize, Vec<Value>)]) {
            let mut expected = original.clone();
            for (start, end, values) in specs.iter().rev() {
                expected.splice(*start..*end, values.clone());
            }
            let replacements = specs
                .iter()
                .rev()
                .map(|(start, end, values)| StructuralReplacement {
                    start: *start,
                    end: *end,
                    values: values
                        .iter()
                        .map(clone_value)
                        .map(StackSafeJsonValue::new)
                        .collect(),
                })
                .collect();
            let mut actual = original;
            reset_structural_list_rebuild_count();
            rebuild_structural_list(&mut actual, replacements);
            assert_eq!(actual, expected);
            assert_eq!(structural_list_rebuild_count(), 1);
        }

        assert_equivalent(
            (0..8).map(Value::from).collect(),
            &[
                (0, 2, Vec::new()),
                (3, 5, vec![json!(30), json!(31)]),
                (7, 8, vec![json!(70)]),
            ],
        );
        assert_equivalent(
            (0..4).map(Value::from).collect(),
            &[(0, 4, vec![json!(100), json!(101), json!(102)])],
        );

        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        for case in 0..128_i64 {
            let length = 1 + (next_test_random(&mut state) % 128) as usize;
            let original = (0..length)
                .map(|value| Value::from(i64::try_from(value).unwrap()))
                .collect::<Vec<_>>();
            let mut specs = Vec::new();
            let mut cursor = 0;
            while cursor < length && specs.len() < 24 {
                let start = (cursor + (next_test_random(&mut state) % 4) as usize).min(length);
                if start == length {
                    break;
                }
                let maximum = (length - start).min(4);
                let removed = 1 + (next_test_random(&mut state) % maximum as u64) as usize;
                let end = start + removed;
                let inserted = (next_test_random(&mut state) % 5) as usize;
                let values = (0..inserted)
                    .map(|index| Value::from(-(case * 100 + index as i64 + 1)))
                    .collect();
                specs.push((start, end, values));
                cursor = end;
            }
            if specs.is_empty() {
                specs.push((0, 1, vec![Value::from(-case - 1)]));
            }
            assert_equivalent(original, &specs);
        }
    }

    fn next_test_random(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *state
    }

    #[test]
    fn three_source_401_lines_can_be_replaced_by_one_two_or_four_semantic_lines() {
        let source = RpgMakerSource::map(70);
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
            ["源一", "源二", "源三"]
                .into_iter()
                .enumerate()
                .map(|(source_line_index, text)| {
                    DialogueLineRecipe::new(
                        line_location(source_line_index + 1),
                        text,
                        vec![DialogueLinePart::BodyLine { source_line_index }],
                    )
                    .expect("正文行配方应合法")
                })
                .collect(),
        )
        .expect("三行对话配方应合法");

        for (texts, expected_templates) in [
            (vec!["译一"], vec![(1, "A")]),
            (vec!["译一", "译二"], vec![(1, "A"), (2, "B")]),
            (
                vec!["译一", "译二", "译三", "译四"],
                vec![(1, "A"), (2, "B"), (3, "C"), (3, "C")],
            ),
        ] {
            let body_lines = texts.iter().map(|text| (*text).to_owned()).collect();
            let mutation =
                ReplaceDialogueMutation::new(recipe.clone(), None, None, Some(body_lines))
                    .expect("自由断行正文应建立一个原子 Mutation");
            let documents = RpgMakerProjectDocuments::new(
                BTreeMap::from([(
                    RpgMakerDocumentId::Map(MapId::new(70).unwrap()),
                    json!({
                        "list": [
                            {"code":101,"indent":0,"parameters":["",0,0,2],"headerUnknown":true},
                            {"code":401,"indent":1,"parameters":["源一"],"lineUnknown":"A"},
                            {"code":401,"indent":2,"parameters":["源二"],"lineUnknown":"B"},
                            {"code":401,"indent":3,"parameters":["源三"],"lineUnknown":"C"},
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
                plan(vec![RpgMakerWriteBackMutation::ReplaceDialogue(mutation)]),
            )
            .expect("完整正文应按模型语义行数原子重建");
            let map: Value =
                serde_json::from_str(file_text(&candidate, Path::new("data/Map070.json")))
                    .expect("Map 候选应是 JSON");
            let list = map["list"].as_array().expect("事件列表应存在");
            assert_eq!(list.len(), texts.len() + 2);
            for (index, (expected_indent, expected_unknown)) in
                expected_templates.into_iter().enumerate()
            {
                assert_eq!(list[index + 1]["parameters"][0], texts[index]);
                assert_eq!(list[index + 1]["indent"], expected_indent);
                assert_eq!(list[index + 1]["lineUnknown"], expected_unknown);
            }
        }
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
                        DialogueLinePart::BodyLine {
                            source_line_index: 0,
                        },
                    ],
                )
                .expect("内嵌姓名行配方应合法"),
                DialogueLineRecipe::new(
                    line_location(2),
                    "Tail",
                    vec![DialogueLinePart::BodyLine {
                        source_line_index: 1,
                    }],
                )
                .expect("第二正文行配方应合法"),
            ],
        )
        .expect("MV 对话配方应合法");
        let mutation = ReplaceDialogueMutation::new(
            recipe,
            Some("Alice".to_owned()),
            Some("爱丽丝".to_owned()),
            Some(vec![
                "第一行".to_owned(),
                "第二行".to_owned(),
                "尾行".to_owned(),
            ]),
        )
        .expect("MV 对话 Mutation 应合法");
        let documents = RpgMakerProjectDocuments::new(
            BTreeMap::from([(
                RpgMakerDocumentId::Map(MapId::new(8).unwrap()),
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
            plan(vec![RpgMakerWriteBackMutation::ReplaceDialogue(mutation)]),
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
        assert_eq!(list[2]["lineUnknown"], "tail");
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
                        vec![DialogueLinePart::BodyLine {
                            source_line_index: 0,
                        }],
                    )
                    .unwrap(),
                ],
            )
            .unwrap()
        };
        let mutation = || {
            ReplaceDialogueMutation::new(
                recipe(),
                Some("バニー淫魔".to_owned()),
                Some("兔女郎魅魔".to_owned()),
                Some(vec!["译文".to_owned()]),
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
                BTreeMap::from([(
                    RpgMakerDocumentId::Map(MapId::new(9).unwrap()),
                    json!({"list":list}),
                )]),
                Vec::new(),
            )
        };

        let candidate = rewrite_documents(
            project_name(),
            workspace_root(),
            documents("台词", false),
            plan(vec![RpgMakerWriteBackMutation::ReplaceDialogue(mutation())]),
        )
        .expect("精确首行 Speaker 应可写回");
        let map: Value =
            serde_json::from_str(file_text(&candidate, Path::new("data/Map009.json"))).unwrap();
        assert_eq!(map["list"][1]["parameters"][0], "兔女郎魅魔");
        assert_eq!(map["list"][2]["parameters"][0], "译文");

        for (documents, expected_violation) in [
            (
                documents("来源已变化", false),
                WriteBackMutationViolation::FrozenDialogueBodyMismatch,
            ),
            (
                documents("台词", true),
                WriteBackMutationViolation::IncompleteDialogueCoverage,
            ),
        ] {
            let error = rewrite_documents(
                project_name(),
                workspace_root(),
                documents,
                plan(vec![RpgMakerWriteBackMutation::ReplaceDialogue(mutation())]),
            )
            .expect_err("来源或块长度漂移必须失败");
            assert!(matches!(
                error,
                RpgMakerWriteBackDocumentRewriteFailure::InvalidMutation { violation, .. }
                    if violation == expected_violation
            ));
        }
    }

    #[test]
    fn choices_update_the_102_list_and_same_level_402_labels_atomically() {
        let source = RpgMakerSource::map(10);
        let list_steps = vec![RpgMakerLocationStep::key("list")];
        let group_location = RpgMakerLocation::value(
            source.clone(),
            [list_steps.clone(), vec![RpgMakerLocationStep::index(0)]].concat(),
        );
        let target = |command_index, parameter_index, choice_index: Option<usize>| {
            choice_value_location(
                &source,
                &list_steps,
                command_index,
                parameter_index,
                choice_index,
            )
        };
        let recipe = |location, source_line_index, expected: &str| {
            DirectTextRecipe::new(
                location,
                expected,
                vec![DirectTextPart::LineSlot {
                    role: TextUnitRole::Choices,
                    source_line_index,
                }],
            )
            .expect("选项行配方应合法")
        };
        let recipes = vec![
            recipe(target(0, 0, Some(0)), 0, "はい"),
            recipe(target(0, 0, Some(1)), 1, "いいえ"),
            recipe(target(1, 1, None), 0, "はい"),
            recipe(target(6, 1, None), 1, "いいえ"),
        ];
        let mutation = ReplaceChoicesMutation::new(
            group_location,
            recipes,
            vec!["はい".to_owned(), "いいえ".to_owned()],
            vec!["是".to_owned(), "否".to_owned()],
        )
        .expect("选项 Mutation 应合法");
        let event_mutation = ReplaceEventBodyMutation::new(
            RpgMakerLocation::value(
                source.clone(),
                [list_steps.clone(), vec![RpgMakerLocationStep::index(9)]].concat(),
            ),
            vec![EventBodyMutationSegment::replace_for_test(
                RpgMakerLocation::value(
                    source,
                    [
                        list_steps,
                        vec![
                            RpgMakerLocationStep::index(10),
                            RpgMakerLocationStep::key("parameters"),
                            RpgMakerLocationStep::index(0),
                        ],
                    ]
                    .concat(),
                ),
                "滚动原文",
                vec!["滚动译文一".to_owned(), "滚动译文二".to_owned()],
            )],
        )
        .expect("同一 list 的滚动正文 Mutation 应合法");
        let documents = RpgMakerProjectDocuments::new(
            BTreeMap::from([(
                RpgMakerDocumentId::Map(MapId::new(10).unwrap()),
                json!({
                    "list": [
                        {"code":102,"indent":0,"parameters":[["はい","いいえ"],0,0,2,0],"headerUnknown":true},
                        {"code":402,"indent":0,"parameters":[0,"はい"],"branchUnknown":"first"},
                        {"code":102,"indent":1,"parameters":[["内側"],0,0,2,0]},
                        {"code":402,"indent":1,"parameters":[0,"内側"]},
                        {"code":404,"indent":1,"parameters":[]},
                        {"code":0,"indent":1,"parameters":[]},
                        {"code":402,"indent":0,"parameters":[1,"いいえ"],"branchUnknown":"second"},
                        {"code":404,"indent":0,"parameters":[]},
                        {"code":0,"indent":0,"parameters":[]},
                        {"code":105,"indent":0,"parameters":[2,false],"eventHeaderUnknown":true},
                        {"code":405,"indent":1,"parameters":["滚动原文"],"eventBodyUnknown":true},
                        {"code":0,"indent":0,"parameters":[]}
                    ]
                }),
            )]),
            Vec::new(),
        );

        reset_structural_list_rebuild_count();
        let candidate = rewrite_documents(
            project_name(),
            workspace_root(),
            documents,
            plan(vec![
                RpgMakerWriteBackMutation::ReplaceChoices(mutation),
                RpgMakerWriteBackMutation::ReplaceEventBody(event_mutation),
            ]),
        )
        .expect("选项头、分支标签与同 list 事件正文应能原子同步");
        assert_eq!(
            structural_list_rebuild_count(),
            1,
            "选项原位修改后，同一事件 list 只能线性重建一次"
        );
        let map: Value =
            serde_json::from_str(file_text(&candidate, Path::new("data/Map010.json"))).unwrap();
        let list = map["list"].as_array().unwrap();
        assert_eq!(list[0]["parameters"][0], json!(["是", "否"]));
        assert_eq!(list[1]["parameters"][1], "是");
        assert_eq!(list[6]["parameters"][1], "否");
        assert_eq!(list[3]["parameters"][1], "内側");
        assert_eq!(list[0]["headerUnknown"], true);
        assert_eq!(list[1]["branchUnknown"], "first");
        assert_eq!(list[6]["branchUnknown"], "second");
        assert_eq!(list[9]["eventHeaderUnknown"], true);
        assert_eq!(list[10]["parameters"][0], "滚动译文一");
        assert_eq!(list[11]["parameters"][0], "滚动译文二");
        assert_eq!(list[10]["eventBodyUnknown"], true);
        assert_eq!(list[11]["eventBodyUnknown"], true);
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
        let mutation_plan = plan(vec![RpgMakerWriteBackMutation::ReplaceEventBody(mutation)]);
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
            RpgMakerWriteBackDocumentRewriteFailure::InvalidMutation {
                violation: WriteBackMutationViolation::FrozenBodyCodeMismatch,
                ..
            }
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
            plan(vec![RpgMakerWriteBackMutation::ReplaceEventBody(mutation)]),
        )
        .expect_err("无法证明正文块边界时必须拒绝改写");

        assert!(matches!(
            error,
            RpgMakerWriteBackDocumentRewriteFailure::InvalidMutation {
                violation: WriteBackMutationViolation::CommandCodeMissingOrInvalid,
                ..
            }
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
            RpgMakerWriteBackDocumentRewriteFailure::InvalidMutation {
                violation: WriteBackMutationViolation::PluginNameMismatch,
                ..
            }
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
            RpgMakerWriteBackDocumentRewriteFailure::InvalidMutation {
                violation: WriteBackMutationViolation::ExpectedOriginalMismatch,
                ..
            }
        ));
    }

    #[test]
    fn document_failures_follow_stable_physical_order_instead_of_plan_order() {
        let actor_location = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Actors),
            vec![
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("name"),
            ],
        );
        let map_location = RpgMakerLocation::value(
            RpgMakerSource::map(1),
            vec![RpgMakerLocationStep::key("displayName")],
        );
        let documents = RpgMakerProjectDocuments::new(
            BTreeMap::from([
                (
                    RpgMakerDocumentId::Data(StandardDataFile::Actors),
                    json!([null, {"name": "角色现值"}]),
                ),
                (
                    RpgMakerDocumentId::Map(MapId::new(1).unwrap()),
                    json!({"displayName": "地图现值"}),
                ),
            ]),
            Vec::new(),
        );
        let plan = plan(vec![
            set_text(map_location, "地图旧值", "地图译文"),
            set_text(actor_location.clone(), "角色旧值", "角色译文"),
        ]);

        let error = rewrite_documents(project_name(), workspace_root(), documents, plan)
            .expect_err("两个物理文档都不匹配时必须稳定选择自然顺序中的首个错误");

        assert!(matches!(
            error,
            RpgMakerWriteBackDocumentRewriteFailure::InvalidMutation { location, .. }
                if location.as_ref() == &actor_location
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
    fn candidate_paths_reject_non_adjacent_windows_case_equivalent_duplicates() {
        let files = ["data/Actors.json", "data/[.json", "data/actors.json"]
            .into_iter()
            .map(|path| RpgMakerRewrittenFile::new(path.into(), Vec::new()).unwrap())
            .collect();

        let error = RpgMakerRewrittenDocuments::new(project_name(), workspace_root(), files)
            .expect_err("原始排序中不相邻的 Windows 等价路径也必须在领域边界失败");

        assert!(matches!(
            error,
            RpgMakerWriteBackDocumentRewriteFailure::DuplicateOutputPath { path }
                if path == Path::new("data/Actors.json")
        ));
    }

    #[test]
    fn candidate_path_identity_uses_windows_ordinal_kelvin_matrix() {
        let distinct = ["data/K.json", "data/\u{212a}.json"]
            .into_iter()
            .map(|path| RpgMakerRewrittenFile::new(path.into(), Vec::new()).unwrap())
            .collect();
        RpgMakerRewrittenDocuments::new(project_name(), workspace_root(), distinct)
            .expect("Kelvin 符号与 ASCII K 在 Windows ordinal 语义下可并存");

        let duplicates = ["data/K.json", "data/k.json"]
            .into_iter()
            .map(|path| RpgMakerRewrittenFile::new(path.into(), Vec::new()).unwrap())
            .collect();
        assert!(matches!(
            RpgMakerRewrittenDocuments::new(project_name(), workspace_root(), duplicates),
            Err(RpgMakerWriteBackDocumentRewriteFailure::DuplicateOutputPath { .. })
        ));
    }

    #[test]
    fn standard_data_at_4096_levels_parses_rewrites_serializes_and_drops_without_recursion() {
        const DEPTH: usize = 4_096;
        let mut source_text = "[".repeat(DEPTH);
        source_text.push_str(r#""原文""#);
        source_text.push_str(&"]".repeat(DEPTH));
        let id = RpgMakerDocumentId::Data(StandardDataFile::Actors);
        let documents = parse_json_document_for_test(id, &source_text);
        let location = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Actors),
            std::iter::repeat_n(RpgMakerLocationStep::index(0), DEPTH).collect(),
        );

        let candidate = rewrite_documents(
            project_name(),
            workspace_root(),
            documents,
            plan(vec![set_text(location, "原文", "译文")]),
        )
        .expect("4096 层 RPG Maker data 文档应可完成写回");

        let reparsed = parse_json(file_text(&candidate, Path::new("data/Actors.json")))
            .expect("深层 RPG Maker data 候选应仍是有效 JSON");
        let mut current = reparsed.as_ref();
        for _ in 0..DEPTH {
            current = current
                .as_array()
                .and_then(|values| values.first())
                .expect("每一层都应保留单元素 array");
        }
        assert_eq!(current.as_str(), Some("译文"));
    }

    #[test]
    fn plugin_parameter_at_10000_levels_parses_rewrites_serializes_and_drops_without_recursion() {
        const DEPTH: usize = 10_000;
        let mut nested = "[".repeat(DEPTH);
        nested.push_str(r#""原文""#);
        nested.push_str(&"]".repeat(DEPTH));
        let encoded_parameter =
            serde_json::to_string(&nested).expect("嵌套参数字符串应可编码为 JSON scalar");
        let prefix = "// Generated by RPG Maker.\r\n// 原样保留  \r\n\r\n";
        let plugins_text = format!(
            "{prefix}var $plugins = [{{\"name\":\"Deep\",\"status\":true,\"parameters\":{{\"Config\":{encoded_parameter}}}}}];"
        );
        let documents = parse_plugins_document_for_test(&plugins_text);
        let mut steps = Vec::with_capacity(DEPTH + 1);
        steps.push(RpgMakerLocationStep::DecodeJsonString);
        steps.extend(std::iter::repeat_n(RpgMakerLocationStep::index(0), DEPTH));
        let location =
            RpgMakerLocation::value(RpgMakerSource::plugin_parameter(0, "Deep", "Config"), steps);

        let candidate = rewrite_documents(
            project_name(),
            workspace_root(),
            documents,
            plan(vec![set_text(location, "原文", "译文")]),
        )
        .expect("10000 层插件嵌套 JSON 参数应可完成写回");

        let plugin_file = file_text(&candidate, Path::new("js/plugins.js"));
        let plugin_json = plugin_file
            .strip_prefix(prefix)
            .and_then(|value| value.strip_prefix("var $plugins = "))
            .and_then(|value| value.strip_suffix(";\n"))
            .expect("插件候选应逐字保留合法前缀并使用规范 assignment 外壳");
        let plugins = parse_json(plugin_json).expect("插件候选主体应是有效 JSON");
        let nested = plugins[0]["parameters"]["Config"]
            .as_str()
            .expect("Config 应保持嵌套 JSON 字符串");
        let nested = parse_json(nested).expect("改写后的深层参数应是有效 JSON");
        let mut current = nested.as_ref();
        for _ in 0..DEPTH {
            current = current
                .as_array()
                .and_then(|values| values.first())
                .expect("每一层都应保留单元素 array");
        }
        assert_eq!(current.as_str(), Some("译文"));
    }

    #[test]
    fn rewrite_future_is_send() {
        fn assert_send(_: impl Send) {}

        let service = RpgMakerWriteBackDocumentRewritingService::new(PanickingReader, PanickingCpu);
        let project = project();
        assert_send(service.rewrite(&project, RpgMakerWriteBackMutationPlan::empty()));
    }

    fn set_text(
        location: RpgMakerLocation,
        expected: impl Into<String>,
        replacement: impl Into<String>,
    ) -> RpgMakerWriteBackMutation {
        RpgMakerWriteBackMutation::SetText(SetTextMutation::for_test(
            location,
            expected,
            replacement,
        ))
    }

    fn plan(mutations: Vec<RpgMakerWriteBackMutation>) -> RpgMakerWriteBackMutationPlan {
        RpgMakerWriteBackMutationPlan::new(mutations).expect("测试 Mutation Plan 应该合法")
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
        )
    }
}
