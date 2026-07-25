//! RPG Maker 固定位置文本与标准事件块的完整 Builtin 快照提取。

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::sync::Arc;

use serde_json::{Map, Value};

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, RecoveryFact, SafeDiagnostic, SafeDiagnosticSource,
};
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::rpg_maker::dialogue::{
    DialoguePhysicalLine, MvDialogueDefinition, MvDialogueDefinitionError,
    MvDialogueProjectionError, MvDialogueProjector, projection_model_detail,
};
use crate::rpg_maker::json::StackSafeJsonValue;
use crate::rpg_maker::model::{
    DialogueLinePart, DialogueLineRecipe, DialogueWriteRecipe, DirectSpeakerTarget, DirectTextPart,
    DirectTextRecipe, MutationClaim, TextProjectionRecipe, TextUnitContent, TextUnitRole,
};
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::text::MapId;

use super::document::{
    DocumentReadProgress, RpgMakerDocumentId, RpgMakerDocumentSelection,
    RpgMakerProjectDocumentReader, RpgMakerProjectDocuments, StandardDataFile,
};
use super::model::{
    BuiltinSnapshot, ExtractedTextGroup, ExtractedTextUnit, RpgMakerLocation, RpgMakerLocationStep,
    RpgMakerSource, SnapshotModelError, TextGroupKind,
};
use super::store::{BuiltinProjectDefinitionUpdate, BuiltinSnapshotStore};
use super::{ExtractProgress, ExtractProgressPhase};

/// 刷新 RPG Maker 内置规格能够确定的标准文本资产。
pub(crate) trait BuiltInExtraction: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn refresh(
        &self,
        project: &OpenedProject,
        progress: ExtractProgress,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 从无损 RPG Maker 文档建立 Builtin 快照，再由 Store 原子替换旧快照。
pub(crate) struct BuiltInExtractionService<R, S, C> {
    document_reader: R,
    snapshot_store: S,
    cpu_executor: C,
    dialogue_definition: BuiltinDialogueDefinition,
}

impl<R, S, C> BuiltInExtractionService<R, S, C> {
    pub(crate) fn new(document_reader: R, snapshot_store: S, cpu_executor: C) -> Self {
        Self {
            document_reader,
            snapshot_store,
            cpu_executor,
            dialogue_definition: BuiltinDialogueDefinition::MzNative,
        }
    }

    /// 建立 MV Builtin 提取，并明确复用项目定义或完整替换定义。
    pub(crate) fn for_mv(
        document_reader: R,
        snapshot_store: S,
        cpu_executor: C,
        dialogue_definition: MvDialogueDefinitionSelection,
    ) -> Self {
        Self {
            document_reader,
            snapshot_store,
            cpu_executor,
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

    async fn refresh(
        &self,
        project: &OpenedProject,
        progress: ExtractProgress,
    ) -> Result<(), Self::Error> {
        let (dialogue_projection, project_definition_update) = self
            .dialogue_definition
            .resolve(project)
            .map_err(BuiltInExtractionError::CompileDialogueDefinition)?;
        let documents = self
            .document_reader
            .read_with_progress(
                project,
                builtin_document_selection(),
                DocumentReadProgress::new({
                    let progress = progress.clone();
                    move |completed, total| {
                        progress.determinate(
                            ExtractProgressPhase::BuiltinDocuments,
                            completed,
                            total,
                        );
                    }
                }),
            )
            .await
            .map_err(BuiltInExtractionError::ReadDocuments)?;

        let snapshot = match build_builtin_snapshot_parallel(
            &self.cpu_executor,
            documents,
            &dialogue_projection,
            progress.clone(),
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

        progress.indeterminate(ExtractProgressPhase::BuiltinCommit);
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

impl<RE, SE, CE> BuiltInExtractionError<RE, SE, CE>
where
    RE: SafeDiagnosticSource,
    SE: SafeDiagnosticSource,
    CpuTaskExecutionError<CE>: SafeDiagnosticSource,
{
    /// 在仍持有 Builtin 阶段错误类型时建立唯一公开投影。
    pub(crate) fn safe_diagnostic(&self) -> SafeDiagnostic {
        match self {
            Self::CompileDialogueDefinition(source) => source.safe_diagnostic_source(
                DiagnosticStage::Extract,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
            ),
            Self::ReadDocuments(source) => source.safe_diagnostic_source(
                DiagnosticStage::Extract,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
            ),
            Self::ScheduleCompute(source) => source.safe_diagnostic_source(
                DiagnosticStage::Extract,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::Retry,
            ),
            Self::MalformedDocument(source) => source.safe_diagnostic_source(
                DiagnosticStage::Extract,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
            ),
            Self::BuildSnapshot(source) => snapshot_model_safe_diagnostic(source),
            Self::ProjectDialogue(source) => source.safe_diagnostic_source(
                DiagnosticStage::Extract,
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
            ),
            Self::Persist(source) => source
                .safe_diagnostic_source(
                    DiagnosticStage::Extract,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckProjectState,
                )
                .with_recovery(RecoveryFact::component("owner=builtin")),
        }
    }
}

/// 一个所选标准 RPG Maker 文档不符合固定结构。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BuiltinDocumentError {
    location: String,
    reason: BuiltinDocumentFailure,
}

impl BuiltinDocumentError {
    fn new(location: impl Into<String>, reason: BuiltinDocumentFailure) -> Self {
        Self {
            location: location.into(),
            reason,
        }
    }

    #[cfg(test)]
    pub(crate) fn location(&self) -> &str {
        &self.location
    }

    fn reason_detail(&self) -> (DiagnosticFailureKind, String) {
        match &self.reason {
            BuiltinDocumentFailure::MissingDocument => (
                DiagnosticFailureKind::NotFound,
                "structure=document_missing".to_owned(),
            ),
            BuiltinDocumentFailure::ExpectedObject => (
                DiagnosticFailureKind::InvalidValue,
                "structure=expected_object".to_owned(),
            ),
            BuiltinDocumentFailure::ExpectedArray => (
                DiagnosticFailureKind::InvalidValue,
                "structure=expected_array".to_owned(),
            ),
            BuiltinDocumentFailure::ExpectedString => (
                DiagnosticFailureKind::InvalidValue,
                "structure=expected_string".to_owned(),
            ),
            BuiltinDocumentFailure::MissingValue => (
                DiagnosticFailureKind::MissingRequiredValue,
                "structure=field_or_element_missing".to_owned(),
            ),
            BuiltinDocumentFailure::EventCodeMustBeInteger => (
                DiagnosticFailureKind::InvalidValue,
                "structure=event_command; field=code; expected=integer".to_owned(),
            ),
            BuiltinDocumentFailure::EventParametersMissing => (
                DiagnosticFailureKind::MissingRequiredValue,
                "structure=event_command; field=parameters".to_owned(),
            ),
            BuiltinDocumentFailure::EventIndentMustBeInteger => (
                DiagnosticFailureKind::InvalidValue,
                "structure=event_command; field=indent; expected=integer".to_owned(),
            ),
            BuiltinDocumentFailure::ContinuationWithoutStart { command_code } => (
                DiagnosticFailureKind::RequirementFailed,
                format!(
                    "structure=event_command; command_code={command_code}; required_start_command=missing"
                ),
            ),
            BuiltinDocumentFailure::ChoiceIndexInvalid {
                actual,
                option_count,
            } => (
                DiagnosticFailureKind::InvalidValue,
                format!(
                    "structure=choice_branch; command_code=402; choice_index={}; option_count={option_count}",
                    actual.map_or_else(|| "not_integer".to_owned(), |value| value.to_string())
                ),
            ),
            BuiltinDocumentFailure::DuplicateChoiceBranch { choice_index } => (
                DiagnosticFailureKind::ConflictingValues,
                format!(
                    "structure=choice_branch; command_code=402; duplicate_choice_index={choice_index}"
                ),
            ),
            BuiltinDocumentFailure::ChoiceBranchTextMismatch { choice_index } => (
                DiagnosticFailureKind::ConflictingValues,
                format!(
                    "structure=choice_branch; command_code=402; choice_index={choice_index}; branch_text=does_not_match_command_102"
                ),
            ),
            BuiltinDocumentFailure::ChoiceEndMissing => (
                DiagnosticFailureKind::RequirementFailed,
                "structure=choice_block; start_command=102; end_command=404; state=missing"
                    .to_owned(),
            ),
            BuiltinDocumentFailure::ChoiceBranchesIncomplete { expected, actual } => (
                DiagnosticFailureKind::RequirementFailed,
                format!(
                    "structure=choice_block; start_command=102; branch_command=402; expected_branches={expected}; actual_branches={actual}"
                ),
            ),
        }
    }

    pub(crate) fn safe_diagnostic(
        &self,
        stage: DiagnosticStage,
        impact: DiagnosticImpact,
        action: DiagnosticAction,
    ) -> SafeDiagnostic {
        let (failure, detail) = self.reason_detail();
        SafeDiagnostic::new(
            DiagnosticCode::ExtractBuiltin,
            stage,
            DiagnosticSubject::field(&self.location),
            DiagnosticReason::failure_with_detail(failure, detail),
            impact,
            action,
        )
    }
}

impl fmt::Display for BuiltinDocumentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}：{}", self.location, self.reason)
    }
}

impl Error for BuiltinDocumentError {}

impl SafeDiagnosticSource for BuiltinDocumentError {
    fn safe_diagnostic_source(
        &self,
        stage: DiagnosticStage,
        impact: DiagnosticImpact,
        fallback_action: DiagnosticAction,
    ) -> SafeDiagnostic {
        self.safe_diagnostic(stage, impact, fallback_action)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum BuiltinDocumentFailure {
    MissingDocument,
    ExpectedObject,
    ExpectedArray,
    ExpectedString,
    MissingValue,
    EventCodeMustBeInteger,
    EventParametersMissing,
    EventIndentMustBeInteger,
    ContinuationWithoutStart {
        command_code: i64,
    },
    ChoiceIndexInvalid {
        actual: Option<i64>,
        option_count: usize,
    },
    DuplicateChoiceBranch {
        choice_index: usize,
    },
    ChoiceBranchTextMismatch {
        choice_index: usize,
    },
    ChoiceEndMissing,
    ChoiceBranchesIncomplete {
        expected: usize,
        actual: usize,
    },
}

impl fmt::Display for BuiltinDocumentFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingDocument => formatter.write_str("文档缺失"),
            Self::ExpectedObject => formatter.write_str("必须是对象"),
            Self::ExpectedArray => formatter.write_str("必须是数组"),
            Self::ExpectedString => formatter.write_str("必须是字符串"),
            Self::MissingValue => formatter.write_str("字段或元素缺失"),
            Self::EventCodeMustBeInteger => formatter.write_str("code 必须是整数"),
            Self::EventParametersMissing => formatter.write_str("缺少 parameters"),
            Self::EventIndentMustBeInteger => formatter.write_str("indent 必须是整数"),
            Self::ContinuationWithoutStart { command_code } => {
                write!(formatter, "事件指令 {command_code} 缺少对应的起始指令")
            }
            Self::ChoiceIndexInvalid { .. } => {
                formatter.write_str("402 选项索引必须指向当前 102 的一个选项")
            }
            Self::DuplicateChoiceBranch { choice_index } => {
                write!(formatter, "当前 102 重复包含选项分支 {choice_index}")
            }
            Self::ChoiceBranchTextMismatch { choice_index } => {
                write!(formatter, "402 分支文本与 102 选项 {choice_index} 不一致")
            }
            Self::ChoiceEndMissing | Self::ChoiceBranchesIncomplete { .. } => {
                formatter.write_str("102 必须包含完整且唯一的同层 402 分支以及 404 结束指令")
            }
        }
    }
}

fn snapshot_model_safe_diagnostic(source: &SnapshotModelError) -> SafeDiagnostic {
    let (subject, failure, detail) = match source {
        SnapshotModelError::BlankSourceContent { exact_location } => (
            DiagnosticSubject::field(exact_location.to_string()),
            DiagnosticFailureKind::InvalidValue,
            "structure=text_unit; source_content=blank".to_owned(),
        ),
        SnapshotModelError::ContentShapeMismatch {
            role,
            exact_location,
        } => (
            DiagnosticSubject::field(exact_location.to_string()),
            DiagnosticFailureKind::InvalidValue,
            format!(
                "structure=text_unit; role={}; content_shape=mismatch",
                builtin_role_name(role)
            ),
        ),
        SnapshotModelError::DirectGroupRequiresValue {
            role,
            exact_location,
        } => (
            DiagnosticSubject::field(exact_location.to_string()),
            DiagnosticFailureKind::InvalidValue,
            format!(
                "structure=direct_group; role={}; required_content_shape=value",
                builtin_role_name(role)
            ),
        ),
        SnapshotModelError::InvalidSourceLine {
            source_line_index,
            exact_location,
        } => (
            DiagnosticSubject::field(exact_location.to_string()),
            DiagnosticFailureKind::InvalidValue,
            format!(
                "structure=text_unit; source_line_index={source_line_index}; forbidden_character=cr_lf_or_nul"
            ),
        ),
        SnapshotModelError::EmptyGroup { group_location } => (
            DiagnosticSubject::field(group_location.to_string()),
            DiagnosticFailureKind::MissingRequiredValue,
            "structure=text_group; units=empty".to_owned(),
        ),
        SnapshotModelError::EmptyProjection { group_location } => (
            DiagnosticSubject::field(group_location.to_string()),
            DiagnosticFailureKind::MissingRequiredValue,
            "structure=text_group; projection_recipes=empty".to_owned(),
        ),
        SnapshotModelError::DuplicateLogicalLocation { logical_location } => (
            DiagnosticSubject::field(logical_location.group_location().to_string()),
            DiagnosticFailureKind::ConflictingValues,
            format!(
                "structure=snapshot; duplicate_logical_location=true; role={}",
                builtin_role_name(logical_location.role())
            ),
        ),
        SnapshotModelError::ConflictingGroupKind {
            group_location,
            first,
            second,
        } => (
            DiagnosticSubject::field(group_location.to_string()),
            DiagnosticFailureKind::ConflictingValues,
            format!(
                "structure=text_group; first_kind={}; second_kind={}",
                builtin_group_kind_name(*first),
                builtin_group_kind_name(*second)
            ),
        ),
        SnapshotModelError::MutationClaimConflict { resource } => (
            DiagnosticSubject::operation("build_builtin_snapshot_claim_index"),
            DiagnosticFailureKind::ConflictingValues,
            format!(
                "structure=mutation_claim; resource_kind={}; access=conflicting",
                mutation_resource_kind(resource)
            ),
        ),
        SnapshotModelError::RecipeRoleMismatch {
            group_location,
            units,
            referenced,
        } => (
            DiagnosticSubject::field(group_location.to_string()),
            DiagnosticFailureKind::InvalidValue,
            format!(
                "structure=projection_recipe; role_set=mismatch; unit_role_count={}; referenced_role_count={}",
                units.len(),
                referenced.len()
            ),
        ),
        SnapshotModelError::RecipeLineMismatch {
            group_location,
            role,
            expected,
            referenced,
        } => (
            DiagnosticSubject::field(group_location.to_string()),
            DiagnosticFailureKind::InvalidValue,
            format!(
                "structure=projection_recipe; role={}; line_set=mismatch; expected_count={}; referenced_count={}; first_missing={}; first_unexpected={}",
                builtin_role_name(role),
                expected.len(),
                referenced.len(),
                optional_usize(expected.difference(referenced).next().copied()),
                optional_usize(referenced.difference(expected).next().copied())
            ),
        ),
        SnapshotModelError::Projection(source) => (
            DiagnosticSubject::operation("build_builtin_projection"),
            DiagnosticFailureKind::InternalInvariant,
            projection_model_detail(source),
        ),
    };
    SafeDiagnostic::new(
        DiagnosticCode::ExtractBuiltin,
        DiagnosticStage::Extract,
        subject,
        DiagnosticReason::failure_with_detail(failure, detail),
        DiagnosticImpact::Unchanged,
        DiagnosticAction::ReportBug,
    )
}

fn builtin_role_name(role: &TextUnitRole) -> &'static str {
    match role {
        TextUnitRole::Scalar(_) => "scalar",
        TextUnitRole::DialogueSpeaker => "dialogue_speaker",
        TextUnitRole::DialogueBody => "dialogue_body",
        TextUnitRole::Choices => "choices",
        TextUnitRole::ScrollingText => "scrolling_text",
    }
}

fn builtin_group_kind_name(kind: TextGroupKind) -> &'static str {
    match kind {
        TextGroupKind::DatabaseEntry => "database_entry",
        TextGroupKind::System => "system",
        TextGroupKind::Map => "map",
        TextGroupKind::EventDialogue => "event_dialogue",
        TextGroupKind::EventChoices => "event_choices",
        TextGroupKind::EventScrollingText => "event_scrolling_text",
        TextGroupKind::EventCommand => "event_command",
        TextGroupKind::PluginParameter => "plugin_parameter",
    }
}

fn mutation_resource_kind(resource: &crate::rpg_maker::model::MutationResource) -> &'static str {
    match resource {
        crate::rpg_maker::model::MutationResource::Value { .. } => "value",
        crate::rpg_maker::model::MutationResource::NoteTag { .. } => "note_tag",
        crate::rpg_maker::model::MutationResource::CommentTag { .. } => "comment_tag",
    }
}

fn optional_usize(value: Option<usize>) -> String {
    value.map_or_else(|| "none".to_owned(), |value| value.to_string())
}

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
    documents: RpgMakerProjectDocuments,
    dialogue_projection: &BuiltinDialogueProjection,
    progress: ExtractProgress,
) -> Result<BuiltinSnapshot, ParallelBuiltinBuildError<C::Error>>
where
    C: CpuTaskExecutor,
{
    let work_units = builtin_work_units(documents, dialogue_projection).map_err(|error| {
        ParallelBuiltinBuildError::Build(BuildBuiltinSnapshotError::Malformed(error))
    })?;
    let total = u64::try_from(work_units.len()).expect("Builtin 工作单元数必须能用 u64 表达");
    progress.determinate(ExtractProgressPhase::BuiltinWorkUnits, 0, total);
    let results = cpu_executor
        .execute_ordered_map_observed(work_units, BuiltinWorkUnit::run, {
            let progress = progress.clone();
            move |completed| {
                progress.determinate(ExtractProgressPhase::BuiltinWorkUnits, completed, total);
            }
        })
        .await
        .map_err(ParallelBuiltinBuildError::Compute)?;
    let dialogue_projection = dialogue_projection.fork_for_scan();
    cpu_executor
        .execute(move || finish_builtin_work_units(results, dialogue_projection))
        .await
        .map_err(ParallelBuiltinBuildError::Compute)?
        .map_err(ParallelBuiltinBuildError::Build)
}

fn finish_builtin_work_units(
    results: Vec<Result<BuiltinWorkUnitResult, BuildBuiltinSnapshotError>>,
    mut dialogue_projection: BuiltinDialogueProjection,
) -> Result<BuiltinSnapshot, BuildBuiltinSnapshotError> {
    let mut groups = Vec::new();
    for result in results {
        let local_result = result?;
        groups.extend(local_result.groups);
        if let Some(scanned) = local_result.dialogue_projection {
            dialogue_projection.merge_scan(scanned);
        }
    }
    dialogue_projection.finish()?;
    BuiltinSnapshot::new(groups).map_err(Into::into)
}

enum BuiltinWorkUnit {
    Database {
        file: StandardDataFile,
        field_names: &'static [&'static str],
        document: Arc<StackSafeJsonValue>,
    },
    System(Arc<StackSafeJsonValue>),
    Map {
        map_id: MapId,
        document: Arc<StackSafeJsonValue>,
        dialogue_projection: BuiltinDialogueProjection,
    },
    CommonEvents {
        document: Arc<StackSafeJsonValue>,
        dialogue_projection: BuiltinDialogueProjection,
    },
    Troops {
        document: Arc<StackSafeJsonValue>,
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

fn single_document(
    id: RpgMakerDocumentId,
    document: Arc<StackSafeJsonValue>,
) -> RpgMakerProjectDocuments {
    RpgMakerProjectDocuments::from_shared_parts([(id, document)].into_iter().collect(), Vec::new())
}

fn builtin_work_units(
    documents: RpgMakerProjectDocuments,
    dialogue_projection: &BuiltinDialogueProjection,
) -> Result<Vec<BuiltinWorkUnit>, BuiltinDocumentError> {
    let (mut documents, _plugins) = documents.into_shared_parts();
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
    documents: &mut std::collections::BTreeMap<RpgMakerDocumentId, Arc<StackSafeJsonValue>>,
    file: StandardDataFile,
) -> Result<Arc<StackSafeJsonValue>, BuiltinDocumentError> {
    documents
        .remove(&RpgMakerDocumentId::Data(file))
        .ok_or_else(|| {
            BuiltinDocumentError::new(
                format!("data/{}", file.file_name()),
                BuiltinDocumentFailure::MissingDocument,
            )
        })
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
        let source = RpgMakerSource::map_id(*map_id);
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
                list,
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
                    BuiltinDocumentFailure::ContinuationWithoutStart {
                        command_code: command.code,
                    },
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
    let code = command.get("code").and_then(Value::as_i64).ok_or_else(|| {
        BuiltinDocumentError::new(
            location.to_string(),
            BuiltinDocumentFailure::EventCodeMustBeInteger,
        )
    })?;
    let parameters = command.get("parameters").ok_or_else(|| {
        BuiltinDocumentError::new(
            location.to_string(),
            BuiltinDocumentFailure::EventParametersMissing,
        )
    })?;
    let parameters = expect_array(parameters, format!("{location}.parameters"))?;
    Ok(EventCommand {
        code,
        parameters,
        location,
    })
}

fn command_indent(
    list: &[Value],
    command_index: usize,
    location: &RpgMakerLocation,
) -> Result<i64, BuiltinDocumentError> {
    expect_object(&list[command_index], location.to_string())?
        .get("indent")
        .and_then(Value::as_i64)
        .ok_or_else(|| {
            BuiltinDocumentError::new(
                location.to_string(),
                BuiltinDocumentFailure::EventIndentMustBeInteger,
            )
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
    if let Some(group) = group {
        groups.push(group);
    }
    Ok(next_index.saturating_sub(1))
}

fn project_mz_dialogue(
    source: &RpgMakerSource,
    list_steps: &[RpgMakerLocationStep],
    start_index: usize,
    parameters: &[Value],
    group_location: RpgMakerLocation,
    lines: Vec<DialoguePhysicalLine>,
) -> Result<Option<ExtractedTextGroup>, BuildBuiltinSnapshotError> {
    let mut units = Vec::new();
    let speaker_location = parameter_location(source, list_steps, start_index, 4);
    let direct_speaker = match parameters.get(4) {
        None => None,
        Some(value) => {
            let speaker = expect_string(value, &speaker_location)?;
            if speaker.trim().is_empty() {
                None
            } else {
                units.push(ExtractedTextUnit::projected(
                    TextUnitRole::DialogueSpeaker,
                    speaker_location.clone(),
                    TextUnitContent::Value(speaker.to_owned()),
                )?);
                Some(DirectSpeakerTarget::new(speaker_location, speaker))
            }
        }
    };

    let has_body = lines
        .iter()
        .any(|line| !line.expected_raw().trim().is_empty());
    let body_projection_location = lines.first().map(|line| line.physical_location().clone());
    let body_lines = has_body.then(|| {
        lines
            .iter()
            .map(|line| line.expected_raw().to_owned())
            .collect::<Vec<_>>()
    });
    let mut line_recipes = Vec::with_capacity(lines.len());
    for (source_line_index, line) in lines.into_iter().enumerate() {
        let parts = if has_body {
            vec![DialogueLinePart::BodyLine { source_line_index }]
        } else {
            vec![DialogueLinePart::Literal(line.expected_raw().to_owned())]
        };
        line_recipes.push(
            DialogueLineRecipe::new(line.physical_location().clone(), line.expected_raw(), parts)
                .map_err(SnapshotModelError::Projection)?,
        );
    }
    if let Some(body_lines) = body_lines {
        units.push(ExtractedTextUnit::projected(
            TextUnitRole::DialogueBody,
            body_projection_location.expect("非空正文必须有首个物理来源"),
            TextUnitContent::Lines(body_lines),
        )?);
    }

    if units.is_empty() {
        return Ok(None);
    }

    let recipe = DialogueWriteRecipe::new(group_location.clone(), direct_speaker, line_recipes)
        .map_err(SnapshotModelError::Projection)?;
    ExtractedTextGroup::projected(
        TextGroupKind::EventDialogue,
        group_location,
        units,
        vec![TextProjectionRecipe::Dialogue(recipe)],
    )
    .map(Some)
    .map_err(Into::into)
}

fn project_mv_dialogue(
    projector: &mut MvDialogueProjector,
    group_location: RpgMakerLocation,
    lines: Vec<DialoguePhysicalLine>,
) -> Result<Option<ExtractedTextGroup>, BuildBuiltinSnapshotError> {
    let projected = projector.project(group_location.clone(), lines)?;
    let (projected_units, recipe) = projected.into_parts();
    let units = projected_units
        .into_iter()
        .map(|unit| {
            let (role, projection_location, source_content) = unit.into_parts();
            ExtractedTextUnit::projected(role, projection_location, source_content)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if units.is_empty() {
        return Ok(None);
    }
    ExtractedTextGroup::projected(
        TextGroupKind::EventDialogue,
        group_location,
        units,
        vec![TextProjectionRecipe::Dialogue(recipe)],
    )
    .map(Some)
    .map_err(Into::into)
}

fn extract_choices(
    source: &RpgMakerSource,
    list_steps: &[RpgMakerLocationStep],
    list: &[Value],
    command_index: usize,
    parameters: &[Value],
    groups: &mut Vec<ExtractedTextGroup>,
) -> Result<(), BuildBuiltinSnapshotError> {
    let choices_location = parameter_location(source, list_steps, command_index, 0);
    let group_location = command_location(source, list_steps, command_index);
    let choices = parameters
        .first()
        .ok_or_else(|| missing_value(&choices_location))?;
    let choices = expect_array(choices, choices_location.to_string())?;
    let choice_texts = choices
        .iter()
        .enumerate()
        .map(|(choice_index, choice)| {
            let mut steps = value_steps(&choices_location);
            steps.push(RpgMakerLocationStep::index(choice_index));
            let exact_location = RpgMakerLocation::value(source.clone(), steps);
            expect_string(choice, &exact_location).map(str::to_owned)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if choice_texts.iter().all(|choice| choice.trim().is_empty()) {
        return Ok(());
    }

    let mut recipes = Vec::new();
    let mut covered_values = vec![
        choices_location.clone(),
        command_field_location(source, list_steps, command_index, "code"),
        command_field_location(source, list_steps, command_index, "indent"),
    ];
    for (choice_index, choice) in choices.iter().enumerate() {
        let mut steps = value_steps(&choices_location);
        steps.push(RpgMakerLocationStep::index(choice_index));
        let exact_location = RpgMakerLocation::value(source.clone(), steps);
        let text = expect_string(choice, &exact_location)?;
        covered_values.push(exact_location.clone());
        recipes.push(TextProjectionRecipe::Direct(
            DirectTextRecipe::new(
                exact_location,
                text,
                vec![DirectTextPart::LineSlot {
                    role: TextUnitRole::Choices,
                    source_line_index: choice_index,
                }],
            )
            .map_err(SnapshotModelError::Projection)?,
        ));
    }

    let choice_indent = command_indent(
        list,
        command_index,
        &command_location(source, list_steps, command_index),
    )?;
    let mut branch_indexes = BTreeSet::new();
    let mut end_location = None;
    for branch_command_index in command_index + 1..list.len() {
        let command = command_at(source, list_steps, list, branch_command_index)?;
        if !matches!(command.code, 402 | 404) {
            continue;
        }
        let indent = command_indent(list, branch_command_index, &command.location)?;
        if command.code == 404 && indent == choice_indent {
            covered_values.extend([
                command_field_location(source, list_steps, branch_command_index, "code"),
                command_field_location(source, list_steps, branch_command_index, "indent"),
            ]);
            end_location = Some(command.location.clone());
            break;
        }
        if command.code != 402 || indent != choice_indent {
            continue;
        }
        covered_values.extend([
            command_field_location(source, list_steps, branch_command_index, "code"),
            command_field_location(source, list_steps, branch_command_index, "indent"),
        ]);
        let index_location = parameter_location(source, list_steps, branch_command_index, 0);
        let raw_choice_index = command.parameters.first().and_then(Value::as_i64);
        let choice_index = raw_choice_index
            .and_then(|index| usize::try_from(index).ok())
            .filter(|index| *index < choice_texts.len())
            .ok_or_else(|| {
                BuiltinDocumentError::new(
                    index_location.to_string(),
                    BuiltinDocumentFailure::ChoiceIndexInvalid {
                        actual: raw_choice_index,
                        option_count: choice_texts.len(),
                    },
                )
            })?;
        if !branch_indexes.insert(choice_index) {
            return Err(BuiltinDocumentError::new(
                index_location.to_string(),
                BuiltinDocumentFailure::DuplicateChoiceBranch { choice_index },
            )
            .into());
        }
        let text_location = parameter_location(source, list_steps, branch_command_index, 1);
        let branch_text = parameter_string(command.parameters, 1, &text_location)?;
        if branch_text != choice_texts[choice_index] {
            return Err(BuiltinDocumentError::new(
                text_location.to_string(),
                BuiltinDocumentFailure::ChoiceBranchTextMismatch { choice_index },
            )
            .into());
        }
        covered_values.push(index_location);
        covered_values.push(text_location.clone());
        recipes.push(TextProjectionRecipe::Direct(
            DirectTextRecipe::new(
                text_location,
                branch_text,
                vec![DirectTextPart::LineSlot {
                    role: TextUnitRole::Choices,
                    source_line_index: choice_index,
                }],
            )
            .map_err(SnapshotModelError::Projection)?,
        ));
    }
    let Some(_) = end_location else {
        return Err(BuiltinDocumentError::new(
            command_location(source, list_steps, command_index).to_string(),
            BuiltinDocumentFailure::ChoiceEndMissing,
        )
        .into());
    };
    if branch_indexes.len() != choice_texts.len() {
        return Err(BuiltinDocumentError::new(
            command_location(source, list_steps, command_index).to_string(),
            BuiltinDocumentFailure::ChoiceBranchesIncomplete {
                expected: choice_texts.len(),
                actual: branch_indexes.len(),
            },
        )
        .into());
    }

    recipes.push(TextProjectionRecipe::Claim(
        MutationClaim::event_block(group_location.clone(), covered_values)
            .map_err(SnapshotModelError::Projection)?,
    ));

    let unit = ExtractedTextUnit::projected(
        TextUnitRole::Choices,
        choices_location,
        TextUnitContent::Lines(choice_texts),
    )?;
    groups.push(ExtractedTextGroup::projected(
        TextGroupKind::EventChoices,
        group_location,
        vec![unit],
        recipes,
    )?);
    Ok(())
}

fn extract_scrolling_text(
    source: &RpgMakerSource,
    list_steps: &[RpgMakerLocationStep],
    list: &[Value],
    start_index: usize,
    groups: &mut Vec<ExtractedTextGroup>,
) -> Result<usize, BuildBuiltinSnapshotError> {
    let mut lines = Vec::new();
    let mut next_index = start_index + 1;
    while next_index < list.len() {
        let command = command_at(source, list_steps, list, next_index)?;
        if command.code != 405 {
            break;
        }
        let exact_location = parameter_location(source, list_steps, next_index, 0);
        let text = parameter_string(command.parameters, 0, &exact_location)?;
        lines.push((exact_location, text.to_owned()));
        next_index += 1;
    }
    if lines.iter().any(|(_, text)| !text.trim().is_empty()) {
        let source_lines = lines
            .iter()
            .map(|(_, text)| text.clone())
            .collect::<Vec<_>>();
        let projection_location = lines
            .first()
            .expect("非空滚动文本必须有首个物理来源")
            .0
            .clone();
        let recipes = lines
            .into_iter()
            .enumerate()
            .map(|(source_line_index, (exact_location, text))| {
                DirectTextRecipe::new(
                    exact_location,
                    &text,
                    vec![DirectTextPart::LineSlot {
                        role: TextUnitRole::ScrollingText,
                        source_line_index,
                    }],
                )
                .map(TextProjectionRecipe::Direct)
                .map_err(SnapshotModelError::Projection)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let unit = ExtractedTextUnit::projected(
            TextUnitRole::ScrollingText,
            projection_location,
            TextUnitContent::Lines(source_lines),
        )?;
        groups.push(ExtractedTextGroup::projected(
            TextGroupKind::EventScrollingText,
            command_location(source, list_steps, start_index),
            vec![unit],
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

fn command_field_location(
    source: &RpgMakerSource,
    list_steps: &[RpgMakerLocationStep],
    command_index: usize,
    field_name: &str,
) -> RpgMakerLocation {
    let mut steps = list_steps.to_vec();
    steps.extend([
        RpgMakerLocationStep::index(command_index),
        RpgMakerLocationStep::key(field_name),
    ]);
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
    fields: &mut Vec<ExtractedTextUnit>,
    field_name: impl Into<String>,
    exact_location: RpgMakerLocation,
    original_text: &str,
) -> Result<(), SnapshotModelError> {
    if original_text.trim().is_empty() {
        return Ok(());
    }
    fields.push(ExtractedTextUnit::new(
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
    fields: Vec<ExtractedTextUnit>,
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
        .ok_or_else(|| {
            BuiltinDocumentError::new(
                format!("data/{}", file.file_name()),
                BuiltinDocumentFailure::MissingDocument,
            )
        })
}

fn expect_object(
    value: &Value,
    location: impl Into<String>,
) -> Result<&Map<String, Value>, BuiltinDocumentError> {
    value
        .as_object()
        .ok_or_else(|| BuiltinDocumentError::new(location, BuiltinDocumentFailure::ExpectedObject))
}

fn expect_array(
    value: &Value,
    location: impl Into<String>,
) -> Result<&[Value], BuiltinDocumentError> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| BuiltinDocumentError::new(location, BuiltinDocumentFailure::ExpectedArray))
}

fn expect_string<'a>(
    value: &'a Value,
    location: &RpgMakerLocation,
) -> Result<&'a str, BuiltinDocumentError> {
    value.as_str().ok_or_else(|| {
        BuiltinDocumentError::new(location.to_string(), BuiltinDocumentFailure::ExpectedString)
    })
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
    BuiltinDocumentError::new(location.to_string(), BuiltinDocumentFailure::MissingValue)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;
    use crate::progress::{ProgressAmount, ProgressObserver, ProgressSnapshot};

    #[derive(Clone, Default)]
    struct RecordingProgress(Arc<Mutex<Vec<ProgressSnapshot<ExtractProgressPhase>>>>);

    impl ProgressObserver<ExtractProgressPhase> for RecordingProgress {
        fn observe(&self, snapshot: ProgressSnapshot<ExtractProgressPhase>) {
            self.0.lock().expect("进度记录锁不应中毒").push(snapshot);
        }
    }

    impl RecordingProgress {
        fn snapshots(&self) -> Vec<ProgressSnapshot<ExtractProgressPhase>> {
            self.0.lock().expect("进度记录锁不应中毒").clone()
        }
    }

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

    impl SafeDiagnosticSource for FakeError {
        fn safe_diagnostic_source(
            &self,
            stage: DiagnosticStage,
            impact: DiagnosticImpact,
            fallback_action: DiagnosticAction,
        ) -> SafeDiagnostic {
            SafeDiagnostic::new(
                DiagnosticCode::InternalOperation,
                stage,
                DiagnosticSubject::component("typed_fake_root"),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::InvalidValue,
                    "source=fake_root",
                ),
                impact,
                fallback_action,
            )
        }
    }

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

        async fn execute_ordered_map<I, T, F>(
            &self,
            inputs: Vec<I>,
            operation: F,
        ) -> Result<Vec<T>, CpuTaskExecutionError<Self::Error>>
        where
            I: Send + 'static,
            T: Send + 'static,
            F: Fn(I) -> T + Send + Sync + 'static,
        {
            self.calls.fetch_add(inputs.len(), Ordering::SeqCst);
            self.max_active
                .fetch_max(inputs.len().min(self.root_limit), Ordering::SeqCst);
            Ok(inputs.into_iter().map(operation).collect())
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

    #[tokio::test]
    async fn parallel_scan_uses_the_root_ordered_map_and_keeps_serial_results() {
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
            complete_documents(),
            &BuiltinDialogueProjection::MzNative,
            ExtractProgress::default(),
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
            documents,
            &projection,
            ExtractProgress::default(),
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
        assert_eq!(
            unit_value(common_dialogue, TextUnitRole::DialogueSpeaker),
            "莉莉"
        );
        assert_eq!(
            unit_lines(common_dialogue, TextUnitRole::DialogueBody),
            ["你好", "   ", "第二行"]
        );
        let [TextProjectionRecipe::Dialogue(recipe)] = common_dialogue.recipes() else {
            panic!("MV 对话必须使用唯一块级配方");
        };
        assert_eq!(recipe.lines().len(), 3, "空白 401 也必须进入写回配方");
        assert!(matches!(
            recipe.lines()[1].parts(),
            [DialogueLinePart::BodyLine {
                source_line_index: 1
            }]
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
        assert_eq!(
            unit_value(map_dialogue, TextUnitRole::DialogueSpeaker),
            "バニー淫魔"
        );
        assert_eq!(
            unit_lines(map_dialogue, TextUnitRole::DialogueBody),
            ["「台词」"]
        );
        let [TextProjectionRecipe::Dialogue(recipe)] = map_dialogue.recipes() else {
            panic!("MV 对话必须使用唯一块级配方");
        };
        assert!(
            recipe.lines()[0]
                .parts()
                .iter()
                .all(|part| !matches!(part, DialogueLinePart::BodyLine { .. })),
            "整条第一行是姓名时不得进入正文单元"
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
    async fn root_cpu_budget_is_the_only_parallel_scan_limit() {
        let max_active = Arc::new(AtomicUsize::new(0));
        let cpu = RecordingCpu {
            calls: Arc::new(AtomicUsize::new(0)),
            active: Arc::new(AtomicUsize::new(0)),
            max_active: Arc::clone(&max_active),
            root_limit: 2,
        };

        build_builtin_snapshot_parallel(
            &cpu,
            complete_documents(),
            &BuiltinDialogueProjection::MzNative,
            ExtractProgress::default(),
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
        );

        let error = service
            .refresh(&project(), ExtractProgress::default())
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
        let progress = RecordingProgress::default();

        service
            .refresh(&project(), ExtractProgress::new(progress.clone()))
            .await
            .expect("完整文档应该被保存");

        let snapshots = progress.snapshots();
        let work_units = snapshots
            .iter()
            .filter(|snapshot| snapshot.phase == ExtractProgressPhase::BuiltinWorkUnits)
            .map(|snapshot| snapshot.amount)
            .collect::<Vec<_>>();
        assert_eq!(
            work_units,
            (0..=12)
                .map(|completed| ProgressAmount::Determinate {
                    completed,
                    total: 12
                })
                .collect::<Vec<_>>()
        );
        assert_eq!(
            snapshots.last(),
            Some(&ProgressSnapshot::indeterminate(
                ExtractProgressPhase::BuiltinCommit
            ))
        );

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
            MvDialogueDefinitionSelection::Replace {
                projector: definition.compile().expect("测试定义应能编译"),
                definition: definition.clone(),
            },
        );

        service
            .refresh(&project(), ExtractProgress::default())
            .await
            .expect("快照应成功保存");

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
            MvDialogueDefinitionSelection::ReuseProjectDefinition,
        );

        service
            .refresh(&project(), ExtractProgress::default())
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
                .units()
                .iter()
                .all(|unit| unit.role() != &TextUnitRole::DialogueSpeaker),
            "MV 不得把 101.parameters[4] 当作原生姓名"
        );
        assert_eq!(
            unit_lines(dialogue, TextUnitRole::DialogueBody),
            ["你好", "Welcome"]
        );
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
            MvDialogueDefinitionSelection::Replace {
                projector: definition.compile().expect("测试定义应能编译"),
                definition,
            },
        );

        let error = service
            .refresh(&project(), ExtractProgress::default())
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
            .refresh(&project(), ExtractProgress::default())
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
            .refresh(&project(), ExtractProgress::default())
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

    #[test]
    fn builtin_diagnostics_keep_document_location_snapshot_variant_and_typed_root_details() {
        type DiagnosticExtractionError = BuiltInExtractionError<
            FakeError,
            FakeError,
            crate::runtime::cpu::CpuExecutorUnavailable,
        >;

        let document_error = BuiltinDocumentError::new(
            "data/Items.json[1].name",
            BuiltinDocumentFailure::ExpectedString,
        );
        let error = DiagnosticExtractionError::MalformedDocument(document_error);
        let diagnostic = error.safe_diagnostic();
        assert_eq!(
            diagnostic.subject,
            DiagnosticSubject::field("data/Items.json[1].name")
        );
        let DiagnosticReason::FailureWithDetail { failure, detail } = diagnostic.reason else {
            panic!("Builtin 文档错误应公开具体结构原因")
        };
        assert_eq!(failure, DiagnosticFailureKind::InvalidValue);
        assert_eq!(detail, "structure=expected_string");

        let group_location = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(1)],
        );
        let error =
            DiagnosticExtractionError::BuildSnapshot(SnapshotModelError::ConflictingGroupKind {
                group_location: Box::new(group_location.clone()),
                first: TextGroupKind::DatabaseEntry,
                second: TextGroupKind::EventCommand,
            });
        let diagnostic = error.safe_diagnostic();
        assert_eq!(
            diagnostic.subject,
            DiagnosticSubject::field(group_location.to_string())
        );
        let DiagnosticReason::FailureWithDetail { failure, detail } = diagnostic.reason else {
            panic!("SnapshotModelError 应按具体变体投影")
        };
        assert_eq!(failure, DiagnosticFailureKind::ConflictingValues);
        assert!(detail.contains("first_kind=database_entry"));
        assert!(detail.contains("second_kind=event_command"));

        const SOURCE_BODY: &str = "ROOT_SOURCE_BODY_SENTINEL";
        let error = DiagnosticExtractionError::Persist(FakeError(SOURCE_BODY));
        let diagnostic = error.safe_diagnostic();
        assert_eq!(diagnostic.code, DiagnosticCode::InternalOperation);
        assert!(matches!(
            diagnostic.recovery.as_slice(),
            [RecoveryFact::Component { name }] if name == "owner=builtin"
        ));
        assert!(
            !serde_json::to_string(&diagnostic)
                .expect("Builtin 根诊断应可序列化")
                .contains(SOURCE_BODY),
            "根错误任意 Display 文本不得进入公开投影"
        );
    }

    #[tokio::test]
    async fn store_failure_keeps_persist_stage_and_source() {
        let service = service(Ok(complete_documents()), Some(FakeError("persist failed")));

        let error = service
            .refresh(&project(), ExtractProgress::default())
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

        assert_send(service.refresh(&project, ExtractProgress::default()));
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
        assert_eq!(scalar_value(item_group, "name"), "  宝剑  ");
        assert_eq!(scalar_value(item_group, "description"), "锋利的宝剑");

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
            .expect("应该提取公共事件对话组");
        assert_eq!(unit_value(dialogue, TextUnitRole::DialogueSpeaker), "莉莉");
        assert_eq!(
            unit_lines(dialogue, TextUnitRole::DialogueBody),
            ["你好", "Welcome"],
            "完整 401 正文必须形成一个语义单元"
        );

        let choices = snapshot
            .groups()
            .iter()
            .find(|group| group.kind() == TextGroupKind::EventChoices)
            .expect("应该提取选项组");
        assert_eq!(choices.units().len(), 1);
        assert_eq!(unit_lines(choices, TextUnitRole::Choices), ["接受", "拒绝"]);
        assert_eq!(
            choices.recipes().len(),
            5,
            "102、两个 402 与冻结 404 的结构 Claim 都必须物化"
        );
        assert_eq!(choices.mutation_claims().claims().len(), 1);

        assert!(snapshot.groups().iter().any(|group| {
            group
                .units()
                .iter()
                .any(|unit| matches!(unit.role(), TextUnitRole::Scalar(key) if key.as_str() == "message4"))
        }));
        assert!(snapshot.groups().iter().any(|group| {
            group
                .units()
                .iter()
                .any(|unit| matches!(unit.role(), TextUnitRole::Scalar(key) if key.as_str() == "terms.messages.alwaysDash"))
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
    fn builtin_units_follow_field_spec_and_numeric_array_order() {
        let mut documents = complete_documents();
        documents.insert_document(
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
                "elements": [null, null, "索引二", null, null, null, null, null, null, null, "索引十"],
                "skillTypes": [null, "魔法"],
                "weaponTypes": [null, "剑"],
                "armorTypes": [null, "轻甲"],
                "equipTypes": [null, "武器"]
            }),
        );

        let snapshot = build_builtin_snapshot(&documents).expect("Builtin 顺序夹具应能提取");
        let item = group_at(&snapshot, "data/Items.json[1]");
        assert_eq!(
            item.units()
                .iter()
                .map(|unit| match unit.role() {
                    TextUnitRole::Scalar(key) => key.as_str(),
                    role => panic!("数据库字段应是 Scalar，实际为 {role:?}"),
                })
                .collect::<Vec<_>>(),
            ["name", "description"],
            "Builtin 字段规格声明顺序不得被角色名排序覆盖"
        );
        let elements = group_at(&snapshot, "data/System.json.elements");
        assert_eq!(
            elements
                .units()
                .iter()
                .map(|unit| match unit.role() {
                    TextUnitRole::Scalar(key) => key.as_str(),
                    role => panic!("System 数组字段应是 Scalar，实际为 {role:?}"),
                })
                .collect::<Vec<_>>(),
            ["elements[2]", "elements[10]"],
            "数组索引必须按数值顺序，而不是角色字符串顺序"
        );
    }

    #[test]
    fn choices_require_complete_matching_same_indent_branches() {
        for list in [
            json!([
                {"code": 102, "indent": 0, "parameters": [["是", "否"]]},
                {"code": 402, "indent": 0, "parameters": [0, "错误文本"]},
                {"code": 402, "indent": 0, "parameters": [1, "否"]},
                {"code": 404, "indent": 0, "parameters": []}
            ]),
            json!([
                {"code": 102, "indent": 0, "parameters": [["是", "否"]]},
                {"code": 402, "indent": 0, "parameters": [0, "是"]},
                {"code": 404, "indent": 0, "parameters": []}
            ]),
        ] {
            let mut documents = complete_documents();
            documents.insert_document(
                RpgMakerDocumentId::Data(StandardDataFile::CommonEvents),
                json!([null, {"list": list}]),
            );

            assert!(matches!(
                build_builtin_snapshot(&documents),
                Err(BuildBuiltinSnapshotError::Malformed(_))
            ));
        }
    }

    #[test]
    fn outer_choices_claim_does_not_cover_branch_body_commands() {
        let mut documents = complete_documents();
        documents.insert_document(
            RpgMakerDocumentId::Data(StandardDataFile::CommonEvents),
            json!([null, {"list": [
                {"code": 102, "indent": 0, "parameters": [["外层一", "外层二"]]},
                {"code": 402, "indent": 0, "parameters": [0, "外层一"]},
                {"code": 101, "indent": 1, "parameters": ["", 0, 0, 2, "说话者"]},
                {"code": 401, "indent": 1, "parameters": ["分支对话"]},
                {"code": 105, "indent": 1, "parameters": []},
                {"code": 405, "indent": 1, "parameters": ["分支滚动文本"]},
                {"code": 320, "indent": 1, "parameters": [1, "新名字"]},
                {"code": 324, "indent": 1, "parameters": [1, "新昵称"]},
                {"code": 325, "indent": 1, "parameters": [1, "新简介"]},
                {"code": 102, "indent": 1, "parameters": [["内层选项"]]},
                {"code": 402, "indent": 1, "parameters": [0, "内层选项"]},
                {"code": 404, "indent": 1, "parameters": []},
                {"code": 402, "indent": 0, "parameters": [1, "外层二"]},
                {"code": 404, "indent": 0, "parameters": []},
                {"code": 0, "indent": 0, "parameters": []}
            ]}]),
        );

        let snapshot =
            build_builtin_snapshot(&documents).expect("外层选项不得占用分支正文命令的修改资源");
        let common_event_groups = snapshot.groups().iter().filter(|group| {
            group
                .group_location()
                .to_string()
                .starts_with("data/CommonEvents.json")
        });
        let mut kinds = common_event_groups
            .map(ExtractedTextGroup::kind)
            .collect::<Vec<_>>();
        kinds.sort();

        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == TextGroupKind::EventChoices)
                .count(),
            2,
            "外层与嵌套选项必须各自形成独立组"
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == TextGroupKind::EventDialogue)
                .count(),
            1
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == TextGroupKind::EventScrollingText)
                .count(),
            1
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|kind| **kind == TextGroupKind::EventCommand)
                .count(),
            3,
            "320、324、325 必须能与外层选项共存"
        );
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
                .units()
                .iter()
                .all(|unit| unit.role() != &TextUnitRole::DialogueSpeaker)
        );
        assert_eq!(
            unit_lines(dialogue, TextUnitRole::DialogueBody),
            ["   ", "正文"]
        );
        let [TextProjectionRecipe::Dialogue(recipe)] = dialogue.recipes() else {
            panic!("Builtin 对话必须使用唯一块级配方");
        };
        assert_eq!(recipe.lines().len(), 2);
        assert!(matches!(
            recipe.lines()[0].parts(),
            [DialogueLinePart::BodyLine {
                source_line_index: 0
            }]
        ));
    }

    #[test]
    fn keeps_blank_405_as_a_semantic_aligned_slot() {
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

        assert_eq!(
            unit_lines(scrolling, TextUnitRole::ScrollingText),
            ["第一行", "   ", "第三行"]
        );
        assert_eq!(scrolling.units().len(), 1);
        assert_eq!(scrolling.recipes().len(), 3);
        assert_eq!(scrolling.mutation_claims().claims().len(), 1);
        assert!(matches!(
            &scrolling.recipes()[1],
            TextProjectionRecipe::Direct(recipe)
                if recipe.expected_raw() == "   "
                    && matches!(recipe.parts(), [DirectTextPart::LineSlot {
                        role: TextUnitRole::ScrollingText,
                        source_line_index: 1,
                    }])
        ));
    }

    #[test]
    fn keeps_original_whitespace_skips_only_blank_and_does_not_filter_language() {
        let snapshot =
            build_builtin_snapshot(&complete_documents()).expect("完整最小 MZ 文档应该形成快照");
        let actor = group_at(&snapshot, "data/Actors.json[1]");

        assert_eq!(scalar_value(actor, "name"), "勇者");
        assert_eq!(scalar_value(actor, "profile"), " mixed 日本語 English ");
        assert!(
            actor
                .units()
                .iter()
                .all(|unit| !matches!(unit.role(), TextUnitRole::Scalar(key) if key.as_str() == "nickname")),
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
            RpgMakerDocumentId::Map(MapId::new(1).unwrap()),
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
                {"code": 102, "indent": 0, "parameters": [["接受", "拒绝"], -1, 0, 2, 0]},
                {"code": 402, "indent": 0, "parameters": [0, "接受"]},
                {"code": 402, "indent": 0, "parameters": [1, "拒绝"]},
                {"code": 404, "indent": 0, "parameters": []},
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
            RpgMakerDocumentId::Map(MapId::new(1).unwrap()),
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

    fn scalar_value<'a>(group: &'a ExtractedTextGroup, field_name: &str) -> &'a str {
        group
            .units()
            .iter()
            .find(|unit| {
                matches!(unit.role(), TextUnitRole::Scalar(key) if key.as_str() == field_name)
            })
            .unwrap_or_else(|| panic!("缺少字段 {field_name}"))
            .source_content()
            .as_value()
            .expect("标量单元必须保存单值原文")
    }

    fn unit_value(group: &ExtractedTextGroup, role: TextUnitRole) -> &str {
        group
            .units()
            .iter()
            .find(|unit| unit.role() == &role)
            .unwrap_or_else(|| panic!("缺少语义单元 {role:?}"))
            .source_content()
            .as_value()
            .expect("指定语义单元必须保存单值原文")
    }

    fn unit_lines(group: &ExtractedTextGroup, role: TextUnitRole) -> &[String] {
        group
            .units()
            .iter()
            .find(|unit| unit.role() == &role)
            .unwrap_or_else(|| panic!("缺少语义单元 {role:?}"))
            .source_content()
            .as_lines()
            .expect("指定语义单元必须保存有序原文行")
    }
}
