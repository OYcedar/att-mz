//! RPG Maker 翻译任务规划：自然排序、语义范围、虚原文、术语和占位符。

use std::collections::{BTreeMap, BTreeSet};
#[cfg(test)]
use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::io;
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::sync::Arc;

use crate::diagnostic::{
    Diagnostic, DiagnosticReport, IoFailure, ReportedFailure, RpgMakerDiagnosticScope,
    RpgMakerIssue, RpgMakerOutputContractViolation, RpgMakerPlaceholderMultisetViolation,
    RpgMakerPlaceholderProjectionProblem, RpgMakerTranslationPlanningProblem, RuntimeComponent,
    RuntimeIssue, RuntimeOperation, SafeIdentifier, StateEffect, TranslationIssue,
    TranslationPlanningResourceOrigin, TranslationTaskPlanningProblem,
};
use crate::execution::CooperativeCancellation;
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::execution::isolated::{IsolatedOperationError, run_isolated_operation};
use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::language::{LanguageAnalysis, LanguageOperationCancelled, LanguagePair};
use crate::llm::{ChatMessage, ChatMessageRole, LlmClientConcurrency, LlmClientSemanticIdentity};
use crate::rpg_maker::RpgMakerEngine;
use crate::rpg_maker::location_codec::{RpgMakerLocationCodec, RpgMakerProjectionCodec};
use crate::rpg_maker::model::{TextUnitContent, TextUnitRole};
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::semantic_order::{RpgMakerSemanticOrderKey, RpgMakerSemanticScopeKey};
use crate::rpg_maker::text::{RpgMakerSource, StandardDataFile, TextGroupKind};
use crate::runtime::cpu::CpuExecutorUnavailable;
use crate::runtime::filesystem::SystemFileSystemError;
use crate::storage::file_system::ReadFileError;
use crate::translation::placeholder_projection::LanguageTextProjectionError;
use crate::translation::task_planning::{
    AssignedTaskBlock, StableGroupCharacters, TaskId, TaskPlanningError, TaskPlanningGroupLayout,
    TaskPlanningLayout, TaskPlanningScopeLayout, UnitTaskResponsibility, assign_task_ids,
    pack_complete_task_blocks,
};
use crate::translation::user_message::{
    TranslationReturnType, TranslationUserGroup, TranslationUserMessage,
    TranslationUserTerminology, TranslationUserUnit, measure_translation_user_group,
    render_translation_user_message,
};
use crate::translation_protocol::TranslationResponseMode;

use super::deduplication::{
    TranslationDeduplicationCandidate, TranslationDeduplicationOutcome,
    deduplicate_translation_candidates,
};
#[cfg(test)]
use super::pipeline::RpgMakerTranslationGroup;
use super::pipeline::{
    ExpectedLineShape, ExpectedTranslationOutput, ExpectedTranslationOutputContractError,
    ExpectedTranslationValidation, GroupContextFingerprint, RpgMakerExecutableTask,
    RpgMakerTranslationCorpus, RpgMakerTranslationInput, RpgMakerTranslationPlan,
    RpgMakerTranslationScope, RpgMakerTranslationTaskIndex, RpgMakerTranslationTaskPlanner,
    TerminologyDependency, TranslationInvalidation, TranslationPlaceholderProjectionFailure,
    TranslationPlaceholderProtectionFailure, TranslationPlanPreparation,
    TranslationPlanPreparationCounts, TranslationPlanningFailure, TranslationPlanningFailureReason,
    TranslationPropagationTarget, TranslationStateContext, TranslationUnitIdentity,
    TranslationVirtualReason,
};
use super::placeholder::{
    Pcre2PlaceholderService, PlaceholderProtectionError, PlaceholderRuleCompilationError,
};
use super::profile::{ResolvedRpgMakerTranslationResources, RpgMakerTranslationProfile};
use super::semantics::{
    GroupContextFingerprintError, PreparedTranslationStatus, ResolvedTranslationSemanticError,
    ResolvedTranslationSemantics,
    group_context_fingerprint_with_cancellation as shared_group_context_fingerprint_with_cancellation,
};
use crate::translation::planning_resource::{
    CompiledTerminology, TranslationPlanningResourceReader, TranslationPlanningResourceReadingError,
};

trait TranslationPlanningResourceErrorCancellation {
    fn is_cancelled_error(&self) -> bool;
}

trait TranslationPlanningFileErrorCancellation {
    fn is_cancelled_error(&self) -> bool;
}

impl TranslationPlanningFileErrorCancellation for SystemFileSystemError {
    fn is_cancelled_error(&self) -> bool {
        matches!(self, Self::Cancelled { .. })
    }
}

impl<F, C> TranslationPlanningResourceErrorCancellation
    for TranslationPlanningResourceReadingError<F, C>
where
    F: TranslationPlanningFileErrorCancellation,
{
    fn is_cancelled_error(&self) -> bool {
        match self {
            Self::Cancelled
            | Self::ParseTerminologyCompute {
                source: CpuTaskExecutionError::Cancelled,
                ..
            }
            | Self::ParsePlaceholderRulesCompute {
                source: CpuTaskExecutionError::Cancelled,
                ..
            } => true,
            Self::ReadTerminology {
                source: ReadFileError::Io { source, .. },
                ..
            }
            | Self::ReadPlaceholderRules {
                source: ReadFileError::Io { source, .. },
                ..
            } => source.is_cancelled_error(),
            Self::ReadTerminology { .. }
            | Self::ReadPlaceholderRules { .. }
            | Self::ParseTerminologyCompute { .. }
            | Self::InvalidTerminology { .. }
            | Self::ParsePlaceholderRulesCompute { .. }
            | Self::InvalidPlaceholderRules { .. } => false,
        }
    }
}

/// 使用三个职责模块与 CPU 根建立确定性 RPG Maker 翻译计划。
pub(crate) struct RpgMakerTranslationTaskPlanningService<R, C, L> {
    resources: R,
    translation_resources: Arc<ResolvedRpgMakerTranslationResources>,
    placeholders: Pcre2PlaceholderService,
    cpu: C,
    cancellation: CooperativeCancellation,
    llm_client: PhantomData<fn() -> L>,
}

impl<R, C, L> RpgMakerTranslationTaskPlanningService<R, C, L> {
    pub(crate) fn new(
        resources: R,
        translation_resources: Arc<ResolvedRpgMakerTranslationResources>,
        placeholders: Pcre2PlaceholderService,
        cpu: C,
    ) -> Self {
        Self {
            resources,
            translation_resources,
            placeholders,
            cpu,
            cancellation: CooperativeCancellation::default(),
            llm_client: PhantomData,
        }
    }

    pub(crate) fn with_cancellation(mut self, cancellation: CooperativeCancellation) -> Self {
        self.cancellation = cancellation;
        self
    }
}

struct ResolvedCorpusSemantics {
    scopes: Vec<RpgMakerTranslationScope>,
    snapshot_baseline: super::pipeline::TranslationSnapshotBaseline,
    terminology: Arc<CompiledTerminology>,
    terminology_json: String,
    placeholder_rules_json: String,
    semantics: Arc<ResolvedTranslationSemantics>,
    system_markdown: String,
    task_language_pair: LanguagePair,
    placeholder_rule_source: super::pipeline::TranslationPlaceholderRuleSource,
}

impl<R, C, L> RpgMakerTranslationTaskPlanningService<R, C, L>
where
    R: TranslationPlanningResourceReader,
    C: CpuTaskExecutor,
    L: LlmClientConcurrency + LlmClientSemanticIdentity + 'static,
{
    async fn resolve_corpus_semantics(
        &self,
        project: &OpenedProject,
        profile: &Arc<RpgMakerTranslationProfile<L>>,
        corpus: RpgMakerTranslationCorpus,
        input: RpgMakerTranslationInput,
    ) -> Result<ResolvedCorpusSemantics, RpgMakerTranslationTaskPlanningError<R::Error, C::Error>>
    {
        let resolved_pair = self.translation_resources.language_pair();
        if project.source_language() != resolved_pair.source()
            || project.target_language() != resolved_pair.target()
        {
            return Err(
                RpgMakerTranslationTaskPlanningError::ResolvedLanguagePairMismatch {
                    project_source: project.source_language().to_string(),
                    project_target: project.target_language().to_string(),
                    resolved_source: resolved_pair.source().to_string(),
                    resolved_target: resolved_pair.target().to_string(),
                },
            );
        }
        let source_language = self.translation_resources.source_language();
        let (scopes, snapshot_baseline) = corpus.into_parts();
        let context_resources = Arc::clone(&self.translation_resources);
        let context_cancellation = self.cancellation.clone();
        let engine = project.layout().rpg_maker_layout().engine();
        let source_language_id = project.source_language().to_owned();
        let target_language_id = project.target_language().to_owned();
        let context_source_language = Arc::clone(&source_language);
        let context_profile = Arc::clone(profile);
        let prepared_context = self
            .cpu
            .execute(move || {
                ensure_planner_cpu_running(&context_cancellation)?;
                let language_semantics = context_source_language
                    .semantic_fingerprint_with_cancellation(&mut || {
                        ensure_planner_language_running(&context_cancellation)
                    })
                    .map_err(|LanguageOperationCancelled| ())?;
                ensure_planner_cpu_running(&context_cancellation)?;
                let client_semantics = context_profile.llm_client().semantic_fingerprint();
                ensure_planner_cpu_running(&context_cancellation)?;
                let system_markdown = clone_planner_text_with_cancellation(
                    context_resources.system_prompt().markdown(),
                    &context_cancellation,
                )?;
                let current_terminology_json = clone_planner_text_with_cancellation(
                    snapshot_baseline.terminology_json(),
                    &context_cancellation,
                )?;
                let current_placeholder_rules_json = clone_planner_text_with_cancellation(
                    snapshot_baseline.placeholder_rules_json(),
                    &context_cancellation,
                )?;
                let global_semantics = global_translation_semantics_with_cancellation(
                    engine,
                    context_resources.language_pair(),
                    language_semantics,
                    &system_markdown,
                    context_resources.system_prompt().response_mode(),
                    client_semantics,
                    || ensure_planner_cpu_running(&context_cancellation),
                )?;
                Ok::<_, ()>((
                    scopes,
                    snapshot_baseline,
                    current_terminology_json,
                    current_placeholder_rules_json,
                    system_markdown,
                    global_semantics,
                    source_language_id,
                    target_language_id,
                ))
            })
            .await
            .map_err(RpgMakerTranslationTaskPlanningError::PrepareResourcesCompute)?;
        let (
            scopes,
            snapshot_baseline,
            current_terminology_json,
            current_placeholder_rules_json,
            system_markdown,
            global_semantics,
            source_language_id,
            target_language_id,
        ) = match prepared_context {
            Ok(context) => context,
            Err(()) => {
                return Err(
                    RpgMakerTranslationTaskPlanningError::PrepareResourcesCompute(
                        CpuTaskExecutionError::Cancelled,
                    ),
                );
            }
        };
        let (terminology_path, placeholder_rules_path) = input.into_parts();
        let placeholder_rule_source = placeholder_rules_path.as_ref().map_or(
            super::pipeline::TranslationPlaceholderRuleSource::ProjectSnapshot,
            |path| super::pipeline::TranslationPlaceholderRuleSource::ExternalFile(path.clone()),
        );
        let resources = self
            .resources
            .read(
                terminology_path,
                placeholder_rules_path,
                current_terminology_json,
                current_placeholder_rules_json,
            )
            .await
            .map_err(RpgMakerTranslationTaskPlanningError::ReadResources)?;
        let (terminology, placeholder_definitions, terminology_json, placeholder_rules_json) =
            resources.into_parts();

        let placeholder_service = self.placeholders.clone();
        let placeholder_cancellation = self.cancellation.clone();
        let custom_placeholders = self
            .cpu
            .execute(move || {
                placeholder_service
                    .compile_custom_with_cancellation(placeholder_definitions, || {
                        ensure_planner_cpu_running(&placeholder_cancellation)
                    })
            })
            .await
            .map_err(RpgMakerTranslationTaskPlanningError::CompilePlaceholdersCompute)?;
        let custom_placeholders = match custom_placeholders {
            Err(()) => {
                return Err(
                    RpgMakerTranslationTaskPlanningError::CompilePlaceholdersCompute(
                        CpuTaskExecutionError::Cancelled,
                    ),
                );
            }
            Ok(result) => {
                let origin = placeholder_rule_source.clone();
                result.map_err(|source| {
                    RpgMakerTranslationTaskPlanningError::InvalidPlaceholderRules { origin, source }
                })?
            }
        };
        let task_language_pair = LanguagePair::new(source_language_id, target_language_id);
        let semantics = Arc::new(ResolvedTranslationSemantics::new(
            engine,
            task_language_pair.clone(),
            Arc::clone(&terminology),
            self.placeholders.clone(),
            custom_placeholders,
            source_language,
            global_semantics,
        ));
        Ok(ResolvedCorpusSemantics {
            scopes,
            snapshot_baseline,
            terminology,
            terminology_json,
            placeholder_rules_json,
            semantics,
            system_markdown,
            task_language_pair,
            placeholder_rule_source,
        })
    }
}

impl<R, C, L> RpgMakerTranslationTaskPlanner for RpgMakerTranslationTaskPlanningService<R, C, L>
where
    R: TranslationPlanningResourceReader,
    R::Error: TranslationPlanningResourceErrorCancellation,
    C: CpuTaskExecutor,
    L: LlmClientConcurrency + LlmClientSemanticIdentity + 'static,
{
    type Profile = Arc<RpgMakerTranslationProfile<L>>;
    type Error = RpgMakerTranslationTaskPlanningError<R::Error, C::Error>;

    fn is_cancelled_error(error: &Self::Error) -> bool {
        error.is_cancelled()
    }

    async fn plan(
        &self,
        project: &OpenedProject,
        profile: &Self::Profile,
        corpus: RpgMakerTranslationCorpus,
        input: RpgMakerTranslationInput,
    ) -> Result<RpgMakerTranslationPlan, Self::Error> {
        let planning = profile.planning();
        let ResolvedCorpusSemantics {
            scopes,
            snapshot_baseline,
            terminology,
            terminology_json,
            placeholder_rules_json,
            semantics,
            system_markdown,
            task_language_pair,
            placeholder_rule_source,
        } = self
            .resolve_corpus_semantics(project, profile, corpus, input)
            .await?;

        let prepared = self
            .cpu
            .execute({
                let cancellation = self.cancellation.clone();
                move || prepare_corpus_with_cancellation(scopes, &cancellation)
            })
            .await
            .map_err(RpgMakerTranslationTaskPlanningError::PrepareCorpusCompute)?;
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(PrepareCorpusFailure::Cancelled) => {
                return Err(RpgMakerTranslationTaskPlanningError::PrepareCorpusCompute(
                    CpuTaskExecutionError::Cancelled,
                ));
            }
            Err(PrepareCorpusFailure::Invalid(source)) => {
                return Err(RpgMakerTranslationTaskPlanningError::InvalidCorpus(source));
            }
        };

        let target_user_message_characters = planning.target_user_message_characters();
        let stable_planning = self
            .cpu
            .execute({
                let cancellation = self.cancellation.clone();
                move || {
                    let layout = task_planning_layout_with_cancellation(&prepared, &cancellation)?;
                    let complete_plan = pack_complete_task_blocks(
                        &layout,
                        target_user_message_characters,
                        &cancellation,
                    )
                    .map_err(ScopeTaskPlanningFailure::TaskPlanning)?;
                    Ok::<_, ScopeTaskPlanningFailure>((prepared, complete_plan))
                }
            })
            .await
            .map_err(RpgMakerTranslationTaskPlanningError::PlanScopesCompute)?;
        let (prepared, complete_plan) = match stable_planning {
            Ok(planning) => planning,
            Err(ScopeTaskPlanningFailure::Cancelled) => {
                return Err(RpgMakerTranslationTaskPlanningError::PlanScopesCompute(
                    CpuTaskExecutionError::Cancelled,
                ));
            }
            Err(ScopeTaskPlanningFailure::TaskPlanning(source)) => {
                return Err(RpgMakerTranslationTaskPlanningError::TaskPlanning(source));
            }
            Err(ScopeTaskPlanningFailure::InvalidContract(_)) => {
                unreachable!("稳定装箱尚未建立模型输出契约")
            }
        };
        let scope_semantics = Arc::clone(&semantics);
        let scope_cancellation = self.cancellation.clone();
        let preprocessed_scopes = self
            .cpu
            .execute_ordered_map(prepared.scopes, move |scope| {
                let scope_name = scope.key.clone();
                (
                    scope_name,
                    preprocess_scope(scope, Arc::clone(&scope_semantics), &scope_cancellation),
                )
            })
            .await
            .map_err(RpgMakerTranslationTaskPlanningError::PreprocessScopesCompute)?;

        let deduplication_cancellation = self.cancellation.clone();
        let (scopes, invalidations, reuses, preparation_counts, complete_plan) = self
            .cpu
            .execute(move || {
                match run_isolated_operation(
                    "att-rpg-maker-deduplication",
                    move || {
                        let mut scopes = Vec::with_capacity(preprocessed_scopes.len());
                        for (scope, result) in preprocessed_scopes {
                            let result = result.map_err(|source| match source {
                                ScopePreprocessingFailure::Cancelled => {
                                    GlobalPreparationFailure::Cancelled
                                }
                                ScopePreprocessingFailure::UnitPreparation(source) => {
                                    GlobalPreparationFailure::UnitPreparation(source)
                                }
                                ScopePreprocessingFailure::Invalid(source) => {
                                    GlobalPreparationFailure::InvalidScopePreprocessing {
                                        scope,
                                        source,
                                    }
                                }
                            })?;
                            scopes.push(result.scope);
                        }
                        let (candidates, positions, mut invalidations) =
                            collect_deduplication_inputs(&scopes, &complete_plan);
                        let deduplicated = deduplicate_translation_candidates(candidates);
                        let (outcomes, deduplication_invalidations, reuses) =
                            deduplicated.into_parts();
                        invalidations.extend(deduplication_invalidations);
                        apply_deduplication_outcomes(&mut scopes, positions, outcomes);

                        let retained = scopes
                            .iter()
                            .flat_map(|scope| &scope.groups)
                            .flat_map(|group| &group.units)
                            .filter(|unit| unit.current)
                            .count();
                        let not_applicable = scopes
                            .iter()
                            .flat_map(|scope| &scope.groups)
                            .flat_map(|group| &group.units)
                            .filter(|unit| unit.not_applicable)
                            .count();
                        let invalidated = scopes
                            .iter()
                            .flat_map(|scope| &scope.groups)
                            .flat_map(|group| &group.units)
                            .filter(|unit| unit.invalidated && !unit.not_applicable)
                            .count();

                        Ok::<_, GlobalPreparationFailure>((
                            scopes,
                            invalidations,
                            reuses,
                            TranslationPlanPreparationCounts::new(
                                retained,
                                invalidated,
                                not_applicable,
                            ),
                            complete_plan,
                        ))
                    },
                    || ensure_planner_cpu_running(&deduplication_cancellation),
                ) {
                    Ok(result) => result,
                    Err(IsolatedOperationError::Cancelled(())) => {
                        Err(GlobalPreparationFailure::Cancelled)
                    }
                    Err(IsolatedOperationError::Start { operation, source }) => {
                        Err(GlobalPreparationFailure::StartWorker { operation, source })
                    }
                }
            })
            .await
            .map_err(RpgMakerTranslationTaskPlanningError::DeduplicateCompute)?
            .map_err(|failure| match failure {
                GlobalPreparationFailure::InvalidScopePreprocessing { scope, source } => {
                    RpgMakerTranslationTaskPlanningError::InvalidScopePreprocessing {
                        scope,
                        source,
                    }
                }
                GlobalPreparationFailure::Cancelled => {
                    RpgMakerTranslationTaskPlanningError::DeduplicateCompute(
                        CpuTaskExecutionError::Cancelled,
                    )
                }
                GlobalPreparationFailure::UnitPreparation(source) => {
                    RpgMakerTranslationTaskPlanningError::UnitPreparation(
                        source.with_rule_source(placeholder_rule_source.clone()),
                    )
                }
                GlobalPreparationFailure::StartWorker { operation, source } => {
                    RpgMakerTranslationTaskPlanningError::StartDeduplicationWorker {
                        operation,
                        source,
                    }
                }
            })?;

        let assignment_cancellation = self.cancellation.clone();
        let assigned = self
            .cpu
            .execute(move || {
                assign_complete_task_plan(scopes, complete_plan, &assignment_cancellation)
            })
            .await
            .map_err(RpgMakerTranslationTaskPlanningError::PlanScopesCompute)?;
        let (scopes, assigned) = match assigned {
            Ok(assigned) => assigned,
            Err(ScopeTaskPlanningFailure::Cancelled) => {
                return Err(RpgMakerTranslationTaskPlanningError::PlanScopesCompute(
                    CpuTaskExecutionError::Cancelled,
                ));
            }
            Err(ScopeTaskPlanningFailure::TaskPlanning(source)) if source.is_cancelled() => {
                return Err(RpgMakerTranslationTaskPlanningError::PlanScopesCompute(
                    CpuTaskExecutionError::Cancelled,
                ));
            }
            Err(ScopeTaskPlanningFailure::TaskPlanning(source)) => {
                return Err(RpgMakerTranslationTaskPlanningError::TaskPlanning(source));
            }
            Err(ScopeTaskPlanningFailure::InvalidContract(_)) => {
                unreachable!("分配 Task ID 尚未建立模型输出契约")
            }
        };

        // 术语提示词行只渲染一次。每个实际发送块会合并完整 Group 范围中的全部命中，
        // 再按术语文件自然顺序访问稀疏下标。
        let terminology_index_cancellation = self.cancellation.clone();
        let terminology_for_index = Arc::clone(&terminology);
        let terminology_prompt = self
            .cpu
            .execute(move || {
                TerminologyPromptIndex::new_with_cancellation(
                    &terminology_for_index,
                    &terminology_index_cancellation,
                )
            })
            .await
            .map_err(RpgMakerTranslationTaskPlanningError::PlanScopesCompute)?;
        let terminology_prompt = match terminology_prompt {
            Ok(index) => Arc::new(index),
            Err(ScopeTaskPlanningFailure::Cancelled) => {
                return Err(RpgMakerTranslationTaskPlanningError::PlanScopesCompute(
                    CpuTaskExecutionError::Cancelled,
                ));
            }
            Err(ScopeTaskPlanningFailure::TaskPlanning(source)) => {
                return Err(RpgMakerTranslationTaskPlanningError::TaskPlanning(source));
            }
            Err(ScopeTaskPlanningFailure::InvalidContract(source)) => {
                return Err(RpgMakerTranslationTaskPlanningError::InvalidOutputContract(
                    source,
                ));
            }
        };
        let materialization_scopes = Arc::new(scopes);
        let materialization_blocks = assigned.blocks_with_task_ids().cloned().collect::<Vec<_>>();
        let materialization_terminology = Arc::clone(&terminology_prompt);
        let materialization_system_markdown = Arc::new(system_markdown);
        let materialization_semantics = Arc::clone(&semantics);
        let materialization_cancellation = self.cancellation.clone();
        let materialized_blocks = self
            .cpu
            .execute_ordered_map(materialization_blocks, move |block| {
                materialize_task_block(
                    materialization_scopes.as_slice(),
                    block,
                    &materialization_terminology,
                    &materialization_semantics,
                    materialization_system_markdown.as_str(),
                    &materialization_cancellation,
                )
            })
            .await
            .map_err(RpgMakerTranslationTaskPlanningError::PlanScopesCompute)?;

        let finalization_cancellation = self.cancellation.clone();
        let tasks = self
            .cpu
            .execute(move || {
                ensure_planner_cpu_running(&finalization_cancellation)
                    .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
                let mut tasks = Vec::new();
                for materialized in materialized_blocks {
                    ensure_planner_cpu_running(&finalization_cancellation)
                        .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
                    let task = materialized?;
                    let index = RpgMakerTranslationTaskIndex::new(tasks.len());
                    tasks.push(task.with_index(index, task_language_pair.clone()));
                }
                ensure_planner_cpu_running(&finalization_cancellation)
                    .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
                Ok::<_, ScopeTaskPlanningFailure>(tasks)
            })
            .await
            .map_err(RpgMakerTranslationTaskPlanningError::FinalizePlanCompute)?;
        let tasks = match tasks {
            Ok(tasks) => tasks,
            Err(ScopeTaskPlanningFailure::Cancelled) => {
                return Err(RpgMakerTranslationTaskPlanningError::FinalizePlanCompute(
                    CpuTaskExecutionError::Cancelled,
                ));
            }
            Err(ScopeTaskPlanningFailure::TaskPlanning(source)) => {
                return Err(RpgMakerTranslationTaskPlanningError::TaskPlanning(source));
            }
            Err(ScopeTaskPlanningFailure::InvalidContract(source)) => {
                return Err(RpgMakerTranslationTaskPlanningError::InvalidOutputContract(
                    source,
                ));
            }
        };

        Ok(RpgMakerTranslationPlan::new(
            Arc::clone(&semantics),
            TranslationPlanPreparation::with_baseline(
                invalidations,
                reuses,
                terminology_json,
                placeholder_rules_json,
                preparation_counts,
                snapshot_baseline,
            ),
            tasks,
        ))
    }
}

pub(super) struct PreparedCorpus {
    scopes: Vec<PreparedScope>,
}

pub(super) struct PreparedScope {
    key: RpgMakerSemanticScopeKey,
    groups: Vec<PreparedGroup>,
}

pub(super) struct PreparedGroup {
    kind: TextGroupKind,
    semantic_order_key: RpgMakerSemanticOrderKey,
    assets: Vec<PreparedAsset>,
}

pub(super) struct PreparedAsset {
    identity: TranslationUnitIdentity,
    semantic_order_key: RpgMakerSemanticOrderKey,
    translation: Option<TextUnitContent>,
    translation_state: Option<Sha256Fingerprint>,
    manual: bool,
}

#[cfg(test)]
pub(super) fn prepare_corpus(
    groups: Vec<RpgMakerTranslationGroup>,
) -> Result<PreparedCorpus, CorpusPlanningError> {
    let corpus = RpgMakerTranslationCorpus::new(groups);
    let (scopes, _) = corpus.into_parts();
    match prepare_corpus_with_cancellation(scopes, &CooperativeCancellation::default()) {
        Ok(prepared) => Ok(prepared),
        Err(PrepareCorpusFailure::Invalid(source)) => Err(source),
        Err(PrepareCorpusFailure::Cancelled) => {
            unreachable!("未请求取消的语料准备不能取消")
        }
    }
}

fn prepare_corpus_with_cancellation(
    source_scopes: Vec<RpgMakerTranslationScope>,
    cancellation: &CooperativeCancellation,
) -> Result<PreparedCorpus, PrepareCorpusFailure> {
    ensure_planner_cpu_running(cancellation).map_err(|()| PrepareCorpusFailure::Cancelled)?;
    let mut scopes = Vec::<PreparedScope>::with_capacity(source_scopes.len());
    for scope in source_scopes {
        ensure_planner_cpu_running(cancellation).map_err(|()| PrepareCorpusFailure::Cancelled)?;
        let key = scope.key().clone();
        let source_groups = scope.into_groups();
        if source_groups.is_empty() {
            return Err(PrepareCorpusFailure::Invalid(
                CorpusPlanningError::EmptySemanticScope { scope: key },
            ));
        }
        let mut groups = Vec::with_capacity(source_groups.len());
        for group in source_groups {
            ensure_planner_cpu_running(cancellation)
                .map_err(|()| PrepareCorpusFailure::Cancelled)?;
            let kind = group.kind();
            let semantic_order_key = group.semantic_order_key().clone();
            let source_assets = group.into_assets();
            if source_assets.is_empty() {
                return Err(PrepareCorpusFailure::Invalid(
                    CorpusPlanningError::EmptyGroup {
                        scope: key.clone(),
                        kind,
                    },
                ));
            }
            let mut assets = Vec::with_capacity(source_assets.len());
            for asset in source_assets {
                ensure_planner_cpu_running(cancellation)
                    .map_err(|()| PrepareCorpusFailure::Cancelled)?;
                let (identity, semantic_order_key, translation, translation_state, manual) =
                    asset.into_parts();
                assets.push(PreparedAsset {
                    identity,
                    semantic_order_key,
                    translation,
                    translation_state,
                    manual,
                });
            }
            groups.push(PreparedGroup {
                kind,
                semantic_order_key,
                assets,
            });
        }
        scopes.push(PreparedScope { key, groups });
    }
    ensure_planner_cpu_running(cancellation).map_err(|()| PrepareCorpusFailure::Cancelled)?;
    Ok(PreparedCorpus { scopes })
}

fn task_planning_layout_with_cancellation(
    corpus: &PreparedCorpus,
    cancellation: &CooperativeCancellation,
) -> Result<TaskPlanningLayout, ScopeTaskPlanningFailure> {
    ensure_planner_cpu_running(cancellation).map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
    let mut scopes = Vec::with_capacity(corpus.scopes.len());
    for scope in &corpus.scopes {
        ensure_planner_cpu_running(cancellation)
            .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
        let mut groups = Vec::with_capacity(scope.groups.len());
        for group in &scope.groups {
            ensure_planner_cpu_running(cancellation)
                .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
            let mut stable_units = Vec::with_capacity(group.assets.len());
            for asset in &group.assets {
                ensure_planner_cpu_running(cancellation)
                    .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
                let source_text = task_content_text_with_cancellation(
                    asset.identity.source_content(),
                    cancellation,
                )?;
                stable_units.push((asset.identity.role_label(), source_text));
            }
            let wire_units = stable_units
                .iter()
                .map(|(role, text)| {
                    TranslationUserUnit::context(Some(role.as_str()), text.as_str())
                })
                .collect();
            let wire_group = TranslationUserGroup::new(group.kind.storage_name(), wire_units);
            let Some((first_in_block, following_in_block)) =
                measure_translation_user_group(&wire_group, cancellation)
                    .map_err(|_| ScopeTaskPlanningFailure::Cancelled)?
            else {
                return Err(ScopeTaskPlanningFailure::TaskPlanning(
                    TaskPlanningError::CharacterCountOverflow,
                ));
            };
            groups.push(
                TaskPlanningGroupLayout::new(
                    group.assets.len(),
                    StableGroupCharacters::new(first_in_block, following_in_block),
                )
                .map_err(ScopeTaskPlanningFailure::TaskPlanning)?,
            );
        }
        scopes.push(
            TaskPlanningScopeLayout::new(groups).map_err(ScopeTaskPlanningFailure::TaskPlanning)?,
        );
    }
    TaskPlanningLayout::new(scopes).map_err(ScopeTaskPlanningFailure::TaskPlanning)
}

fn task_content_text_with_cancellation(
    content: &TextUnitContent,
    cancellation: &CooperativeCancellation,
) -> Result<String, ScopeTaskPlanningFailure> {
    match content {
        TextUnitContent::Value(value) => clone_planner_text_with_cancellation(value, cancellation)
            .map_err(|()| ScopeTaskPlanningFailure::Cancelled),
        TextUnitContent::Lines(lines) => {
            let mut capacity = lines.len().saturating_sub(1);
            for line in lines {
                ensure_planner_cpu_running(cancellation)
                    .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
                capacity = capacity.checked_add(line.len()).ok_or(
                    ScopeTaskPlanningFailure::TaskPlanning(
                        TaskPlanningError::CharacterCountOverflow,
                    ),
                )?;
            }
            let mut text = String::with_capacity(capacity);
            for (index, line) in lines.iter().enumerate() {
                ensure_planner_cpu_running(cancellation)
                    .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
                if index != 0 {
                    text.push('\n');
                }
                append_planner_text_with_cancellation(&mut text, line, cancellation)?;
            }
            Ok(text)
        }
    }
}

enum PrepareCorpusFailure {
    Cancelled,
    Invalid(CorpusPlanningError),
}

struct PreprocessedScope {
    groups: Vec<PreprocessedGroup>,
}

struct PreprocessedScopeResult {
    scope: PreprocessedScope,
}

struct PreprocessedGroup {
    kind: TextGroupKind,
    /// 完整 Group 内所有 Unit 实际命中的术语文件索引。
    triggered_terms: Vec<usize>,
    units: Vec<PreprocessedUnit>,
}

struct PreprocessedUnit {
    identity: TranslationUnitIdentity,
    protected_text: String,
    placeholders: Vec<super::pipeline::AppliedPlaceholder>,
    language_analysis: LanguageAnalysis,
    translation: Option<TextUnitContent>,
    translation_state: Option<Sha256Fingerprint>,
    invalidated: bool,
    state_context: TranslationStateContext,
    current: bool,
    not_applicable: bool,
    responsibility: PreparedUnitResponsibility,
}

#[derive(Clone)]
enum PreparedUnitResponsibility {
    AwaitingDeduplication,
    Active {
        propagation_targets: Vec<TranslationPropagationTarget>,
    },
    Virtual {
        reason: TranslationVirtualReason,
    },
}

fn preprocess_scope(
    scope: PreparedScope,
    semantics: Arc<ResolvedTranslationSemantics>,
    cancellation: &CooperativeCancellation,
) -> Result<PreprocessedScopeResult, ScopePreprocessingFailure> {
    ensure_planner_cpu_running(cancellation).map_err(|()| ScopePreprocessingFailure::Cancelled)?;
    let mut groups = Vec::with_capacity(scope.groups.len());
    for group in scope.groups {
        ensure_planner_cpu_running(cancellation)
            .map_err(|()| ScopePreprocessingFailure::Cancelled)?;
        let group_context = group_context_fingerprint_with_cancellation(&group, cancellation)?;
        let mut prepared_assets = Vec::with_capacity(group.assets.len());
        for asset in group.assets {
            ensure_planner_cpu_running(cancellation)
                .map_err(|()| ScopePreprocessingFailure::Cancelled)?;
            let prepared = match semantics.prepare_content_with_cancellation(
                group.kind,
                asset.identity.source_content(),
                || ensure_planner_cpu_running(cancellation),
            ) {
                Ok(Ok(prepared)) => prepared,
                Ok(Err(source)) => {
                    let reason = planning_failure_reason(source);
                    return Err(ScopePreprocessingFailure::UnitPreparation(
                        TranslationPlanningFailure::new(asset.identity, reason),
                    ));
                }
                Err(()) => return Err(ScopePreprocessingFailure::Cancelled),
            };
            prepared_assets.push((asset, prepared));
        }

        // 术语属于完整 Group：任一 Unit 命中的术语都必须进入该 Group 的全部自动状态，
        // 并且按术语文件中的自然顺序只保留一次。
        let mut group_terminology = BTreeMap::<usize, TerminologyDependency>::new();
        for (_, prepared) in &prepared_assets {
            ensure_planner_cpu_running(cancellation)
                .map_err(|()| ScopePreprocessingFailure::Cancelled)?;
            debug_assert_eq!(prepared.term_indices().len(), prepared.terms().len());
            for (&term_index, dependency) in prepared.term_indices().iter().zip(prepared.terms()) {
                ensure_planner_cpu_running(cancellation)
                    .map_err(|()| ScopePreprocessingFailure::Cancelled)?;
                if let Some(existing) = group_terminology.get(&term_index) {
                    debug_assert_eq!(existing, dependency);
                } else {
                    group_terminology.insert(term_index, dependency.clone());
                }
            }
        }
        let triggered_terms = group_terminology.keys().copied().collect::<Vec<_>>();
        let terminology_dependencies = group_terminology.into_values().collect::<Vec<_>>();

        let mut units = Vec::with_capacity(prepared_assets.len());
        for (asset, prepared) in prepared_assets {
            let protected_text = prepared.model_text().to_owned();
            let placeholders = prepared.placeholders().to_vec();
            let language_analysis = prepared.language_analysis().clone();
            let state_context = translation_state_context_with_cancellation(
                semantics.global_fingerprint(),
                group_context,
                &asset.identity,
                &protected_text,
                &placeholders,
                &terminology_dependencies,
                || ensure_planner_cpu_running(cancellation),
            )
            .map_err(|()| ScopePreprocessingFailure::Cancelled)?
            .map_err(ScopePreprocessingFailure::Invalid)?;
            let not_applicable = prepared.status() != PreparedTranslationStatus::Active;
            let current = asset.translation.as_ref().is_some_and(|translation| {
                asset.manual
                    || (!not_applicable
                        && asset.translation_state == Some(state_context.finish(translation)))
            });
            let invalidated = asset.translation.is_some() && !current;
            let responsibility = match prepared.status() {
                PreparedTranslationStatus::Active => {
                    PreparedUnitResponsibility::AwaitingDeduplication
                }
                PreparedTranslationStatus::NonSourceLanguage => {
                    PreparedUnitResponsibility::Virtual {
                        reason: TranslationVirtualReason::NonSourceLanguage,
                    }
                }
                PreparedTranslationStatus::FullyProtected => PreparedUnitResponsibility::Virtual {
                    reason: TranslationVirtualReason::FullyProtected,
                },
            };
            units.push(PreprocessedUnit {
                identity: asset.identity,
                protected_text,
                placeholders,
                language_analysis,
                translation: asset.translation,
                translation_state: asset.translation_state,
                invalidated,
                state_context,
                current,
                not_applicable,
                responsibility,
            });
        }
        groups.push(PreprocessedGroup {
            kind: group.kind,
            triggered_terms,
            units,
        });
    }
    Ok(PreprocessedScopeResult {
        scope: PreprocessedScope { groups },
    })
}

fn group_context_fingerprint_with_cancellation(
    group: &PreparedGroup,
    cancellation: &CooperativeCancellation,
) -> Result<GroupContextFingerprint, ScopePreprocessingFailure> {
    shared_group_context_fingerprint_with_cancellation(
        group.kind,
        &group.semantic_order_key,
        group
            .assets
            .iter()
            .map(|asset| (&asset.semantic_order_key, &asset.identity)),
        || ensure_planner_cpu_running(cancellation),
    )
    .map_err(|()| ScopePreprocessingFailure::Cancelled)?
    .map_err(|source| {
        ScopePreprocessingFailure::Invalid(match source {
            GroupContextFingerprintError::SemanticOrder(source) => {
                ScopePreprocessingError::SemanticOrder(source)
            }
            GroupContextFingerprintError::Location(source) => {
                ScopePreprocessingError::StateLocation(source)
            }
            GroupContextFingerprintError::Role(source) => {
                ScopePreprocessingError::StateRole(source)
            }
        })
    })
}
fn planning_failure_reason(
    source: ResolvedTranslationSemanticError,
) -> TranslationPlanningFailureReason {
    match source {
        ResolvedTranslationSemanticError::ProtectPlaceholder(source) => {
            TranslationPlanningFailureReason::PlaceholderProtection {
                failure: placeholder_protection_planning_failure(source),
            }
        }
        ResolvedTranslationSemanticError::ProjectLanguageText(source) => {
            TranslationPlanningFailureReason::PlaceholderProjection {
                failure: placeholder_projection_planning_failure(source),
            }
        }
        #[cfg(test)]
        ResolvedTranslationSemanticError::AcceptCandidate(_) => {
            unreachable!("译前准备不会执行候选译文验收")
        }
    }
}

pub(crate) fn placeholder_protection_planning_failure(
    source: PlaceholderProtectionError,
) -> TranslationPlaceholderProtectionFailure {
    match source {
        PlaceholderProtectionError::StartWorker { operation, source } => {
            TranslationPlaceholderProtectionFailure::WorkerStart {
                operation,
                io_kind: source.kind(),
                raw_os_code: source.raw_os_error(),
            }
        }
        PlaceholderProtectionError::Match { rule, source } => {
            TranslationPlaceholderProtectionFailure::Pcre2 {
                rule,
                kind: source.kind(),
                code: source.code(),
                offset: source.offset(),
            }
        }
        PlaceholderProtectionError::EmptyMatch { matched } => {
            TranslationPlaceholderProtectionFailure::EmptyMatch { matched }
        }
        PlaceholderProtectionError::MissingTextCapture {
            rule_number,
            whole_match_start_byte,
            whole_match_end_byte,
        } => TranslationPlaceholderProtectionFailure::MissingTextCapture {
            rule_number,
            whole_match_start_byte,
            whole_match_end_byte,
        },
        PlaceholderProtectionError::InvalidMatchRange {
            rule_number,
            whole_match_start_byte,
            whole_match_end_byte,
            capture_start_byte,
            capture_end_byte,
            violation,
        } => TranslationPlaceholderProtectionFailure::InvalidMatchRange {
            rule_number,
            whole_match_start_byte,
            whole_match_end_byte,
            capture_start_byte,
            capture_end_byte,
            violation,
        },
        PlaceholderProtectionError::OverlappingMatches { first, second } => {
            TranslationPlaceholderProtectionFailure::OverlappingMatches { first, second }
        }
        PlaceholderProtectionError::CrossesLineBoundary {
            matched,
            source_line_index,
        } => TranslationPlaceholderProtectionFailure::CrossesLineBoundary {
            matched,
            source_line_index,
        },
        PlaceholderProtectionError::ReservedTokenNamespace {
            start_byte,
            end_byte,
        } => TranslationPlaceholderProtectionFailure::ReservedTokenNamespace {
            start_byte,
            end_byte,
        },
    }
}

pub(crate) fn placeholder_projection_planning_failure(
    source: LanguageTextProjectionError,
) -> TranslationPlaceholderProjectionFailure {
    match source {
        LanguageTextProjectionError::TokenIndexConstruction => {
            TranslationPlaceholderProjectionFailure::TokenIndexConstruction
        }
        LanguageTextProjectionError::EmptyToken => {
            TranslationPlaceholderProjectionFailure::EmptyToken
        }
        LanguageTextProjectionError::MissingToken { token } => {
            TranslationPlaceholderProjectionFailure::MissingToken { token }
        }
        LanguageTextProjectionError::RepeatedToken { token } => {
            TranslationPlaceholderProjectionFailure::RepeatedToken { token }
        }
        LanguageTextProjectionError::OverlappingToken { token } => {
            TranslationPlaceholderProjectionFailure::OverlappingToken { token }
        }
        LanguageTextProjectionError::ChangedTokenOrder {
            position,
            expected_token,
            actual_token,
        } => TranslationPlaceholderProjectionFailure::ChangedTokenOrder {
            position,
            expected_token,
            actual_token,
        },
        LanguageTextProjectionError::ChangedSegmentCount { expected, actual } => {
            TranslationPlaceholderProjectionFailure::ChangedSegmentCount { expected, actual }
        }
        LanguageTextProjectionError::ChangedSegmentKind { segment_index } => {
            TranslationPlaceholderProjectionFailure::ChangedSegmentKind { segment_index }
        }
        LanguageTextProjectionError::MissingOrderedToken { segment_index } => {
            TranslationPlaceholderProjectionFailure::MissingOrderedToken { segment_index }
        }
        LanguageTextProjectionError::UnusedOrderedToken => {
            TranslationPlaceholderProjectionFailure::UnusedOrderedToken
        }
    }
}

fn ensure_planner_cpu_running(cancellation: &CooperativeCancellation) -> Result<(), ()> {
    if cancellation.is_requested() {
        Err(())
    } else {
        Ok(())
    }
}

fn ensure_planner_language_running(
    cancellation: &CooperativeCancellation,
) -> Result<(), LanguageOperationCancelled> {
    ensure_planner_cpu_running(cancellation).map_err(|()| LanguageOperationCancelled)
}

fn clone_planner_text_with_cancellation(
    source: &str,
    cancellation: &CooperativeCancellation,
) -> Result<String, ()> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    ensure_planner_cpu_running(cancellation)?;
    let mut bytes = Vec::with_capacity(source.len());
    for chunk in source.as_bytes().chunks(CANCELLATION_CHECK_BYTES) {
        ensure_planner_cpu_running(cancellation)?;
        bytes.extend_from_slice(chunk);
    }
    ensure_planner_cpu_running(cancellation)?;
    // SAFETY: `source` 已经是 UTF-8；这里只按字节复制全部内容，没有改写或遗漏字节。
    Ok(unsafe { String::from_utf8_unchecked(bytes) })
}

fn append_planner_text_with_cancellation(
    output: &mut String,
    source: &str,
    cancellation: &CooperativeCancellation,
) -> Result<(), ScopeTaskPlanningFailure> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    let mut start = 0_usize;
    while start < source.len() {
        ensure_planner_cpu_running(cancellation)
            .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
        let mut end = start
            .saturating_add(CANCELLATION_CHECK_BYTES)
            .min(source.len());
        while end < source.len() && !source.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&source[start..end]);
        start = end;
    }
    ensure_planner_cpu_running(cancellation).map_err(|()| ScopeTaskPlanningFailure::Cancelled)
}

#[cfg(test)]
pub(crate) fn global_translation_semantics(
    engine: RpgMakerEngine,
    source_language: &str,
    target_language: &str,
    language_semantics: Sha256Fingerprint,
    system_markdown: &str,
    response_mode: TranslationResponseMode,
    client_semantics: Sha256Fingerprint,
) -> Sha256Fingerprint {
    let language_pair = LanguagePair::new(
        crate::language::LanguageId::parse(source_language).expect("测试源语言标识必须有效"),
        crate::language::LanguageId::parse(target_language).expect("测试目标语言标识必须有效"),
    );
    match global_translation_semantics_with_cancellation(
        engine,
        &language_pair,
        language_semantics,
        system_markdown,
        response_mode,
        client_semantics,
        || Ok::<_, Infallible>(()),
    ) {
        Ok(fingerprint) => fingerprint,
        Err(unreachable) => match unreachable {},
    }
}

pub(crate) fn global_translation_semantics_with_cancellation<E>(
    engine: RpgMakerEngine,
    language_pair: &LanguagePair,
    language_semantics: Sha256Fingerprint,
    system_markdown: &str,
    response_mode: TranslationResponseMode,
    client_semantics: Sha256Fingerprint,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Sha256Fingerprint, E> {
    let chunk_size = NonZeroUsize::new(64 * 1024).expect("全局翻译语义指纹取消检查块大小必须非零");
    ensure_running()?;
    let mut hasher = Sha256FramedHasher::new(b"att.rpg_maker.translation-global");
    hasher
        .frame(1, language_pair.source().as_str().as_bytes())
        .frame(2, language_pair.target().as_str().as_bytes())
        .frame(3, language_semantics.as_bytes());
    hasher
        .try_frame_chunks(
            4,
            system_markdown.as_bytes(),
            chunk_size,
            &mut ensure_running,
        )?
        .frame(
            5,
            if response_mode.thinking() {
                b"thinking=true"
            } else {
                b"thinking=false"
            },
        )
        .frame(
            6,
            if response_mode.source_echo() {
                b"source-echo=true"
            } else {
                b"source-echo=false"
            },
        )
        .frame(7, client_semantics.as_bytes())
        .frame(8, engine.storage_name().as_bytes());
    ensure_running()?;
    Ok(hasher.finish())
}

#[cfg(test)]
pub(crate) fn translation_state_context(
    global_semantics: Sha256Fingerprint,
    identity: &TranslationUnitIdentity,
    protected_text: &str,
    placeholders: &[super::pipeline::AppliedPlaceholder],
    terminology: &[TerminologyDependency],
) -> Result<TranslationStateContext, ScopePreprocessingError> {
    let group = PreparedGroup {
        kind: identity.kind(),
        semantic_order_key: RpgMakerSemanticOrderKey::new(Vec::new(), 0),
        assets: vec![PreparedAsset {
            identity: identity.clone(),
            semantic_order_key: RpgMakerSemanticOrderKey::new(Vec::new(), 0),
            translation: None,
            translation_state: None,
            manual: false,
        }],
    };
    let group_context = match group_context_fingerprint_with_cancellation(
        &group,
        &CooperativeCancellation::default(),
    ) {
        Ok(fingerprint) => fingerprint,
        Err(ScopePreprocessingFailure::Invalid(source)) => return Err(source),
        Err(ScopePreprocessingFailure::Cancelled) => {
            unreachable!("未请求取消的测试 Group 指纹不能取消")
        }
        Err(ScopePreprocessingFailure::UnitPreparation(_)) => {
            unreachable!("Group 指纹不执行 Unit Placeholder 或语言投影准备")
        }
    };
    match translation_state_context_with_cancellation(
        global_semantics,
        group_context,
        identity,
        protected_text,
        placeholders,
        terminology,
        || Ok::<_, Infallible>(()),
    ) {
        Ok(result) => result,
        Err(unreachable) => match unreachable {},
    }
}

/// 保持自动译文状态的既有字节语义，并在任意长度字段之间轮询取消。
///
/// 外层错误只表示取消；内层错误表示受信 Unit 身份不能编码。
pub(crate) fn translation_state_context_with_cancellation<E>(
    global_semantics: Sha256Fingerprint,
    group_context: GroupContextFingerprint,
    identity: &TranslationUnitIdentity,
    protected_text: &str,
    placeholders: &[super::pipeline::AppliedPlaceholder],
    terminology: &[TerminologyDependency],
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Result<TranslationStateContext, ScopePreprocessingError>, E> {
    ensure_running()?;
    let group_location = match RpgMakerLocationCodec::encode(identity.group_location()) {
        Ok(group_location) => group_location,
        Err(source) => {
            return Ok(Err(ScopePreprocessingError::StateLocation(source)));
        }
    };
    let role = match RpgMakerProjectionCodec::encode_role(identity.role()) {
        Ok(role) => role,
        Err(source) => return Ok(Err(ScopePreprocessingError::StateRole(source))),
    };
    let chunk_size = NonZeroUsize::new(64 * 1024).expect("自动译文状态哈希取消检查块大小必须非零");
    let mut hasher = Sha256FramedHasher::new(b"att.rpg_maker.translation-unit-context");
    hasher
        .frame(1, global_semantics.as_bytes())
        .frame(2, group_context.as_fingerprint().as_bytes())
        .frame(3, identity.owner().storage_name().as_bytes())
        .frame(4, group_kind_name(identity.kind()));
    hasher.try_frame_chunks(
        5,
        group_location.as_bytes(),
        chunk_size,
        &mut ensure_running,
    )?;
    hasher.try_frame_chunks(6, role.as_bytes(), chunk_size, &mut ensure_running)?;
    hasher.try_frame_chunks(
        7,
        identity.source_context_json().as_bytes(),
        chunk_size,
        &mut ensure_running,
    )?;
    match identity.source_content() {
        TextUnitContent::Value(value) => {
            hasher.frame(8, b"value");
            hasher.try_frame_chunks(9, value.as_bytes(), chunk_size, &mut ensure_running)?;
        }
        TextUnitContent::Lines(lines) => {
            let count = u64::try_from(lines.len())
                .expect("源行数必须能表示为 u64")
                .to_le_bytes();
            hasher.frame(8, b"lines").frame(9, &count);
            for line in lines {
                ensure_running()?;
                hasher.try_frame_chunks(10, line.as_bytes(), chunk_size, &mut ensure_running)?;
            }
        }
    }
    hasher.try_frame_chunks(
        11,
        protected_text.as_bytes(),
        chunk_size,
        &mut ensure_running,
    )?;
    for placeholder in placeholders {
        ensure_running()?;
        let origin = match placeholder.origin() {
            super::pipeline::PlaceholderRuleOrigin::BuiltIn => b"builtin".as_slice(),
            super::pipeline::PlaceholderRuleOrigin::Custom => b"custom".as_slice(),
        };
        let segment = match placeholder.segment() {
            super::pipeline::PlaceholderSegment::Whole => b"whole".as_slice(),
            super::pipeline::PlaceholderSegment::Begin => b"begin".as_slice(),
            super::pipeline::PlaceholderSegment::End => b"end".as_slice(),
        };
        hasher.try_frame_chunks(
            20,
            placeholder.token().as_bytes(),
            chunk_size,
            &mut ensure_running,
        )?;
        hasher.try_frame_chunks(
            21,
            placeholder.original().as_bytes(),
            chunk_size,
            &mut ensure_running,
        )?;
        hasher.frame(22, origin);
        hasher.try_frame_chunks(
            23,
            placeholder.label().as_bytes(),
            chunk_size,
            &mut ensure_running,
        )?;
        hasher.try_frame_chunks(
            24,
            placeholder.scope().as_bytes(),
            chunk_size,
            &mut ensure_running,
        )?;
        hasher.frame(25, segment);
    }
    for dependency in terminology {
        ensure_running()?;
        hasher.try_frame_chunks(
            30,
            dependency.term().as_bytes(),
            chunk_size,
            &mut ensure_running,
        )?;
        hasher.try_frame_chunks(
            31,
            dependency.translation().as_bytes(),
            chunk_size,
            &mut ensure_running,
        )?;
    }
    ensure_running()?;
    Ok(Ok(TranslationStateContext::new(hasher.finish())))
}

type UnitPosition = (usize, usize, usize);

fn collect_deduplication_inputs(
    scopes: &[PreprocessedScope],
    complete_plan: &crate::translation::task_planning::CompleteTaskPlan,
) -> (
    Vec<TranslationDeduplicationCandidate>,
    Vec<UnitPosition>,
    Vec<TranslationInvalidation>,
) {
    let mut candidates = Vec::new();
    let mut positions = Vec::new();
    let mut invalidations = Vec::new();
    let mut global_unit_index = 0_usize;
    for (scope_index, scope) in scopes.iter().enumerate() {
        for (group_index, group) in scope.groups.iter().enumerate() {
            for (unit_index, unit) in group.units.iter().enumerate() {
                if matches!(
                    unit.responsibility,
                    PreparedUnitResponsibility::AwaitingDeduplication
                ) {
                    candidates.push(TranslationDeduplicationCandidate::new(
                        unit.identity.clone(),
                        unit.protected_text.clone(),
                        unit.placeholders.clone(),
                        unit.translation.clone(),
                        unit.translation_state,
                        unit.state_context,
                        unit.invalidated,
                    ));
                    positions.push((scope_index, group_index, unit_index));
                } else if unit.invalidated {
                    invalidations.push(TranslationInvalidation::new(
                        unit.identity.clone(),
                        unit.translation
                            .clone()
                            .expect("只有已有译文的单元才可能语义失效"),
                        unit.translation_state
                            .expect("已有译文必须同时具有 translation_state"),
                    ));
                }
                global_unit_index += 1;
            }
        }
    }
    assert_eq!(
        global_unit_index,
        complete_plan.total_units(),
        "共享 Planner 的完整 Unit 数必须与预处理语料一致"
    );
    (candidates, positions, invalidations)
}

fn apply_deduplication_outcomes(
    scopes: &mut [PreprocessedScope],
    positions: Vec<UnitPosition>,
    outcomes: Vec<TranslationDeduplicationOutcome>,
) {
    assert_eq!(
        positions.len(),
        outcomes.len(),
        "全局去重必须为每个候选单元返回一个责任"
    );
    for ((scope_index, group_index, unit_index), outcome) in positions.into_iter().zip(outcomes) {
        let unit = &mut scopes[scope_index].groups[group_index].units[unit_index];
        unit.responsibility = match outcome {
            TranslationDeduplicationOutcome::Active {
                propagation_targets,
            } => PreparedUnitResponsibility::Active {
                propagation_targets,
            },
            TranslationDeduplicationOutcome::Virtual { reason } => {
                PreparedUnitResponsibility::Virtual { reason }
            }
        };
    }
}

fn assign_complete_task_plan(
    scopes: Vec<PreprocessedScope>,
    complete_plan: crate::translation::task_planning::CompleteTaskPlan,
    cancellation: &CooperativeCancellation,
) -> Result<
    (
        Vec<PreprocessedScope>,
        crate::translation::task_planning::AssignedTaskPlan,
    ),
    ScopeTaskPlanningFailure,
> {
    ensure_planner_cpu_running(cancellation).map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
    let mut responsibilities = Vec::with_capacity(complete_plan.total_units());
    for scope in &scopes {
        for group in &scope.groups {
            for unit in &group.units {
                ensure_planner_cpu_running(cancellation)
                    .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
                responsibilities.push(match &unit.responsibility {
                    PreparedUnitResponsibility::Active { .. } => {
                        UnitTaskResponsibility::ModelRepresentative
                    }
                    PreparedUnitResponsibility::AwaitingDeduplication => {
                        unreachable!("分配 Task ID 前必须完成全局去重")
                    }
                    PreparedUnitResponsibility::Virtual { .. } => UnitTaskResponsibility::Context,
                });
            }
        }
    }
    let assigned = assign_task_ids(complete_plan, &responsibilities, cancellation)
        .map_err(ScopeTaskPlanningFailure::TaskPlanning)?;
    Ok((scopes, assigned))
}

fn materialize_task_block(
    scopes: &[PreprocessedScope],
    block: AssignedTaskBlock,
    terminology: &TerminologyPromptIndex,
    semantics: &ResolvedTranslationSemantics,
    system_markdown: &str,
    cancellation: &CooperativeCancellation,
) -> Result<UnindexedTask, ScopeTaskPlanningFailure> {
    ensure_planner_cpu_running(cancellation).map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
    let layout = block.layout();
    let scope = scopes
        .get(layout.scope_index())
        .expect("共享 Planner 返回的 Scope 索引必须来自当前完整语料");
    let group_range = layout.group_range();
    let groups = scope
        .groups
        .get(group_range)
        .expect("共享 Planner 返回的 Group 范围必须来自当前完整语料");

    let mut task_ids = block.unit_task_ids().iter().copied();
    let mut selected_terms = BTreeSet::new();
    let mut rendered_groups = Vec::with_capacity(groups.len());
    for group in groups {
        ensure_planner_cpu_running(cancellation)
            .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
        let mut rendered_units = Vec::with_capacity(group.units.len());
        let mut expected = Vec::new();
        for term in &group.triggered_terms {
            selected_terms.insert(*term);
        }
        for unit in &group.units {
            ensure_planner_cpu_running(cancellation)
                .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
            let task_id = task_ids
                .next()
                .expect("AssignedTaskBlock 的 Unit ID 槽必须覆盖完整块");
            let display_text = if task_id.is_some() {
                unit.protected_text.clone()
            } else {
                context_model_text_with_cancellation(unit, semantics, cancellation)?
            };
            let line_shape = expected_line_shape_with_cancellation(&unit.identity, cancellation)?;
            rendered_units.push(RenderedUnit {
                id: task_id,
                role: unit.identity.role_label(),
                return_type: task_id.map(|_| translation_return_type(line_shape)),
                text: display_text,
            });

            let Some(task_id) = task_id else {
                continue;
            };
            let PreparedUnitResponsibility::Active {
                propagation_targets,
            } = &unit.responsibility
            else {
                unreachable!("只有模型代表可以获得 Task ID")
            };
            expected.push(ExpectedBase {
                id: task_id,
                line_shape,
                identity: unit.identity.clone(),
                propagation_targets: propagation_targets.clone(),
                protected_text: unit.protected_text.clone(),
                placeholders: unit.placeholders.clone(),
                language_analysis: unit.language_analysis.clone(),
                state_context: unit.state_context,
            });
        }
        rendered_groups.push(RenderedGroup {
            kind: group.kind,
            units: rendered_units,
            expected,
        });
    }
    assert!(
        task_ids.next().is_none(),
        "AssignedTaskBlock 不得包含超出完整块的 Unit ID 槽"
    );

    let user_message = render_user_message_with_cancellation(
        &rendered_groups,
        terminology,
        &selected_terms,
        cancellation,
    )?;
    let mut expected_outputs = Vec::new();
    for group in rendered_groups {
        for expected in group.expected {
            ensure_planner_cpu_running(cancellation)
                .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
            let mut propagation_targets = Vec::with_capacity(expected.propagation_targets.len());
            let mut propagation_state_contexts =
                Vec::with_capacity(expected.propagation_targets.len());
            for target in expected.propagation_targets {
                ensure_planner_cpu_running(cancellation)
                    .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
                propagation_targets.push(target.identity().clone());
                propagation_state_contexts.push(target.state_context());
            }
            expected_outputs.push(
                ExpectedTranslationOutput::try_new_with_cancellation(
                    expected.id,
                    expected.identity,
                    propagation_targets,
                    ExpectedTranslationValidation::new(
                        expected.line_shape,
                        expected.protected_text,
                        expected.placeholders,
                        expected.language_analysis,
                    ),
                    expected.state_context,
                    propagation_state_contexts,
                    || ensure_planner_cpu_running(cancellation),
                )
                .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?
                .map_err(ScopeTaskPlanningFailure::InvalidContract)?,
            );
        }
    }
    let system_markdown = clone_planner_text_with_cancellation(system_markdown, cancellation)
        .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
    Ok(UnindexedTask {
        messages: vec![
            ChatMessage::new(ChatMessageRole::System, system_markdown),
            ChatMessage::new(ChatMessageRole::User, user_message),
        ],
        expected_outputs,
    })
}

fn context_model_text_with_cancellation(
    unit: &PreprocessedUnit,
    semantics: &ResolvedTranslationSemantics,
    cancellation: &CooperativeCancellation,
) -> Result<String, ScopeTaskPlanningFailure> {
    let target = if unit.current {
        unit.translation.as_ref()
    } else {
        match &unit.responsibility {
            PreparedUnitResponsibility::Virtual {
                reason: TranslationVirtualReason::ExistingTranslation,
            } => unit.translation.as_ref(),
            PreparedUnitResponsibility::Virtual {
                reason: TranslationVirtualReason::Reused { translation, .. },
            } => Some(translation),
            PreparedUnitResponsibility::Virtual { .. }
            | PreparedUnitResponsibility::AwaitingDeduplication
            | PreparedUnitResponsibility::Active { .. } => None,
        }
    };
    let Some(target) = target else {
        return Ok(unit.protected_text.clone());
    };
    let projected = semantics
        .prepare_content_with_cancellation(unit.identity.kind(), target, || {
            ensure_planner_cpu_running(cancellation)
        })
        .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
    match projected {
        Ok(projected) if projected.placeholders() == unit.placeholders => {
            Ok(projected.model_text().to_owned())
        }
        Ok(_) | Err(_) => Ok(unit.protected_text.clone()),
    }
}

struct RenderedGroup {
    kind: TextGroupKind,
    units: Vec<RenderedUnit>,
    expected: Vec<ExpectedBase>,
}

struct RenderedUnit {
    id: Option<TaskId>,
    role: String,
    return_type: Option<TranslationReturnType>,
    text: String,
}

const fn translation_return_type(line_shape: ExpectedLineShape) -> TranslationReturnType {
    match line_shape {
        ExpectedLineShape::Aligned(_) => TranslationReturnType::Strict,
        ExpectedLineShape::Reflow => TranslationReturnType::Free,
    }
}

struct ExpectedBase {
    id: TaskId,
    line_shape: ExpectedLineShape,
    identity: TranslationUnitIdentity,
    propagation_targets: Vec<TranslationPropagationTarget>,
    protected_text: String,
    placeholders: Vec<super::pipeline::AppliedPlaceholder>,
    language_analysis: LanguageAnalysis,
    state_context: TranslationStateContext,
}

pub(crate) fn expected_line_shape(identity: &TranslationUnitIdentity) -> ExpectedLineShape {
    match expected_line_shape_with_cancellation(identity, &CooperativeCancellation::default()) {
        Ok(shape) => shape,
        Err(ScopeTaskPlanningFailure::Cancelled) => {
            unreachable!("未请求取消的行形状判断不能取消")
        }
        Err(ScopeTaskPlanningFailure::TaskPlanning(_)) => {
            unreachable!("行形状判断不执行 TaskBlock 规划")
        }
        Err(ScopeTaskPlanningFailure::InvalidContract(_)) => {
            unreachable!("行形状判断不验证输出契约")
        }
    }
}

fn expected_line_shape_with_cancellation(
    identity: &TranslationUnitIdentity,
    cancellation: &CooperativeCancellation,
) -> Result<ExpectedLineShape, ScopeTaskPlanningFailure> {
    ensure_planner_cpu_running(cancellation).map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
    Ok(match identity.role() {
        TextUnitRole::DialogueBody => ExpectedLineShape::Reflow,
        TextUnitRole::Choices | TextUnitRole::ScrollingText => ExpectedLineShape::Aligned(
            NonZeroUsize::new(source_line_count(identity))
                .expect("选项与滚动文本必须至少包含一个语义槽"),
        ),
        TextUnitRole::DialogueSpeaker => ExpectedLineShape::Aligned(NonZeroUsize::MIN),
        TextUnitRole::Scalar(key)
            if scalar_allows_reflow_with_cancellation(identity, key.as_str(), cancellation)? =>
        {
            ExpectedLineShape::Reflow
        }
        TextUnitRole::Scalar(_) => ExpectedLineShape::Aligned(NonZeroUsize::MIN),
    })
}

fn source_line_count(identity: &TranslationUnitIdentity) -> usize {
    identity
        .source_content()
        .as_lines()
        .expect("复合行角色必须保存完整行序列")
        .len()
}

fn scalar_allows_reflow_with_cancellation(
    identity: &TranslationUnitIdentity,
    field_name: &str,
    cancellation: &CooperativeCancellation,
) -> Result<bool, ScopeTaskPlanningFailure> {
    let value = identity
        .source_content()
        .as_value()
        .expect("Scalar 角色必须保存单个 Value");
    for chunk in value.as_bytes().chunks(64 * 1024) {
        ensure_planner_cpu_running(cancellation)
            .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
        if chunk.contains(&b'\n') {
            return Ok(true);
        }
    }
    Ok(matches!(
        (identity.group_location().source(), field_name),
        (RpgMakerSource::Data(StandardDataFile::Actors), "profile")
            | (
                RpgMakerSource::Data(StandardDataFile::Skills),
                "description"
            )
            | (RpgMakerSource::Data(StandardDataFile::Items), "description")
            | (
                RpgMakerSource::Data(StandardDataFile::Weapons),
                "description"
            )
            | (
                RpgMakerSource::Data(StandardDataFile::Armors),
                "description"
            )
    ))
}

/// 一次规划运行共享的术语提示词索引。
///
/// `lines` 严格沿用术语文件自然顺序；Task 只根据已经由 Aho-Corasick 得到的命中下标
/// 借用当前需要的条目，不再次扫描正文。
struct TerminologyPromptIndex {
    lines: Vec<TerminologyPromptLine>,
}

struct TerminologyPromptLine {
    source: String,
    translation: String,
}

impl TerminologyPromptIndex {
    #[cfg(test)]
    fn new(terminology: &CompiledTerminology) -> Self {
        match Self::new_with_cancellation(terminology, &CooperativeCancellation::default()) {
            Ok(index) => index,
            Err(ScopeTaskPlanningFailure::Cancelled) => {
                unreachable!("未请求取消的术语索引构建不能取消")
            }
            Err(ScopeTaskPlanningFailure::TaskPlanning(_)) => {
                unreachable!("术语索引构建不执行 TaskBlock 规划")
            }
            Err(ScopeTaskPlanningFailure::InvalidContract(_)) => {
                unreachable!("术语索引构建不验证输出契约")
            }
        }
    }

    fn new_with_cancellation(
        terminology: &CompiledTerminology,
        cancellation: &CooperativeCancellation,
    ) -> Result<Self, ScopeTaskPlanningFailure> {
        let mut lines = Vec::with_capacity(terminology.entries().len());
        for entry in terminology.entries() {
            ensure_planner_cpu_running(cancellation)
                .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
            lines.push(TerminologyPromptLine {
                source: clone_planner_text_with_cancellation(entry.term(), cancellation)
                    .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?,
                translation: clone_planner_text_with_cancellation(
                    entry.translation(),
                    cancellation,
                )
                .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?,
            });
        }
        ensure_planner_cpu_running(cancellation)
            .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
        Ok(Self { lines })
    }
}

#[cfg(test)]
fn render_user_message(
    groups: &[RenderedGroup],
    terminology: &TerminologyPromptIndex,
    selected_terms: &BTreeSet<usize>,
) -> String {
    match render_user_message_with_cancellation(
        groups,
        terminology,
        selected_terms,
        &CooperativeCancellation::default(),
    ) {
        Ok(markdown) => markdown,
        Err(ScopeTaskPlanningFailure::Cancelled) => {
            unreachable!("未请求取消的用户消息渲染不能取消")
        }
        Err(ScopeTaskPlanningFailure::TaskPlanning(_)) => {
            unreachable!("用户消息渲染不执行 TaskBlock 规划")
        }
        Err(ScopeTaskPlanningFailure::InvalidContract(_)) => {
            unreachable!("用户消息渲染不验证输出契约")
        }
    }
}

fn render_user_message_with_cancellation(
    groups: &[RenderedGroup],
    terminology: &TerminologyPromptIndex,
    selected_terms: &BTreeSet<usize>,
    cancellation: &CooperativeCancellation,
) -> Result<String, ScopeTaskPlanningFailure> {
    ensure_planner_cpu_running(cancellation).map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
    let mut selected_terminology = Vec::with_capacity(selected_terms.len());
    for &index in selected_terms {
        ensure_planner_cpu_running(cancellation)
            .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
        let entry = &terminology.lines[index];
        selected_terminology.push(TranslationUserTerminology::new(
            &entry.source,
            &entry.translation,
        ));
    }
    let mut wire_groups = Vec::with_capacity(groups.len());
    for group in groups {
        ensure_planner_cpu_running(cancellation)
            .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
        let mut units = Vec::with_capacity(group.units.len());
        for unit in &group.units {
            ensure_planner_cpu_running(cancellation)
                .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
            units.push(match (unit.id, unit.return_type) {
                (Some(id), Some(return_type)) => TranslationUserUnit::translated(
                    id,
                    Some(unit.role.as_str()),
                    return_type,
                    unit.text.as_str(),
                ),
                (None, None) => {
                    TranslationUserUnit::context(Some(unit.role.as_str()), unit.text.as_str())
                }
                _ => unreachable!("RPG Maker user message 的 ID 与返回类型必须同时出现"),
            });
        }
        wire_groups.push(TranslationUserGroup::new(group.kind.storage_name(), units));
    }
    render_translation_user_message(
        &TranslationUserMessage::new(selected_terminology, wire_groups),
        cancellation,
    )
    .map_err(|_| ScopeTaskPlanningFailure::Cancelled)
}

struct UnindexedTask {
    messages: Vec<ChatMessage>,
    expected_outputs: Vec<ExpectedTranslationOutput>,
}

impl UnindexedTask {
    fn with_index(
        self,
        index: RpgMakerTranslationTaskIndex,
        language_pair: LanguagePair,
    ) -> RpgMakerExecutableTask {
        RpgMakerExecutableTask::new(index, language_pair, self.messages, self.expected_outputs)
    }
}

const fn group_kind_name(kind: TextGroupKind) -> &'static [u8] {
    kind.storage_name().as_bytes()
}

enum GlobalPreparationFailure {
    Cancelled,
    UnitPreparation(TranslationPlanningFailure),
    StartWorker {
        operation: &'static str,
        source: io::Error,
    },
    InvalidScopePreprocessing {
        scope: RpgMakerSemanticScopeKey,
        source: ScopePreprocessingError,
    },
}

enum ScopeTaskPlanningFailure {
    Cancelled,
    TaskPlanning(TaskPlanningError),
    InvalidContract(ExpectedTranslationOutputContractError),
}

impl From<ExpectedTranslationOutputContractError> for ScopeTaskPlanningFailure {
    fn from(source: ExpectedTranslationOutputContractError) -> Self {
        Self::InvalidContract(source)
    }
}

#[derive(Debug)]
enum ScopePreprocessingFailure {
    Cancelled,
    UnitPreparation(TranslationPlanningFailure),
    Invalid(ScopePreprocessingError),
}

#[derive(Debug)]
pub(crate) enum RpgMakerTranslationTaskPlanningError<R, C> {
    ResolvedLanguagePairMismatch {
        project_source: String,
        project_target: String,
        resolved_source: String,
        resolved_target: String,
    },
    ReadResources(R),
    PrepareResourcesCompute(CpuTaskExecutionError<C>),
    CompilePlaceholdersCompute(CpuTaskExecutionError<C>),
    InvalidPlaceholderRules {
        origin: super::pipeline::TranslationPlaceholderRuleSource,
        source: PlaceholderRuleCompilationError,
    },
    PrepareCorpusCompute(CpuTaskExecutionError<C>),
    InvalidCorpus(CorpusPlanningError),
    PreprocessScopesCompute(CpuTaskExecutionError<C>),
    InvalidScopePreprocessing {
        scope: RpgMakerSemanticScopeKey,
        source: ScopePreprocessingError,
    },
    UnitPreparation(TranslationPlanningFailure),
    DeduplicateCompute(CpuTaskExecutionError<C>),
    StartDeduplicationWorker {
        operation: &'static str,
        source: io::Error,
    },
    PlanScopesCompute(CpuTaskExecutionError<C>),
    FinalizePlanCompute(CpuTaskExecutionError<C>),
    TaskPlanning(TaskPlanningError),
    InvalidOutputContract(ExpectedTranslationOutputContractError),
}

impl<R: fmt::Display, C: fmt::Display> fmt::Display for RpgMakerTranslationTaskPlanningError<R, C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ResolvedLanguagePairMismatch {
                project_source,
                project_target,
                resolved_source,
                resolved_target,
            } => write!(
                formatter,
                "项目语言对 {project_source} -> {project_target} 与已解析资源 {resolved_source} -> {resolved_target} 不一致"
            ),
            Self::ReadResources(source) => write!(formatter, "无法读取翻译规划资料：{source}"),
            Self::PrepareResourcesCompute(source) => {
                write!(formatter, "无法调度 RPG Maker 翻译规划上下文准备：{source}")
            }
            Self::CompilePlaceholdersCompute(source) => {
                write!(formatter, "无法调度占位符规则编译：{source}")
            }
            Self::InvalidPlaceholderRules { source, .. } => {
                write!(formatter, "占位符规则无效：{source}")
            }
            Self::PrepareCorpusCompute(source) => {
                write!(formatter, "无法调度 RPG Maker 语料排序：{source}")
            }
            Self::InvalidCorpus(source) => {
                write!(formatter, "RPG Maker 语料无法建立语义范围：{source}")
            }
            Self::PreprocessScopesCompute(source) => {
                write!(formatter, "无法调度语义范围的并行译前处理：{source}")
            }
            Self::InvalidScopePreprocessing { scope, source } => {
                write!(formatter, "语义范围 {scope} 无法完成译前处理：{source}")
            }
            Self::UnitPreparation(source) => source.fmt(formatter),
            Self::DeduplicateCompute(source) => {
                write!(formatter, "无法调度 RPG Maker 语料全局去重：{source}")
            }
            Self::StartDeduplicationWorker { operation, source } => {
                write!(
                    formatter,
                    "无法启动 RPG Maker 全局去重 worker {operation}：{source}"
                )
            }
            Self::PlanScopesCompute(source) => {
                write!(formatter, "无法调度语义范围的并行任务规划：{source}")
            }
            Self::FinalizePlanCompute(source) => {
                write!(formatter, "无法调度翻译任务最终编号：{source}")
            }
            Self::TaskPlanning(source) => write!(formatter, "无法建立稳定 TaskBlock：{source}"),
            Self::InvalidOutputContract(source) => {
                write!(formatter, "Planner 建立的模型输出契约无效：{source}")
            }
        }
    }
}

impl<R: Error + 'static, C: Error + 'static> Error for RpgMakerTranslationTaskPlanningError<R, C> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadResources(source) => Some(source),
            Self::PrepareResourcesCompute(source) => Some(source),
            Self::CompilePlaceholdersCompute(source) => Some(source),
            Self::InvalidPlaceholderRules { source, .. } => Some(source),
            Self::PrepareCorpusCompute(source) => Some(source),
            Self::InvalidCorpus(source) => Some(source),
            Self::PreprocessScopesCompute(source) => Some(source),
            Self::InvalidScopePreprocessing { source, .. } => Some(source),
            Self::UnitPreparation(source) => Some(source),
            Self::DeduplicateCompute(source) => Some(source),
            Self::StartDeduplicationWorker { source, .. } => Some(source),
            Self::PlanScopesCompute(source) => Some(source),
            Self::FinalizePlanCompute(source) => Some(source),
            Self::TaskPlanning(source) => Some(source),
            Self::InvalidOutputContract(source) => Some(source),
            Self::ResolvedLanguagePairMismatch { .. } => None,
        }
    }
}

impl<R, C> RpgMakerTranslationTaskPlanningError<R, C> {
    fn is_cancelled(&self) -> bool
    where
        R: TranslationPlanningResourceErrorCancellation,
    {
        match self {
            Self::ReadResources(source) => source.is_cancelled_error(),
            Self::PrepareResourcesCompute(source)
            | Self::CompilePlaceholdersCompute(source)
            | Self::PrepareCorpusCompute(source)
            | Self::PreprocessScopesCompute(source)
            | Self::DeduplicateCompute(source)
            | Self::PlanScopesCompute(source)
            | Self::FinalizePlanCompute(source) => {
                matches!(source, CpuTaskExecutionError::Cancelled)
            }
            Self::TaskPlanning(source) => source.is_cancelled(),
            Self::ResolvedLanguagePairMismatch { .. }
            | Self::InvalidPlaceholderRules { .. }
            | Self::InvalidCorpus(_)
            | Self::InvalidScopePreprocessing { .. }
            | Self::UnitPreparation(_)
            | Self::StartDeduplicationWorker { .. }
            | Self::InvalidOutputContract(_) => false,
        }
    }
}

type ProductionPlanningResourceError =
    TranslationPlanningResourceReadingError<SystemFileSystemError, CpuExecutorUnavailable>;

impl RpgMakerTranslationTaskPlanningError<ProductionPlanningResourceError, CpuExecutorUnavailable> {
    pub(crate) fn diagnostic_report(&self) -> DiagnosticReport {
        match self {
            Self::ResolvedLanguagePairMismatch {
                project_source,
                project_target,
                resolved_source,
                resolved_target,
            } => rpg_maker_planning_report(
                RpgMakerTranslationPlanningProblem::LanguagePairMismatch {
                    project_source: SafeIdentifier::new(project_source).ok(),
                    project_target: SafeIdentifier::new(project_target).ok(),
                    resolved_source: SafeIdentifier::new(resolved_source).ok(),
                    resolved_target: SafeIdentifier::new(resolved_target).ok(),
                },
            ),
            Self::ReadResources(source) => source.diagnostic_report(),
            Self::PrepareResourcesCompute(source) => {
                planner_cpu_report(source, RuntimeOperation::PrepareRpgMakerPlanningResources)
            }
            Self::CompilePlaceholdersCompute(source) => {
                planner_cpu_report(source, RuntimeOperation::CompileRpgMakerCustomPlaceholders)
            }
            Self::InvalidPlaceholderRules { origin, source } => DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::translation(TranslationIssue::PlaceholderCompilation {
                    origin: planning_placeholder_origin(origin),
                    problem: source.diagnostic_problem(),
                }),
            ),
            Self::PrepareCorpusCompute(source) => {
                planner_cpu_report(source, RuntimeOperation::PrepareRpgMakerTranslationCorpus)
            }
            Self::InvalidCorpus(source) => rpg_maker_planning_report(corpus_problem(source)),
            Self::PreprocessScopesCompute(source) => planner_cpu_report(
                source,
                RuntimeOperation::PreprocessRpgMakerTranslationScopes,
            ),
            Self::InvalidScopePreprocessing { scope, source } => {
                rpg_maker_planning_report(scope_preprocessing_problem(scope, source))
            }
            Self::UnitPreparation(source) => source.diagnostic_report(),
            Self::DeduplicateCompute(source) => planner_cpu_report(
                source,
                RuntimeOperation::DeduplicateRpgMakerTranslationCorpus,
            ),
            Self::StartDeduplicationWorker { source, .. } => DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::runtime(RuntimeIssue::Io {
                    component: RuntimeComponent::CpuExecutor,
                    operation: RuntimeOperation::DeduplicateRpgMakerTranslationCorpus,
                    failure: IoFailure::from_error(source),
                }),
            ),
            Self::PlanScopesCompute(source) => {
                planner_cpu_report(source, RuntimeOperation::PlanRpgMakerTranslationScopes)
            }
            Self::FinalizePlanCompute(source) => {
                planner_cpu_report(source, RuntimeOperation::FinalizeRpgMakerTranslationPlan)
            }
            Self::TaskPlanning(source) => DiagnosticReport::new(
                StateEffect::Unchanged,
                Diagnostic::translation(TranslationIssue::TaskPlanning {
                    problem: task_planning_problem(source),
                }),
            ),
            Self::InvalidOutputContract(source) => {
                rpg_maker_planning_report(output_contract_problem(source))
            }
        }
    }

    pub(crate) fn into_reported_failure(self) -> ReportedFailure {
        let report = self.diagnostic_report();
        ReportedFailure::new(report, self)
    }
}

fn rpg_maker_planning_report(problem: RpgMakerTranslationPlanningProblem) -> DiagnosticReport {
    DiagnosticReport::new(
        StateEffect::Unchanged,
        Diagnostic::rpg_maker(RpgMakerIssue::translation_planning(problem)),
    )
}

fn planner_cpu_report(
    source: &CpuTaskExecutionError<CpuExecutorUnavailable>,
    operation: RuntimeOperation,
) -> DiagnosticReport {
    DiagnosticReport::new(StateEffect::Unchanged, source.diagnostic_for(operation))
}

fn planning_placeholder_origin(
    source: &super::pipeline::TranslationPlaceholderRuleSource,
) -> TranslationPlanningResourceOrigin {
    match source {
        super::pipeline::TranslationPlaceholderRuleSource::ExternalFile(path) => {
            TranslationPlanningResourceOrigin::external(path)
        }
        super::pipeline::TranslationPlaceholderRuleSource::ProjectSnapshot => {
            TranslationPlanningResourceOrigin::ProjectSnapshot
        }
    }
}

fn diagnostic_scope(scope: &RpgMakerSemanticScopeKey) -> RpgMakerDiagnosticScope {
    match scope {
        RpgMakerSemanticScopeKey::StandardDatabase(file) => {
            RpgMakerDiagnosticScope::StandardDatabase {
                file: SafeIdentifier::from_validated(file.file_name()),
            }
        }
        RpgMakerSemanticScopeKey::DataFile(file) => RpgMakerDiagnosticScope::DataFile {
            path: crate::diagnostic::SafePath::new(file.to_string()),
        },
        RpgMakerSemanticScopeKey::System => RpgMakerDiagnosticScope::System,
        RpgMakerSemanticScopeKey::Map(map_id) => RpgMakerDiagnosticScope::Map {
            map_id: u64::from(map_id.get()),
        },
        RpgMakerSemanticScopeKey::CommonEvent(event_id) => RpgMakerDiagnosticScope::CommonEvent {
            event_id: *event_id,
        },
        RpgMakerSemanticScopeKey::Troop(troop_id) => RpgMakerDiagnosticScope::Troop {
            troop_id: *troop_id,
        },
        RpgMakerSemanticScopeKey::Plugin { plugin_index, .. } => RpgMakerDiagnosticScope::Plugin {
            plugin_index: *plugin_index,
        },
    }
}

fn corpus_problem(source: &CorpusPlanningError) -> RpgMakerTranslationPlanningProblem {
    match source {
        CorpusPlanningError::EmptySemanticScope { scope } => {
            RpgMakerTranslationPlanningProblem::EmptySemanticScope {
                scope: diagnostic_scope(scope),
            }
        }
        CorpusPlanningError::EmptyGroup { scope, kind } => {
            RpgMakerTranslationPlanningProblem::EmptyGroup {
                scope: diagnostic_scope(scope),
                group_kind: kind.diagnostic_group_kind(),
            }
        }
    }
}

fn scope_preprocessing_problem(
    scope: &RpgMakerSemanticScopeKey,
    source: &ScopePreprocessingError,
) -> RpgMakerTranslationPlanningProblem {
    match source {
        ScopePreprocessingError::StateLocation(source) => {
            RpgMakerTranslationPlanningProblem::ScopeLocationCodec {
                scope: diagnostic_scope(scope),
                failure: source.diagnostic_failure(),
            }
        }
        ScopePreprocessingError::StateRole(source) => {
            RpgMakerTranslationPlanningProblem::ScopeProjectionCodec {
                scope: diagnostic_scope(scope),
                failure: source.diagnostic_failure(),
            }
        }
        ScopePreprocessingError::SemanticOrder(_) => {
            RpgMakerTranslationPlanningProblem::ScopeSemanticOrderLengthOverflow {
                scope: diagnostic_scope(scope),
            }
        }
    }
}

fn task_planning_problem(source: &TaskPlanningError) -> TranslationTaskPlanningProblem {
    match source {
        TaskPlanningError::Cancelled => TranslationTaskPlanningProblem::Cancelled,
        TaskPlanningError::EmptyScope => TranslationTaskPlanningProblem::EmptyScope,
        TaskPlanningError::EmptyGroup => TranslationTaskPlanningProblem::EmptyGroup,
        TaskPlanningError::UnitCountOverflow => TranslationTaskPlanningProblem::UnitCountOverflow,
        TaskPlanningError::CharacterCountOverflow => {
            TranslationTaskPlanningProblem::CharacterCountOverflow
        }
        TaskPlanningError::ResponsibilityCountMismatch { expected, actual } => {
            TranslationTaskPlanningProblem::ResponsibilityCountMismatch {
                expected: *expected,
                actual: *actual,
            }
        }
        TaskPlanningError::TaskIdOverflow => TranslationTaskPlanningProblem::TaskIdOverflow,
    }
}

fn output_contract_problem(
    source: &ExpectedTranslationOutputContractError,
) -> RpgMakerTranslationPlanningProblem {
    let violation = match source {
        ExpectedTranslationOutputContractError::PropagationContextCountMismatch {
            target_count,
            context_count,
            ..
        } => RpgMakerOutputContractViolation::PropagationContextCountMismatch {
            target_count: *target_count,
            context_count: *context_count,
        },
        ExpectedTranslationOutputContractError::PlaceholderIndexInvalid { source, .. } => {
            RpgMakerOutputContractViolation::PlaceholderIndexInvalid {
                failure: language_projection_problem(source),
            }
        }
        ExpectedTranslationOutputContractError::ProtectedPlaceholderMultisetMismatch {
            kind,
            ..
        } => RpgMakerOutputContractViolation::ProtectedPlaceholderMultisetMismatch {
            violation: match kind {
                super::pipeline::PlaceholderMultisetErrorKind::Mismatch => {
                    RpgMakerPlaceholderMultisetViolation::Mismatch
                }
                super::pipeline::PlaceholderMultisetErrorKind::Unexpected => {
                    RpgMakerPlaceholderMultisetViolation::Unexpected
                }
                super::pipeline::PlaceholderMultisetErrorKind::OrderMismatch => {
                    RpgMakerPlaceholderMultisetViolation::OrderMismatch
                }
            },
        },
        ExpectedTranslationOutputContractError::ProtectedPlaceholderCrossesLineBoundary {
            placeholder_index,
            ..
        } => RpgMakerOutputContractViolation::ProtectedPlaceholderCrossesLineBoundary {
            placeholder_index: *placeholder_index,
        },
        ExpectedTranslationOutputContractError::ProtectedLineCountMismatch {
            expected,
            actual,
            ..
        } => RpgMakerOutputContractViolation::ProtectedLineCountMismatch {
            expected: *expected,
            actual: *actual,
        },
        ExpectedTranslationOutputContractError::ScalarAlignedCountInvalid { actual, .. } => {
            RpgMakerOutputContractViolation::ScalarAlignedCountInvalid { actual: *actual }
        }
        ExpectedTranslationOutputContractError::LinesAlignedCountMismatch {
            expected,
            actual,
            ..
        } => RpgMakerOutputContractViolation::LinesAlignedCountMismatch {
            expected: *expected,
            actual: *actual,
        },
    };
    RpgMakerTranslationPlanningProblem::OutputContract {
        task_id: source.diagnostic_task_id(),
        unit: source.diagnostic_unit_locator(),
        violation,
    }
}

fn language_projection_problem(
    source: &LanguageTextProjectionError,
) -> RpgMakerPlaceholderProjectionProblem {
    match source {
        LanguageTextProjectionError::TokenIndexConstruction => {
            RpgMakerPlaceholderProjectionProblem::TokenIndexConstruction
        }
        LanguageTextProjectionError::EmptyToken => RpgMakerPlaceholderProjectionProblem::EmptyToken,
        LanguageTextProjectionError::MissingToken { token } => {
            RpgMakerPlaceholderProjectionProblem::missing_token(token)
        }
        LanguageTextProjectionError::RepeatedToken { token } => {
            RpgMakerPlaceholderProjectionProblem::repeated_token(token)
        }
        LanguageTextProjectionError::OverlappingToken { token } => {
            RpgMakerPlaceholderProjectionProblem::overlapping_token(token)
        }
        LanguageTextProjectionError::ChangedTokenOrder {
            position,
            expected_token,
            actual_token,
        } => RpgMakerPlaceholderProjectionProblem::changed_token_order(
            *position,
            expected_token,
            actual_token,
        ),
        LanguageTextProjectionError::ChangedSegmentCount { expected, actual } => {
            RpgMakerPlaceholderProjectionProblem::ChangedSegmentCount {
                expected: *expected,
                actual: *actual,
            }
        }
        LanguageTextProjectionError::ChangedSegmentKind { segment_index } => {
            RpgMakerPlaceholderProjectionProblem::ChangedSegmentKind {
                segment_index: *segment_index,
            }
        }
        LanguageTextProjectionError::MissingOrderedToken { segment_index } => {
            RpgMakerPlaceholderProjectionProblem::MissingOrderedToken {
                segment_index: *segment_index,
            }
        }
        LanguageTextProjectionError::UnusedOrderedToken => {
            RpgMakerPlaceholderProjectionProblem::UnusedOrderedToken
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CorpusPlanningError {
    EmptySemanticScope {
        scope: RpgMakerSemanticScopeKey,
    },
    EmptyGroup {
        scope: RpgMakerSemanticScopeKey,
        kind: TextGroupKind,
    },
}

impl fmt::Display for CorpusPlanningError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySemanticScope { scope } => write!(formatter, "语义范围 {scope} 不得为空"),
            Self::EmptyGroup { scope, kind } => write!(
                formatter,
                "语义范围 {scope} 中的 {} Group 不得为空",
                kind.storage_name()
            ),
        }
    }
}

impl Error for CorpusPlanningError {}

#[derive(Debug)]
pub(crate) enum ScopePreprocessingError {
    StateLocation(crate::rpg_maker::location_codec::RpgMakerLocationCodecError),
    StateRole(crate::rpg_maker::location_codec::RpgMakerProjectionCodecError),
    SemanticOrder(crate::rpg_maker::semantic_order::RpgMakerSemanticOrderKeyEncodeError),
}

impl fmt::Display for ScopePreprocessingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StateLocation(source) => {
                write!(formatter, "无法编码译文状态位置：{source}")
            }
            Self::StateRole(source) => write!(formatter, "无法编码译文状态角色：{source}"),
            Self::SemanticOrder(source) => {
                write!(formatter, "无法编码 Group 语义顺序：{source}")
            }
        }
    }
}

impl Error for ScopePreprocessingError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::StateLocation(source) => Some(source),
            Self::StateRole(source) => Some(source),
            Self::SemanticOrder(source) => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::num::NonZeroUsize;
    use std::path::PathBuf;
    use std::sync::Arc;

    use crate::rpg_maker::model::{ScalarFieldKey, TextUnitContent, TextUnitRole};
    use crate::rpg_maker::text::{RpgMakerLocation, RpgMakerLocationStep};
    use crate::storage::file_system::{FileReader, ReadFile, ReadFileError};

    use super::*;
    use crate::language::{
        JapaneseLanguageModule, JapaneseResidualPolicy, LanguageId, LanguageModule, LanguagePair,
    };
    use crate::rpg_maker::asset::RpgMakerAssetOwner;
    use crate::rpg_maker::translate::pipeline::{AppliedPlaceholder, RpgMakerTranslationAsset};
    use crate::rpg_maker::translate::profile::{
        ResolvedRpgMakerTranslationResources, RpgMakerSystemPrompt,
        RpgMakerTranslationPlanningConfiguration, RpgMakerTranslationProfile,
    };
    use crate::translation::placeholder::{PlaceholderRuleOrigin, PlaceholderWorkerOperation};
    use crate::translation::planning_resource::{
        TranslationPlanningResourceReadingService, TranslationPlanningResources,
    };
    use crate::translation::profile::TranslationRequestConfiguration;
    use crate::translation_protocol::TranslationResponseMode;

    fn task_id(value: usize) -> TaskId {
        TaskId::new(value)
    }

    fn expect_unit_preparation_failure<R, C>(
        result: Result<RpgMakerTranslationPlan, RpgMakerTranslationTaskPlanningError<R, C>>,
    ) -> TranslationPlanningFailure {
        match result {
            Err(RpgMakerTranslationTaskPlanningError::UnitPreparation(failure)) => failure,
            Ok(_) => panic!("Unit Placeholder 或语言投影失败时不得返回可执行计划"),
            Err(_) => panic!("测试应得到类型化 Unit 规划准备失败"),
        }
    }

    #[derive(Clone, Copy)]
    struct ImmediateCpu;

    impl CpuTaskExecutor for ImmediateCpu {
        type Error = FakeError;

        async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            Ok(task())
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct FakeError;

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fake failure")
        }
    }

    impl Error for FakeError {}

    impl TranslationPlanningResourceErrorCancellation for FakeError {
        fn is_cancelled_error(&self) -> bool {
            false
        }
    }

    impl TranslationPlanningFileErrorCancellation for FakeError {
        fn is_cancelled_error(&self) -> bool {
            false
        }
    }

    #[test]
    fn planning_resource_read_classification_uses_the_typed_file_error() {
        let path = PathBuf::from("C:/input/placeholders.toml");
        let cancelled_read = |path: PathBuf| ReadFileError::Io {
            path: path.clone(),
            source: SystemFileSystemError::Cancelled {
                operation: "read_file",
                path,
            },
        };
        let cancelled_errors: Vec<ProductionPlanningResourceError> = vec![
            TranslationPlanningResourceReadingError::ReadTerminology {
                path: path.clone(),
                source: cancelled_read(path.clone()),
            },
            TranslationPlanningResourceReadingError::ReadPlaceholderRules {
                path: path.clone(),
                source: cancelled_read(path.clone()),
            },
        ];
        for source in cancelled_errors {
            let error: RpgMakerTranslationTaskPlanningError<_, CpuExecutorUnavailable> =
                RpgMakerTranslationTaskPlanningError::ReadResources(source);
            assert!(error.is_cancelled());
        }

        let source: ProductionPlanningResourceError =
            TranslationPlanningResourceReadingError::ReadPlaceholderRules {
                path: path.clone(),
                source: ReadFileError::Io {
                    path: path.clone(),
                    source: SystemFileSystemError::Io {
                        operation: "read_file",
                        path,
                        source: io::Error::other("disk failure"),
                    },
                },
            };
        let error: RpgMakerTranslationTaskPlanningError<_, CpuExecutorUnavailable> =
            RpgMakerTranslationTaskPlanningError::ReadResources(source);
        assert!(!error.is_cancelled());
    }

    #[test]
    fn cancellable_global_semantics_preserves_fingerprint_and_observes_chunks() {
        let system_markdown = "系统提示。".repeat(32 * 1024);
        let language_semantics = Sha256Fingerprint::from_bytes([1; 32]);
        let client_semantics = Sha256Fingerprint::from_bytes([2; 32]);
        let language_pair = LanguagePair::new(
            LanguageId::parse("ja").expect("测试源语言应合法"),
            LanguageId::parse("zh-Hans").expect("测试目标语言应合法"),
        );
        let expected = global_translation_semantics(
            RpgMakerEngine::Mz,
            "ja",
            "zh-Hans",
            language_semantics,
            &system_markdown,
            TranslationResponseMode::new(false, false),
            client_semantics,
        );
        for response_mode in [
            TranslationResponseMode::new(true, false),
            TranslationResponseMode::new(false, true),
            TranslationResponseMode::new(true, true),
        ] {
            assert_ne!(
                global_translation_semantics(
                    RpgMakerEngine::Mz,
                    "ja",
                    "zh-Hans",
                    language_semantics,
                    &system_markdown,
                    response_mode,
                    client_semantics,
                ),
                expected,
                "响应开关必须独立进入自动译文语义指纹"
            );
        }
        let mut polls = 0_usize;
        let actual = global_translation_semantics_with_cancellation(
            RpgMakerEngine::Mz,
            &language_pair,
            language_semantics,
            &system_markdown,
            TranslationResponseMode::new(false, false),
            client_semantics,
            || {
                polls += 1;
                Ok::<_, Infallible>(())
            },
        )
        .expect("未取消的分块指纹应完成");

        assert_eq!(actual, expected);
        assert!(polls > 2, "长 Prompt 必须在多个块之间轮询取消");

        let mut cancellation_polls = 0_usize;
        let cancelled = global_translation_semantics_with_cancellation(
            RpgMakerEngine::Mz,
            &language_pair,
            language_semantics,
            &system_markdown,
            TranslationResponseMode::new(false, false),
            client_semantics,
            || {
                cancellation_polls += 1;
                if cancellation_polls >= 3 {
                    Err("cancelled")
                } else {
                    Ok(())
                }
            },
        );
        assert_eq!(cancelled, Err("cancelled"));
    }

    #[test]
    fn cancellable_unit_state_preserves_fingerprint_and_observes_chunks() {
        let identity = TranslationUnitIdentity::new(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            RpgMakerLocation::value(
                RpgMakerSource::data(StandardDataFile::Actors),
                vec![RpgMakerLocationStep::index(1)],
            ),
            TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法")),
            TextUnitContent::Value("原".repeat(80_000)),
            format!(r#"{{"note":"{}"}}"#, "文".repeat(80_000)),
        );
        let protected_text = "保护".repeat(80_000);
        let terminology = vec![TerminologyDependency::new(
            "术".repeat(80_000),
            "语".repeat(80_000),
        )];
        let global = Sha256Fingerprint::from_bytes([0x6b; 32]);
        let group_context = GroupContextFingerprint::new(Sha256Fingerprint::from_bytes([0x4c; 32]));
        let expected = translation_state_context_with_cancellation(
            global,
            group_context,
            &identity,
            &protected_text,
            &[],
            &terminology,
            || Ok::<_, Infallible>(()),
        )
        .expect("测试不取消")
        .expect("普通状态应可建立");
        let mut polls = 0_usize;
        let actual = translation_state_context_with_cancellation(
            global,
            group_context,
            &identity,
            &protected_text,
            &[],
            &terminology,
            || {
                polls += 1;
                Ok::<_, ()>(())
            },
        )
        .expect("不取消")
        .expect("状态应可建立");
        assert_eq!(actual, expected);
        assert!(polls >= 12);

        let mut polls = 0_usize;
        let cancelled = translation_state_context_with_cancellation(
            global,
            group_context,
            &identity,
            &protected_text,
            &[],
            &terminology,
            || {
                polls += 1;
                if polls >= 5 { Err("cancelled") } else { Ok(()) }
            },
        );
        assert!(matches!(cancelled, Err("cancelled")));
    }

    #[test]
    fn group_context_fingerprint_tracks_complete_group_but_not_translation_state() {
        let build = |owner: RpgMakerAssetOwner,
                     group_order: u64,
                     second_order: u64,
                     second_source: &str,
                     second_context: &str,
                     with_translation: bool| {
            let identity = |index, source: &str, context: &str| {
                TranslationUnitIdentity::new(
                    owner,
                    TextGroupKind::DatabaseEntry,
                    RpgMakerLocation::value(
                        RpgMakerSource::data(StandardDataFile::Items),
                        vec![RpgMakerLocationStep::index(index)],
                    ),
                    TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法")),
                    TextUnitContent::Value(source.to_owned()),
                    context,
                )
            };
            PreparedGroup {
                kind: TextGroupKind::DatabaseEntry,
                semantic_order_key: RpgMakerSemanticOrderKey::new(vec![group_order], 0),
                assets: vec![
                    PreparedAsset {
                        identity: identity(1, "第一项", "{}"),
                        semantic_order_key: RpgMakerSemanticOrderKey::new(vec![1], 1),
                        translation: with_translation
                            .then(|| TextUnitContent::Value("译文一".to_owned())),
                        translation_state: with_translation
                            .then(|| Sha256Fingerprint::from_bytes([1; 32])),
                        manual: false,
                    },
                    PreparedAsset {
                        identity: identity(2, second_source, second_context),
                        semantic_order_key: RpgMakerSemanticOrderKey::new(vec![second_order], 1),
                        translation: with_translation
                            .then(|| TextUnitContent::Value("译文二".to_owned())),
                        translation_state: with_translation
                            .then(|| Sha256Fingerprint::from_bytes([2; 32])),
                        manual: false,
                    },
                ],
            }
        };
        let fingerprint = |group: &PreparedGroup| {
            group_context_fingerprint_with_cancellation(group, &CooperativeCancellation::default())
                .expect("完整 Group 语境应可建立")
        };

        let base = fingerprint(&build(
            RpgMakerAssetOwner::Builtin,
            10,
            2,
            "第二项",
            r#"{"speaker":"甲"}"#,
            false,
        ));
        assert_eq!(
            base,
            fingerprint(&build(
                RpgMakerAssetOwner::Builtin,
                10,
                2,
                "第二项",
                r#"{"speaker":"甲"}"#,
                true,
            )),
            "目标译文和旧状态不能进入 Group 语境指纹"
        );
        for changed in [
            build(
                RpgMakerAssetOwner::Rules,
                10,
                2,
                "第二项",
                r#"{"speaker":"甲"}"#,
                false,
            ),
            build(
                RpgMakerAssetOwner::Builtin,
                11,
                2,
                "第二项",
                r#"{"speaker":"甲"}"#,
                false,
            ),
            build(
                RpgMakerAssetOwner::Builtin,
                10,
                3,
                "第二项",
                r#"{"speaker":"甲"}"#,
                false,
            ),
            build(
                RpgMakerAssetOwner::Builtin,
                10,
                2,
                "改过的第二项",
                r#"{"speaker":"甲"}"#,
                false,
            ),
            build(
                RpgMakerAssetOwner::Builtin,
                10,
                2,
                "第二项",
                r#"{"speaker":"乙"}"#,
                false,
            ),
        ] {
            assert_ne!(
                base,
                fingerprint(&changed),
                "owner、Group/Unit 顺序、兄弟原文和 source context 都必须属于完整 Group 语境"
            );
        }
    }

    impl LlmClientSemanticIdentity for () {
        fn semantic_fingerprint(&self) -> Sha256Fingerprint {
            Sha256Fingerprint::from_bytes([0x33; 32])
        }
    }

    impl LlmClientConcurrency for () {
        fn max_concurrent_requests(&self) -> NonZeroUsize {
            NonZeroUsize::new(2).expect("测试并发数必须非零")
        }
    }

    struct EmptyResources;

    impl TranslationPlanningResourceReader for EmptyResources {
        type Error = FakeError;

        async fn read(
            &self,
            _terminology_path: Option<PathBuf>,
            _placeholder_rules_path: Option<PathBuf>,
            _current_terminology_json: String,
            _current_placeholder_rules_json: String,
        ) -> Result<TranslationPlanningResources, Self::Error> {
            Ok(TranslationPlanningResources::new(
                CompiledTerminology::empty(),
                Vec::new(),
                "[]".to_owned(),
                "[]".to_owned(),
            ))
        }
    }

    #[derive(Clone)]
    struct MemoryFileReader {
        files: Arc<BTreeMap<PathBuf, Vec<u8>>>,
    }

    impl FileReader for MemoryFileReader {
        type Error = FakeError;

        async fn read_file(&self, path: PathBuf) -> Result<ReadFile, ReadFileError<Self::Error>> {
            let bytes = self
                .files
                .get(&path)
                .cloned()
                .ok_or_else(|| ReadFileError::NotFound { path: path.clone() })?;
            Ok(ReadFile::new(path, bytes))
        }
    }

    fn translation_resources_for(
        source_language: &str,
        target_language: &str,
    ) -> Arc<ResolvedRpgMakerTranslationResources> {
        translation_resources_for_with_system(
            source_language,
            target_language,
            "# System\n完整且由外部提供。".to_owned(),
        )
    }

    fn translation_resources_for_with_system(
        source_language: &str,
        target_language: &str,
        system_markdown: String,
    ) -> Arc<ResolvedRpgMakerTranslationResources> {
        let module: Arc<dyn LanguageModule> = Arc::new(JapaneseLanguageModule::new(
            JapaneseResidualPolicy::new(
                NonZeroUsize::new(1).expect("测试残留阈值必须非零"),
                Vec::new(),
            )
            .expect("测试日文残留策略应该有效"),
        ));
        let pair = LanguagePair::new(
            LanguageId::parse(source_language).expect("测试源语言应合法"),
            LanguageId::parse(target_language).expect("测试目标语言应合法"),
        );
        let prompt = RpgMakerSystemPrompt::new(
            pair,
            system_markdown,
            TranslationResponseMode::new(false, false),
        )
        .expect("测试 Prompt 应合法");
        Arc::new(ResolvedRpgMakerTranslationResources::new(prompt, module))
    }

    fn translation_resources() -> Arc<ResolvedRpgMakerTranslationResources> {
        translation_resources_for("ja", "zh-Hans")
    }

    fn profile(target_user_message_characters: usize) -> Arc<RpgMakerTranslationProfile<()>> {
        let planning = RpgMakerTranslationPlanningConfiguration::new(
            NonZeroUsize::new(target_user_message_characters).expect("测试目标必须非零"),
        );
        Arc::new(RpgMakerTranslationProfile::new(
            "test",
            planning,
            TranslationRequestConfiguration::new(Vec::new(), std::time::Duration::ZERO),
            Arc::new(()),
        ))
    }

    fn user_message(task: &RpgMakerExecutableTask) -> &str {
        task.messages()
            .iter()
            .find(|message| message.role() == ChatMessageRole::User)
            .expect("任务必须包含 user message")
            .content()
    }

    fn user_message_json(task: &RpgMakerExecutableTask) -> serde_json::Value {
        parse_user_message_json(user_message(task))
    }

    fn parse_user_message_json(message: &str) -> serde_json::Value {
        let json = message
            .strip_prefix("```json\n")
            .and_then(|value| value.strip_suffix("\n```"))
            .expect("RPG Maker user message 必须是单一 JSON 围栏");
        serde_json::from_str(json).expect("RPG Maker user message 围栏内部必须是 JSON")
    }

    fn project() -> OpenedProject {
        project_with_languages("ja", "zh-Hans")
    }

    fn project_with_languages(source_language: &str, target_language: &str) -> OpenedProject {
        OpenedProject::new(
            "测试游戏".parse().expect("测试项目名应该有效"),
            PathBuf::from("C:/Projects/测试游戏"),
            PathBuf::from("C:/Projects/测试游戏/project.db"),
            source_language.to_owned(),
            target_language.to_owned(),
            crate::rpg_maker::project::test_layout_profile(),
        )
    }

    fn group(
        source: RpgMakerSource,
        object_index: usize,
        original: impl Into<String>,
        translation: Option<&str>,
        terms: Vec<TerminologyDependency>,
    ) -> RpgMakerTranslationGroup {
        let original = original.into();
        let group_location = RpgMakerLocation::value(
            source.clone(),
            vec![RpgMakerLocationStep::index(object_index)],
        );
        let source_content = TextUnitContent::Value(original.clone());
        let identity = TranslationUnitIdentity::new(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            group_location.clone(),
            TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法")),
            source_content,
            "{}",
        );
        let translation_state = translation.map(|translation| {
            translation_state_for(&identity, &original, translation, &terms, Vec::new())
        });
        RpgMakerTranslationGroup::new(
            TextGroupKind::DatabaseEntry,
            group_location,
            vec![RpgMakerTranslationAsset::new(
                identity,
                translation.map(|value| TextUnitContent::Value(value.to_owned())),
                translation_state,
            )],
        )
    }

    fn manual_current_group(
        source: RpgMakerSource,
        object_index: usize,
        original: &str,
        translation: &str,
    ) -> RpgMakerTranslationGroup {
        let natural_index = u64::try_from(object_index).expect("测试对象索引必须可转成 u64");
        let group_location =
            RpgMakerLocation::value(source, vec![RpgMakerLocationStep::index(object_index)]);
        let identity = TranslationUnitIdentity::new(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            group_location.clone(),
            TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法")),
            TextUnitContent::Value(original.to_owned()),
            "{}",
        );
        let group_order = RpgMakerSemanticOrderKey::new(vec![natural_index], 0);
        let unit_order = RpgMakerSemanticOrderKey::new(vec![natural_index], 1);
        RpgMakerTranslationGroup::with_semantic_order_key(
            TextGroupKind::DatabaseEntry,
            group_location,
            group_order,
            vec![RpgMakerTranslationAsset::with_manual_semantic_order_key(
                identity,
                unit_order,
                TextUnitContent::Value(translation.to_owned()),
                Sha256Fingerprint::from_bytes([0x71; 32]),
            )],
        )
    }

    fn translation_state_for(
        identity: &TranslationUnitIdentity,
        original: &str,
        translation: &str,
        terms: &[TerminologyDependency],
        placeholder_definitions: Vec<super::super::placeholder::PlaceholderRuleDefinition>,
    ) -> Sha256Fingerprint {
        let placeholders = Pcre2PlaceholderService::new().expect("内建占位符应可编译");
        let custom = placeholders
            .compile_custom(placeholder_definitions)
            .expect("测试自定义占位符应可编译");
        let (protected_text, bindings) = placeholders
            .protect(RpgMakerEngine::Mz, identity.kind(), original, &custom)
            .expect("测试原文应可保护")
            .into_parts();
        let source_language = translation_resources().source_language();
        let global = global_translation_semantics(
            RpgMakerEngine::Mz,
            "ja",
            "zh-Hans",
            source_language.semantic_fingerprint(),
            "# System\n完整且由外部提供。",
            TranslationResponseMode::new(false, false),
            ().semantic_fingerprint(),
        );
        translation_state_context(global, identity, &protected_text, &bindings, terms)
            .expect("测试译文状态应可建立")
            .finish(&TextUnitContent::Value(translation.to_owned()))
    }

    fn translation_state_context_for_group(
        global_semantics: Sha256Fingerprint,
        group: &RpgMakerTranslationGroup,
        identity: &TranslationUnitIdentity,
        protected_text: &str,
        placeholders: &[AppliedPlaceholder],
        terminology: &[TerminologyDependency],
    ) -> TranslationStateContext {
        let prepared_group = PreparedGroup {
            kind: group.kind(),
            semantic_order_key: group.semantic_order_key().clone(),
            assets: group
                .assets()
                .iter()
                .map(|asset| PreparedAsset {
                    identity: asset.identity().clone(),
                    semantic_order_key: asset.semantic_order_key().clone(),
                    translation: None,
                    translation_state: None,
                    manual: false,
                })
                .collect(),
        };
        let group_context = group_context_fingerprint_with_cancellation(
            &prepared_group,
            &CooperativeCancellation::default(),
        )
        .expect("测试完整 Group 语境指纹应可建立");
        translation_state_context_with_cancellation(
            global_semantics,
            group_context,
            identity,
            protected_text,
            placeholders,
            terminology,
            || Ok::<_, Infallible>(()),
        )
        .expect("不取消的测试状态计算不能取消")
        .expect("测试完整 Group 状态应可建立")
    }

    #[test]
    fn matching_manual_state_stays_current_for_active_and_non_source_units() {
        for original in ["こんにちは", "已经是中文"] {
            let semantics = Arc::new(ResolvedTranslationSemantics::for_test());
            let group_location = RpgMakerLocation::value(
                RpgMakerSource::data(StandardDataFile::Actors),
                vec![RpgMakerLocationStep::index(1)],
            );
            let identity = TranslationUnitIdentity::new(
                RpgMakerAssetOwner::Builtin,
                TextGroupKind::DatabaseEntry,
                group_location,
                TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法")),
                TextUnitContent::Value(original.to_owned()),
                "{}",
            );
            let group_order = RpgMakerSemanticOrderKey::new(vec![1], 0);
            let unit_order = RpgMakerSemanticOrderKey::new(vec![1], 0);
            let scope = PreparedScope {
                key: RpgMakerSemanticScopeKey::StandardDatabase(StandardDataFile::Actors),
                groups: vec![PreparedGroup {
                    kind: TextGroupKind::DatabaseEntry,
                    semantic_order_key: group_order,
                    assets: vec![PreparedAsset {
                        identity,
                        semantic_order_key: unit_order,
                        translation: Some(TextUnitContent::Value("人工译文".to_owned())),
                        translation_state: Some(Sha256Fingerprint::from_bytes([0x72; 32])),
                        manual: true,
                    }],
                }],
            };

            let result = preprocess_scope(scope, semantics, &CooperativeCancellation::default())
                .expect("人工 Current 应可预处理");
            let unit = &result.scope.groups[0].units[0];
            assert!(unit.current);
            assert!(!unit.invalidated);
        }
    }

    #[test]
    fn manual_current_ignores_sibling_source_and_translation() {
        let semantics = Arc::new(ResolvedTranslationSemantics::for_test());
        let group_location = RpgMakerLocation::value(
            RpgMakerSource::map(1),
            vec![
                RpgMakerLocationStep::key("events"),
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("pages"),
                RpgMakerLocationStep::index(0),
                RpgMakerLocationStep::key("list"),
                RpgMakerLocationStep::index(0),
            ],
        );
        let speaker = TranslationUnitIdentity::new(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            group_location.clone(),
            TextUnitRole::DialogueSpeaker,
            TextUnitContent::Value("先生".to_owned()),
            r#"{"event":1}"#,
        );
        let body = |source: &str| {
            TranslationUnitIdentity::new(
                RpgMakerAssetOwner::Rules,
                TextGroupKind::EventDialogue,
                group_location.clone(),
                TextUnitRole::DialogueBody,
                TextUnitContent::Value(source.to_owned()),
                r#"{"event":1}"#,
            )
        };
        let group_order = RpgMakerSemanticOrderKey::new(vec![1, 1, 0], 0);
        let speaker_order = RpgMakerSemanticOrderKey::new(vec![1, 1, 0], 1);
        let body_order = RpgMakerSemanticOrderKey::new(vec![1, 1, 0], 2);
        let speaker_state = Sha256Fingerprint::from_bytes([0x73; 32]);
        let scope = |body_source: &str, body_translation: Option<&str>| PreparedScope {
            key: RpgMakerSemanticScopeKey::Map(
                crate::rpg_maker::text::MapId::new(1).expect("测试 Map ID 应有效"),
            ),
            groups: vec![PreparedGroup {
                kind: TextGroupKind::EventDialogue,
                semantic_order_key: group_order.clone(),
                assets: vec![
                    PreparedAsset {
                        identity: speaker.clone(),
                        semantic_order_key: speaker_order.clone(),
                        translation: Some(TextUnitContent::Value("老师".to_owned())),
                        translation_state: Some(speaker_state),
                        manual: true,
                    },
                    PreparedAsset {
                        identity: body(body_source),
                        semantic_order_key: body_order.clone(),
                        translation: body_translation
                            .map(|value| TextUnitContent::Value(value.to_owned())),
                        translation_state: body_translation
                            .map(|_| Sha256Fingerprint::from_bytes([0x91; 32])),
                        manual: false,
                    },
                ],
            }],
        };

        let translation_only = preprocess_scope(
            scope("こんにちは", Some("你好")),
            Arc::clone(&semantics),
            &CooperativeCancellation::default(),
        )
        .expect("兄弟译文变化应可预处理");
        assert!(
            translation_only.scope.groups[0].units[0].current,
            "兄弟目标译文不能使人工译文失去 Current"
        );

        let source_changed = preprocess_scope(
            scope("こんばんは", None),
            semantics,
            &CooperativeCancellation::default(),
        )
        .expect("兄弟原文变化应可预处理");
        assert!(
            source_changed.scope.groups[0].units[0].current,
            "兄弟 Unit 的原文变化不能使人工译文失去 Current"
        );
    }

    #[test]
    fn automatic_current_tracks_sibling_source_and_order_but_ignores_sibling_translation() {
        let semantics = Arc::new(ResolvedTranslationSemantics::for_test());
        let group_location = RpgMakerLocation::value(
            RpgMakerSource::map(1),
            vec![
                RpgMakerLocationStep::key("events"),
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("pages"),
                RpgMakerLocationStep::index(0),
                RpgMakerLocationStep::key("list"),
                RpgMakerLocationStep::index(0),
            ],
        );
        let speaker = TranslationUnitIdentity::new(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            group_location.clone(),
            TextUnitRole::DialogueSpeaker,
            TextUnitContent::Value("先生".to_owned()),
            r#"{"event":1}"#,
        );
        let body = |source: &str| {
            TranslationUnitIdentity::new(
                RpgMakerAssetOwner::Rules,
                TextGroupKind::EventDialogue,
                group_location.clone(),
                TextUnitRole::DialogueBody,
                TextUnitContent::Value(source.to_owned()),
                r#"{"event":1}"#,
            )
        };
        let group_order = RpgMakerSemanticOrderKey::new(vec![1, 1, 0], 0);
        let speaker_order = RpgMakerSemanticOrderKey::new(vec![1, 1, 0], 1);
        let original_body_order = RpgMakerSemanticOrderKey::new(vec![1, 1, 0], 2);
        let original_body = body("こんにちは");
        let group_context = shared_group_context_fingerprint_with_cancellation(
            TextGroupKind::EventDialogue,
            &group_order,
            [
                (&speaker_order, &speaker),
                (&original_body_order, &original_body),
            ]
            .into_iter(),
            || Ok::<_, Infallible>(()),
        )
        .expect("测试 Group 指纹不能取消")
        .expect("测试 Group 指纹应可建立");
        let prepared_speaker = semantics
            .prepare_content(speaker.kind(), speaker.source_content())
            .expect("测试说话者应可准备");
        let speaker_translation = TextUnitContent::Value("老师".to_owned());
        let speaker_state = translation_state_context_with_cancellation(
            semantics.global_fingerprint(),
            group_context,
            &speaker,
            prepared_speaker.model_text(),
            prepared_speaker.placeholders(),
            prepared_speaker.terms(),
            || Ok::<_, Infallible>(()),
        )
        .expect("测试自动状态不能取消")
        .expect("测试自动状态应可建立")
        .finish(&speaker_translation);
        let scope = |body_source: &str,
                     body_order: RpgMakerSemanticOrderKey,
                     body_translation: Option<&str>| PreparedScope {
            key: RpgMakerSemanticScopeKey::Map(
                crate::rpg_maker::text::MapId::new(1).expect("测试 Map ID 应有效"),
            ),
            groups: vec![PreparedGroup {
                kind: TextGroupKind::EventDialogue,
                semantic_order_key: group_order.clone(),
                assets: vec![
                    PreparedAsset {
                        identity: speaker.clone(),
                        semantic_order_key: speaker_order.clone(),
                        translation: Some(speaker_translation.clone()),
                        translation_state: Some(speaker_state),
                        manual: false,
                    },
                    PreparedAsset {
                        identity: body(body_source),
                        semantic_order_key: body_order,
                        translation: body_translation
                            .map(|value| TextUnitContent::Value(value.to_owned())),
                        translation_state: body_translation
                            .map(|_| Sha256Fingerprint::from_bytes([0x91; 32])),
                        manual: false,
                    },
                ],
            }],
        };
        let speaker_is_current = |scope| {
            preprocess_scope(
                scope,
                Arc::clone(&semantics),
                &CooperativeCancellation::default(),
            )
            .expect("测试 Scope 应可预处理")
            .scope
            .groups[0]
                .units[0]
                .current
        };

        assert!(
            speaker_is_current(scope(
                "こんにちは",
                original_body_order.clone(),
                Some("你好")
            )),
            "兄弟目标译文不能使自动译文失去 Current"
        );
        assert!(
            !speaker_is_current(scope("こんばんは", original_body_order, None)),
            "兄弟原文变化必须使自动译文失去 Current"
        );
        assert!(
            !speaker_is_current(scope(
                "こんにちは",
                RpgMakerSemanticOrderKey::new(vec![1, 1, 0], 3),
                None
            )),
            "兄弟顺序变化必须使自动译文失去 Current"
        );
    }

    fn map_group(
        kind: TextGroupKind,
        event_index: Option<usize>,
        page_index: Option<usize>,
        command_index: Option<usize>,
        original: &str,
    ) -> RpgMakerTranslationGroup {
        let source = RpgMakerSource::map(1);
        let group_steps = match (event_index, page_index, command_index) {
            (None, None, None) => Vec::new(),
            (Some(event), Some(page), Some(command)) => vec![
                RpgMakerLocationStep::key("events"),
                RpgMakerLocationStep::index(event),
                RpgMakerLocationStep::key("pages"),
                RpgMakerLocationStep::index(page),
                RpgMakerLocationStep::key("list"),
                RpgMakerLocationStep::index(command),
            ],
            _ => panic!("Map 事件测试位置必须同时给出 event/page/command"),
        };
        let (role, source_content) = if group_steps.is_empty() {
            (
                TextUnitRole::Scalar(ScalarFieldKey::new("displayName").expect("字段键应合法")),
                TextUnitContent::Value(original.to_owned()),
            )
        } else {
            (
                TextUnitRole::DialogueBody,
                TextUnitContent::Lines(vec![original.to_owned()]),
            )
        };
        let group_location = RpgMakerLocation::value(source, group_steps);
        let identity = TranslationUnitIdentity::new(
            RpgMakerAssetOwner::Builtin,
            kind,
            group_location.clone(),
            role,
            source_content,
            "{}",
        );
        RpgMakerTranslationGroup::new(
            kind,
            group_location,
            vec![RpgMakerTranslationAsset::new(identity, None, None)],
        )
    }

    fn map_unit_group(
        kind: TextGroupKind,
        command_index: usize,
        units: Vec<(TextUnitRole, TextUnitContent, &str)>,
    ) -> RpgMakerTranslationGroup {
        let group_location = RpgMakerLocation::value(
            RpgMakerSource::map(1),
            vec![
                RpgMakerLocationStep::key("events"),
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("pages"),
                RpgMakerLocationStep::index(0),
                RpgMakerLocationStep::key("list"),
                RpgMakerLocationStep::index(command_index),
            ],
        );
        let assets = units
            .into_iter()
            .map(|(role, content, context)| {
                RpgMakerTranslationAsset::new(
                    TranslationUnitIdentity::new(
                        RpgMakerAssetOwner::Builtin,
                        kind,
                        group_location.clone(),
                        role,
                        content,
                        context,
                    ),
                    None,
                    None,
                )
            })
            .collect();
        RpgMakerTranslationGroup::new(kind, group_location, assets)
    }

    #[test]
    fn dialogue_group_preserves_reader_unit_order_and_state_includes_source_speaker() {
        let group_location = RpgMakerLocation::value(
            RpgMakerSource::map(1),
            vec![
                RpgMakerLocationStep::key("events"),
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("list"),
                RpgMakerLocationStep::index(0),
            ],
        );
        let body = TranslationUnitIdentity::new(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            group_location.clone(),
            TextUnitRole::DialogueBody,
            TextUnitContent::Lines(vec!["同一句".to_owned()]),
            r#"{"source_speaker":"甲"}"#,
        );
        let other_body = TranslationUnitIdentity::new(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            group_location.clone(),
            TextUnitRole::DialogueBody,
            TextUnitContent::Lines(vec!["同一句".to_owned()]),
            r#"{"source_speaker":"乙"}"#,
        );
        let speaker = TranslationUnitIdentity::new(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            group_location.clone(),
            TextUnitRole::DialogueSpeaker,
            TextUnitContent::Value("甲".to_owned()),
            "{}",
        );
        let prepared = prepare_corpus(vec![RpgMakerTranslationGroup::new(
            TextGroupKind::EventDialogue,
            group_location,
            vec![
                RpgMakerTranslationAsset::new(body.clone(), None, None),
                RpgMakerTranslationAsset::new(speaker, None, None),
            ],
        )])
        .expect("对话组应可准备");
        assert_eq!(
            prepared.scopes[0].groups[0].assets[0].identity.role(),
            &TextUnitRole::DialogueBody
        );

        let global = Sha256Fingerprint::from_bytes([0x11; 32]);
        let first =
            translation_state_context(global, &body, "同一句", &[], &[]).expect("状态应可建立");
        let second = translation_state_context(global, &other_body, "同一句", &[], &[])
            .expect("状态应可建立");
        assert_ne!(first, second, "源 Speaker 必须参与正文译文状态");
    }

    #[test]
    fn non_contiguous_equal_scope_keys_do_not_cross_the_reader_global_order() {
        let prepared = prepare_corpus(vec![
            group(
                RpgMakerSource::data(StandardDataFile::Items),
                1,
                "一",
                None,
                Vec::new(),
            ),
            group(
                RpgMakerSource::data(StandardDataFile::Actors),
                1,
                "二",
                None,
                Vec::new(),
            ),
            group(
                RpgMakerSource::data(StandardDataFile::Items),
                2,
                "三",
                None,
                Vec::new(),
            ),
        ])
        .expect("Reader 顺序应可准备");

        assert_eq!(prepared.scopes.len(), 3);
        assert!(matches!(
            &prepared.scopes[0].key,
            RpgMakerSemanticScopeKey::StandardDatabase(StandardDataFile::Items)
        ));
        assert!(matches!(
            &prepared.scopes[1].key,
            RpgMakerSemanticScopeKey::StandardDatabase(StandardDataFile::Actors)
        ));
        assert!(matches!(
            &prepared.scopes[2].key,
            RpgMakerSemanticScopeKey::StandardDatabase(StandardDataFile::Items)
        ));
    }

    #[tokio::test]
    async fn changed_terminology_invalidates_exact_translation_and_builds_dense_task() {
        let terminology_path = PathBuf::from("C:/input/terms.toml");
        let mut files = BTreeMap::new();
        files.insert(
            terminology_path.clone(),
            r#"
                [[term]]
                term = "魔法剣"
                translation = "魔法之剑"
            "#
            .as_bytes()
            .to_vec(),
        );
        let reader = TranslationPlanningResourceReadingService::new(
            MemoryFileReader {
                files: Arc::new(files),
            },
            ImmediateCpu,
        );
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            reader,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let old_dependency = TerminologyDependency::new("魔法剣", "魔法剑");
        let corpus = RpgMakerTranslationCorpus::new(vec![group(
            RpgMakerSource::data(StandardDataFile::Items),
            1,
            r"\C[2]魔法剣",
            Some("魔法剑"),
            vec![old_dependency.clone()],
        )]);

        let plan = planner
            .plan(
                &project(),
                &profile(10_000),
                corpus,
                RpgMakerTranslationInput::new(Some(terminology_path), None),
            )
            .await
            .expect("术语变更应该建立重译计划");
        let (_, preparation, tasks) = plan.into_parts();

        assert_eq!(preparation.invalidations().len(), 1);
        assert_eq!(
            preparation.invalidations()[0].expected_translation(),
            &TextUnitContent::Value("魔法剑".to_owned())
        );
        assert_eq!(preparation.invalidated(), 1);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].expected_outputs()[0].id(), task_id(0));
        assert_eq!(
            tasks[0].expected_outputs()[0].applied_placeholders().len(),
            1
        );
        assert_eq!(
            tasks[0].messages()[0].content(),
            "# System\n完整且由外部提供。"
        );
        let user_json = user_message_json(&tasks[0]);
        assert_eq!(user_json["terminology"][0]["source"], "魔法剣");
        assert_eq!(user_json["terminology"][0]["translation"], "魔法之剑");
        assert_eq!(user_json["groups"][0]["units"][0]["id"], "0");
        assert_eq!(user_json["groups"][0]["units"][0]["role"], "name");
        assert_eq!(user_json["groups"][0]["units"][0]["type"], "strict");
        let user = tasks[0].messages()[1].content();
        assert!(!user.contains("source_language"));
        assert!(!user.contains("target_language"));
        assert!(!user.contains("data/Items.json"));
        assert!(!user.contains("exact_location"));
    }

    #[tokio::test]
    async fn unknown_backslash_sequence_reaches_the_model_request_as_natural_text() {
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let original = r"再生 \SE[Bell] 後で続けます";

        let (_, _, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                RpgMakerTranslationCorpus::new(vec![group(
                    RpgMakerSource::data(StandardDataFile::Items),
                    1,
                    original,
                    None,
                    Vec::new(),
                )]),
                RpgMakerTranslationInput::new(None, None),
            )
            .await
            .expect("未知反斜杠序列不应阻断翻译规划")
            .into_parts();

        assert_eq!(tasks.len(), 1);
        let user = user_message_json(&tasks[0]);
        assert_eq!(
            user["groups"][0]["units"][0]["text"],
            serde_json::json!([original]),
            "未知反斜杠序列必须作为自然文本进入结构化 user message"
        );
        assert!(
            tasks[0].expected_outputs()[0]
                .applied_placeholders()
                .is_empty()
        );
    }

    #[test]
    fn scalar_line_shape_uses_multiline_content_before_canonical_field_policy() {
        let identity = |source: RpgMakerSource, field_name: &str, value: &str| {
            let group_location = RpgMakerLocation::value(source, Vec::new());
            TranslationUnitIdentity::new(
                RpgMakerAssetOwner::Builtin,
                TextGroupKind::DatabaseEntry,
                group_location,
                TextUnitRole::Scalar(ScalarFieldKey::new(field_name).expect("测试字段键应合法")),
                TextUnitContent::Value(value.to_owned()),
                "{}",
            )
        };

        assert_eq!(
            expected_line_shape(&identity(
                RpgMakerSource::data(StandardDataFile::Items),
                "name",
                "任意字段第一行\n任意字段第二行",
            )),
            ExpectedLineShape::Reflow
        );
        assert_eq!(
            expected_line_shape(&identity(
                RpgMakerSource::data(StandardDataFile::Items),
                "name",
                "任意字段单行",
            )),
            ExpectedLineShape::Aligned(NonZeroUsize::MIN)
        );
        for (source, field_name) in [
            (StandardDataFile::Actors, "profile"),
            (StandardDataFile::Skills, "description"),
            (StandardDataFile::Items, "description"),
            (StandardDataFile::Weapons, "description"),
            (StandardDataFile::Armors, "description"),
        ] {
            assert_eq!(
                expected_line_shape(&identity(
                    RpgMakerSource::data(source),
                    field_name,
                    "规范字段单行",
                )),
                ExpectedLineShape::Reflow
            );
        }
    }

    #[tokio::test]
    async fn multiline_rules_scalar_uses_reflow_instead_of_a_single_line_contract() {
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let corpus = RpgMakerTranslationCorpus::new(
            [
                (
                    "GamepadIsNotConnected",
                    "ゲームパッドが接続されていません\nボタンを押して再度試してください",
                ),
                (
                    "needButtonDetouch",
                    "コンフィグを終了するためには\nボタンから手を放してください。",
                ),
            ]
            .into_iter()
            .map(|(parameter_name, value)| {
                let source =
                    RpgMakerSource::plugin_parameter(7, "Mano_InputConfig", parameter_name);
                let group_location = RpgMakerLocation::value(source, Vec::new());
                let identity = TranslationUnitIdentity::new(
                    RpgMakerAssetOwner::Rules,
                    TextGroupKind::PluginParameter,
                    group_location.clone(),
                    TextUnitRole::Scalar(
                        ScalarFieldKey::new("<json>.text[0]").expect("字段键应合法"),
                    ),
                    TextUnitContent::Value(value.to_owned()),
                    "{}",
                );
                RpgMakerTranslationGroup::new(
                    TextGroupKind::PluginParameter,
                    group_location,
                    vec![RpgMakerTranslationAsset::new(identity, None, None)],
                )
            })
            .collect(),
        );

        let (_, _, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                corpus,
                RpgMakerTranslationInput::new(None, None),
            )
            .await
            .expect("多行 Rules Scalar 应形成可执行翻译任务")
            .into_parts();

        assert_eq!(tasks.len(), 1);
        assert_eq!(
            tasks[0]
                .expected_outputs()
                .iter()
                .map(ExpectedTranslationOutput::line_shape)
                .collect::<Vec<_>>(),
            [ExpectedLineShape::Reflow, ExpectedLineShape::Reflow]
        );
        let prompt = user_message_json(&tasks[0]);
        let units = prompt["groups"]
            .as_array()
            .expect("groups 必须是数组")
            .iter()
            .flat_map(|group| group["units"].as_array().expect("units 必须是数组"))
            .collect::<Vec<_>>();
        assert_eq!(units.len(), 2);
        assert_eq!(units[0]["id"], "0");
        assert_eq!(units[0]["type"], "free");
        assert_eq!(
            units[0]["text"],
            serde_json::json!([
                "ゲームパッドが接続されていません",
                "ボタンを押して再度試してください"
            ])
        );
        assert_eq!(units[1]["id"], "1");
        assert_eq!(units[1]["type"], "free");
    }

    #[tokio::test]
    async fn custom_placeholder_scope_is_shared_by_builtin_and_rules_owners() {
        let placeholder_path = PathBuf::from("C:/input/help-placeholders.toml");
        let reader = TranslationPlanningResourceReadingService::new(
            MemoryFileReader {
                files: Arc::new(BTreeMap::from([(
                    placeholder_path.clone(),
                    br#"
[[rule]]
scopes = ["database_entry"]
pattern = '\A<Help:(?<text>.*?)>\z'
"#
                    .to_vec(),
                )])),
            },
            ImmediateCpu,
        );
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            reader,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内建占位符应该可编译"),
            ImmediateCpu,
        );
        let original = "<Help:炎の剣の説明>";
        let source = RpgMakerSource::data(StandardDataFile::Items);
        let group = |owner, index| {
            let group_location =
                RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::index(index)]);
            let identity = TranslationUnitIdentity::new(
                owner,
                TextGroupKind::DatabaseEntry,
                group_location.clone(),
                TextUnitRole::Scalar(ScalarFieldKey::new("note").expect("字段键应合法")),
                TextUnitContent::Value(original.to_owned()),
                "{}",
            );
            RpgMakerTranslationGroup::new(
                TextGroupKind::DatabaseEntry,
                group_location,
                vec![RpgMakerTranslationAsset::new(identity, None, None)],
            )
        };
        let corpus = RpgMakerTranslationCorpus::new(vec![
            group(RpgMakerAssetOwner::Builtin, 1),
            group(RpgMakerAssetOwner::Rules, 2),
        ]);

        let (_, _, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                corpus,
                RpgMakerTranslationInput::new(None, Some(placeholder_path)),
            )
            .await
            .expect("同 kind 的两个 owner 应共享 Placeholder 与去重族")
            .into_parts();

        assert_eq!(tasks.len(), 1);
        let [output] = tasks[0].expected_outputs() else {
            panic!("同原文同 kind 应只有一个活动代表")
        };
        assert_eq!(output.identity().owner(), RpgMakerAssetOwner::Builtin);
        assert_eq!(output.propagation_targets().len(), 1);
        assert_eq!(
            output.propagation_targets()[0].owner(),
            RpgMakerAssetOwner::Rules
        );
        assert_eq!(
            output
                .applied_placeholders()
                .iter()
                .map(AppliedPlaceholder::original)
                .collect::<Vec<_>>(),
            ["<Help:", ">"]
        );
    }

    #[tokio::test]
    async fn one_minimal_message_can_mix_all_five_semantic_unit_roles() {
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let scalar = map_group(TextGroupKind::Map, None, None, None, "始まりの町");
        let dialogue = map_unit_group(
            TextGroupKind::EventDialogue,
            0,
            vec![
                (
                    TextUnitRole::DialogueSpeaker,
                    TextUnitContent::Value("アリス".to_owned()),
                    "{}",
                ),
                (
                    TextUnitRole::DialogueBody,
                    TextUnitContent::Lines(vec![
                        "今日はいい天気ですね。".to_owned(),
                        "一緒に町へ".to_owned(),
                        "行きませんか？".to_owned(),
                    ]),
                    r#"{"source_speaker":"アリス"}"#,
                ),
            ],
        );
        let choices = map_unit_group(
            TextGroupKind::EventChoices,
            10,
            vec![(
                TextUnitRole::Choices,
                TextUnitContent::Lines(vec!["はい".to_owned(), "いいえ".to_owned()]),
                "{}",
            )],
        );
        let scrolling = map_unit_group(
            TextGroupKind::EventScrollingText,
            20,
            vec![(
                TextUnitRole::ScrollingText,
                TextUnitContent::Lines(vec!["制作".to_owned(), String::new(), "終わり".to_owned()]),
                "{}",
            )],
        );

        let (_, _, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                RpgMakerTranslationCorpus::new(vec![scrolling, choices, dialogue, scalar]),
                RpgMakerTranslationInput::new(None, None),
            )
            .await
            .expect("同一 Map 的五种角色应进入同一个请求")
            .into_parts();

        assert_eq!(tasks.len(), 1);
        assert_eq!(
            tasks[0]
                .expected_outputs()
                .iter()
                .map(ExpectedTranslationOutput::id)
                .collect::<Vec<_>>(),
            vec![task_id(0), task_id(1), task_id(2), task_id(3), task_id(4)]
        );
        assert_eq!(
            tasks[0]
                .expected_outputs()
                .iter()
                .map(ExpectedTranslationOutput::line_shape)
                .collect::<Vec<_>>(),
            vec![
                ExpectedLineShape::Aligned(NonZeroUsize::new(3).unwrap()),
                ExpectedLineShape::Aligned(NonZeroUsize::new(2).unwrap()),
                ExpectedLineShape::Aligned(NonZeroUsize::MIN),
                ExpectedLineShape::Reflow,
                ExpectedLineShape::Aligned(NonZeroUsize::MIN),
            ]
        );
        let user = tasks[0].messages()[1].content();
        assert_eq!(
            parse_user_message_json(user),
            serde_json::json!({
                "groups": [
                    {
                        "kind": "event_scrolling_text",
                        "units": [{
                            "id": "0",
                            "role": "scrolling_text",
                            "type": "strict",
                            "text": ["制作", "", "終わり"]
                        }]
                    },
                    {
                        "kind": "event_choices",
                        "units": [{
                            "id": "1",
                            "role": "choices",
                            "type": "strict",
                            "text": ["はい", "いいえ"]
                        }]
                    },
                    {
                        "kind": "event_dialogue",
                        "units": [
                            {
                                "id": "2",
                                "role": "speaker",
                                "type": "strict",
                                "text": ["アリス"]
                            },
                            {
                                "id": "3",
                                "role": "body",
                                "type": "free",
                                "text": [
                                    "今日はいい天気ですね。",
                                    "一緒に町へ",
                                    "行きませんか？"
                                ]
                            }
                        ]
                    },
                    {
                        "kind": "map",
                        "units": [{
                            "id": "4",
                            "role": "displayName",
                            "type": "strict",
                            "text": ["始まりの町"]
                        }]
                    }
                ]
            })
        );
    }

    #[tokio::test]
    async fn current_speaker_translation_is_unnumbered_context_for_active_body() {
        let resources = translation_resources();
        let placeholders = Pcre2PlaceholderService::new().expect("内置占位符应该可编译");
        let custom = placeholders
            .compile_custom(Vec::new())
            .expect("空占位符规则应可编译");
        let group_location = RpgMakerLocation::value(
            RpgMakerSource::map(1),
            vec![
                RpgMakerLocationStep::key("events"),
                RpgMakerLocationStep::index(1),
                RpgMakerLocationStep::key("pages"),
                RpgMakerLocationStep::index(0),
                RpgMakerLocationStep::key("list"),
                RpgMakerLocationStep::index(0),
            ],
        );
        let speaker = TranslationUnitIdentity::new(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            group_location.clone(),
            TextUnitRole::DialogueSpeaker,
            TextUnitContent::Value("アリス".to_owned()),
            "{}",
        );
        let body = TranslationUnitIdentity::new(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            group_location.clone(),
            TextUnitRole::DialogueBody,
            TextUnitContent::Lines(vec!["出かけましょう。".to_owned()]),
            r#"{"source_speaker":"アリス"}"#,
        );
        let fingerprint_group = RpgMakerTranslationGroup::new(
            TextGroupKind::EventDialogue,
            group_location.clone(),
            vec![
                RpgMakerTranslationAsset::new(speaker.clone(), None, None),
                RpgMakerTranslationAsset::new(body.clone(), None, None),
            ],
        );
        let (protected_speaker, bindings) = placeholders
            .protect(
                RpgMakerEngine::Mz,
                TextGroupKind::EventDialogue,
                "アリス",
                &custom,
            )
            .expect("说话人应可保护")
            .into_parts();
        let global = global_translation_semantics(
            RpgMakerEngine::Mz,
            "ja",
            "zh-Hans",
            resources.source_language().semantic_fingerprint(),
            "# System\n完整且由外部提供。",
            resources.system_prompt().response_mode(),
            ().semantic_fingerprint(),
        );
        let speaker_translation = TextUnitContent::Value("爱丽丝".to_owned());
        let speaker_state = translation_state_context_for_group(
            global,
            &fingerprint_group,
            &speaker,
            &protected_speaker,
            &bindings,
            &[],
        )
        .finish(&speaker_translation);
        let corpus = RpgMakerTranslationCorpus::new(vec![RpgMakerTranslationGroup::new(
            TextGroupKind::EventDialogue,
            group_location,
            vec![
                RpgMakerTranslationAsset::new(
                    speaker,
                    Some(speaker_translation),
                    Some(speaker_state),
                ),
                RpgMakerTranslationAsset::new(body, None, None),
            ],
        )]);
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            resources,
            placeholders,
            ImmediateCpu,
        );

        let (_, _, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                corpus,
                RpgMakerTranslationInput::new(None, None),
            )
            .await
            .expect("已有说话人译文应作为正文语境")
            .into_parts();

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].expected_outputs().len(), 1);
        assert_eq!(tasks[0].expected_outputs()[0].id(), task_id(0));
        let user = user_message_json(&tasks[0]);
        assert_eq!(user["groups"][0]["units"][0]["role"], "speaker");
        assert_eq!(
            user["groups"][0]["units"][0]["text"],
            serde_json::json!(["爱丽丝"])
        );
        assert!(user["groups"][0]["units"][0].get("id").is_none());
        assert_eq!(user["groups"][0]["units"][1]["id"], "0");
        assert_eq!(user["groups"][0]["units"][1]["role"], "body");
        assert_eq!(user["groups"][0]["units"][1]["type"], "free");
    }

    #[tokio::test]
    async fn context_only_group_keeps_terminology_and_placeholder_safe_target_text() {
        let terminology_path = PathBuf::from("C:/input/terms.toml");
        let placeholder_path = PathBuf::from("C:/input/placeholders.toml");
        let reader = TranslationPlanningResourceReadingService::new(
            MemoryFileReader {
                files: Arc::new(BTreeMap::from([
                    (
                        terminology_path.clone(),
                        r#"
[[term]]
term = "魔王"
translation = "魔王（Demon King）"
"#
                        .as_bytes()
                        .to_vec(),
                    ),
                    (
                        placeholder_path.clone(),
                        r#"
[[rule]]
scopes = ["database_entry"]
pattern = '\{[^}]+\}'
"#
                        .as_bytes()
                        .to_vec(),
                    ),
                ])),
            },
            ImmediateCpu,
        );
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            reader,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let source = RpgMakerSource::data(StandardDataFile::Items);
        let group_location =
            RpgMakerLocation::value(source.clone(), vec![RpgMakerLocationStep::index(1)]);
        let original = "魔王です {hero}";
        let target = "已有上下文 {hero}";
        let identity = TranslationUnitIdentity::new(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            group_location.clone(),
            TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法")),
            TextUnitContent::Value(original.to_owned()),
            "{}",
        );
        let terminology = vec![TerminologyDependency::new("魔王", "魔王（Demon King）")];
        let state = translation_state_for(
            &identity,
            original,
            target,
            &terminology,
            vec![super::super::placeholder::PlaceholderRuleDefinition::new(
                None,
                r"\{[^}]+\}",
            )],
        );
        let context_group = RpgMakerTranslationGroup::new(
            TextGroupKind::DatabaseEntry,
            group_location,
            vec![RpgMakerTranslationAsset::new(
                identity,
                Some(TextUnitContent::Value(target.to_owned())),
                Some(state),
            )],
        );
        let active_group = group(source, 2, "こんにちは", None, Vec::new());

        let (_, _, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                RpgMakerTranslationCorpus::new(vec![context_group, active_group]),
                RpgMakerTranslationInput::new(Some(terminology_path), Some(placeholder_path)),
            )
            .await
            .expect("无 ID Group 仍应完成术语和 Placeholder 安全语境准备")
            .into_parts();

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].expected_outputs().len(), 1);
        let user = user_message_json(&tasks[0]);
        assert_eq!(
            user["terminology"],
            serde_json::json!([{
                "source": "魔王",
                "translation": "魔王（Demon King）"
            }]),
            "完整块术语应按文件顺序只提供一次"
        );
        let context = &user["groups"][0]["units"][0];
        assert!(context.get("id").is_none());
        assert!(
            context["text"][0]
                .as_str()
                .expect("语境文本必须是字符串")
                .starts_with("已有上下文 ⟦ATT_")
        );
        assert_eq!(user["groups"][1]["units"][0]["id"], "0");
        assert_eq!(
            user["groups"][1]["units"][0]["text"],
            serde_json::json!(["こんにちは"])
        );
        assert!(!user.to_string().contains("{hero}"));
    }

    #[tokio::test]
    async fn automatic_current_state_uses_terms_from_every_unit_in_its_complete_group() {
        let terminology_path = PathBuf::from("C:/input/terms.toml");
        let reader = TranslationPlanningResourceReadingService::new(
            MemoryFileReader {
                files: Arc::new(BTreeMap::from([(
                    terminology_path.clone(),
                    r#"
[[term]]
term = "魔王"
translation = "Demon King"

[[term]]
term = "勇者"
translation = "Hero"
"#
                    .as_bytes()
                    .to_vec(),
                )])),
            },
            ImmediateCpu,
        );
        let resources = translation_resources();
        let placeholders = Pcre2PlaceholderService::new().expect("内置占位符应该可编译");
        let custom = placeholders
            .compile_custom(Vec::new())
            .expect("空 Placeholder 规则应可编译");
        let group_location = RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Actors),
            vec![RpgMakerLocationStep::index(1)],
        );
        let group_order = RpgMakerSemanticOrderKey::new(vec![1], 0);
        let first_order = RpgMakerSemanticOrderKey::new(vec![1], 1);
        let second_order = RpgMakerSemanticOrderKey::new(vec![1], 2);
        let first_identity = TranslationUnitIdentity::new(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            group_location.clone(),
            TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法")),
            TextUnitContent::Value("勇者です".to_owned()),
            "{}",
        );
        let second_identity = TranslationUnitIdentity::new(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            group_location.clone(),
            TextUnitRole::Scalar(ScalarFieldKey::new("description").expect("字段键应合法")),
            TextUnitContent::Value("魔王です".to_owned()),
            "{}",
        );
        let group_without_states = RpgMakerTranslationGroup::with_semantic_order_key(
            TextGroupKind::DatabaseEntry,
            group_location.clone(),
            group_order.clone(),
            vec![
                RpgMakerTranslationAsset::with_semantic_order_key(
                    first_identity.clone(),
                    first_order.clone(),
                    None,
                    None,
                ),
                RpgMakerTranslationAsset::with_semantic_order_key(
                    second_identity.clone(),
                    second_order.clone(),
                    None,
                    None,
                ),
            ],
        );
        let group_terms = vec![
            TerminologyDependency::new("魔王", "Demon King"),
            TerminologyDependency::new("勇者", "Hero"),
        ];
        let global = global_translation_semantics(
            RpgMakerEngine::Mz,
            "ja",
            "zh-Hans",
            resources.source_language().semantic_fingerprint(),
            resources.system_prompt().markdown(),
            resources.system_prompt().response_mode(),
            ().semantic_fingerprint(),
        );
        let first_translation = TextUnitContent::Value("勇者译文".to_owned());
        let second_translation = TextUnitContent::Value("魔王译文".to_owned());
        let first_prepared = placeholders
            .protect(
                RpgMakerEngine::Mz,
                TextGroupKind::DatabaseEntry,
                "勇者です",
                &custom,
            )
            .expect("第一项原文应可保护");
        let (first_protected, first_bindings) = first_prepared.into_parts();
        let second_prepared = placeholders
            .protect(
                RpgMakerEngine::Mz,
                TextGroupKind::DatabaseEntry,
                "魔王です",
                &custom,
            )
            .expect("第二项原文应可保护");
        let (second_protected, second_bindings) = second_prepared.into_parts();
        let first_state = translation_state_context_for_group(
            global,
            &group_without_states,
            &first_identity,
            &first_protected,
            &first_bindings,
            &group_terms,
        )
        .finish(&first_translation);
        let second_state = translation_state_context_for_group(
            global,
            &group_without_states,
            &second_identity,
            &second_protected,
            &second_bindings,
            &group_terms,
        )
        .finish(&second_translation);
        let group = RpgMakerTranslationGroup::with_semantic_order_key(
            TextGroupKind::DatabaseEntry,
            group_location,
            group_order,
            vec![
                RpgMakerTranslationAsset::with_semantic_order_key(
                    first_identity,
                    first_order,
                    Some(first_translation),
                    Some(first_state),
                ),
                RpgMakerTranslationAsset::with_semantic_order_key(
                    second_identity,
                    second_order,
                    Some(second_translation),
                    Some(second_state),
                ),
            ],
        );
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            reader,
            resources,
            placeholders,
            ImmediateCpu,
        );

        let (_, preparation, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                RpgMakerTranslationCorpus::new(vec![group]),
                RpgMakerTranslationInput::new(Some(terminology_path), None),
            )
            .await
            .expect("完整 Group 的两项自动译文都应保持 Current")
            .into_parts();

        assert_eq!(preparation.invalidated(), 0);
        assert!(preparation.invalidations().is_empty());
        assert!(tasks.is_empty(), "全部 Current 的完整块不得进入翻译流水线");
    }

    #[tokio::test]
    async fn manual_current_non_source_and_fully_protected_units_use_safe_target_context() {
        let placeholder_path = PathBuf::from("C:/input/placeholders.toml");
        let placeholder_toml = r#"
[[rule]]
scopes = ["database_entry"]
pattern = '\{[^}]+\}'

[[rule]]
scopes = ["database_entry"]
pattern = '保護対象'
"#;
        let definitions = vec![
            super::super::placeholder::PlaceholderRuleDefinition::new(
                Some(vec!["database_entry".to_owned()]),
                r"\{[^}]+\}",
            ),
            super::super::placeholder::PlaceholderRuleDefinition::new(
                Some(vec!["database_entry".to_owned()]),
                "保護対象",
            ),
        ];
        let semantics =
            ResolvedTranslationSemantics::for_test_with_placeholders(definitions.clone());
        assert_eq!(
            semantics
                .prepare_content(
                    TextGroupKind::DatabaseEntry,
                    &TextUnitContent::Value("12345 {hero}".to_owned()),
                )
                .expect("非源语原文应可准备")
                .status(),
            PreparedTranslationStatus::NonSourceLanguage,
        );
        assert_eq!(
            semantics
                .prepare_content(
                    TextGroupKind::DatabaseEntry,
                    &TextUnitContent::Value("保護対象".to_owned()),
                )
                .expect("完全保护原文应可准备")
                .status(),
            PreparedTranslationStatus::FullyProtected,
        );

        let reader = TranslationPlanningResourceReadingService::new(
            MemoryFileReader {
                files: Arc::new(BTreeMap::from([(
                    placeholder_path.clone(),
                    placeholder_toml.as_bytes().to_vec(),
                )])),
            },
            ImmediateCpu,
        );
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            reader,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let source = RpgMakerSource::data(StandardDataFile::Items);
        let non_source =
            manual_current_group(source.clone(), 1, "12345 {hero}", "人工数字语境 {hero}");
        let fully_protected =
            manual_current_group(source.clone(), 2, "保護対象", "完整保护语境 保護対象");
        let active = group(source, 3, "翻訳対象", None, Vec::new());

        let (_, preparation, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                RpgMakerTranslationCorpus::new(vec![non_source, fully_protected, active]),
                RpgMakerTranslationInput::new(None, Some(placeholder_path)),
            )
            .await
            .expect("人工 Current 的无编号语境应使用安全目标文本")
            .into_parts();

        assert_eq!(preparation.not_applicable(), 2);
        assert_eq!(preparation.invalidated(), 0);
        assert!(preparation.invalidations().is_empty());
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].expected_outputs().len(), 1);
        let user = user_message_json(&tasks[0]);
        let units = user["groups"]
            .as_array()
            .expect("groups 必须是数组")
            .iter()
            .flat_map(|group| group["units"].as_array().expect("units 必须是数组"))
            .collect::<Vec<_>>();
        assert_eq!(units.len(), 3);
        assert!(units[0].get("id").is_none());
        assert!(
            units[0]["text"][0]
                .as_str()
                .expect("语境文本必须是字符串")
                .starts_with("人工数字语境 ⟦ATT_")
        );
        assert!(units[1].get("id").is_none());
        assert!(
            units[1]["text"][0]
                .as_str()
                .expect("语境文本必须是字符串")
                .starts_with("完整保护语境 ⟦ATT_")
        );
        assert_eq!(units[2]["id"], "0");
        assert_eq!(units[2]["text"], serde_json::json!(["翻訳対象"]));
        let user_text = user.to_string();
        assert!(!user_text.contains("12345 {hero}"));
        assert!(!user_text.contains("{hero}"));
        assert!(!user_text.contains("保護対象"));
    }

    #[tokio::test]
    async fn target_language_uses_the_exact_resolved_system_prompt() {
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources_for("ja", "zh-Hant"),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );

        let (_, _, tasks) = planner
            .plan(
                &project_with_languages("ja", "zh-Hant"),
                &profile(10_000),
                RpgMakerTranslationCorpus::new(vec![group(
                    RpgMakerSource::data(StandardDataFile::Items),
                    1,
                    "翻訳対象",
                    None,
                    Vec::new(),
                )]),
                RpgMakerTranslationInput::new(None, None),
            )
            .await
            .expect("目标语言没有对应模块时仍应按精确 system Markdown 规划")
            .into_parts();

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].language_pair().target().as_str(), "zh-Hant");
        assert!(
            tasks[0].expected_outputs()[0]
                .language_analysis()
                .needs_translation()
        );
    }

    #[tokio::test]
    async fn whole_maps_are_independent_semantic_scopes_even_with_large_target() {
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let corpus = RpgMakerTranslationCorpus::new(vec![
            group(RpgMakerSource::map(1), 0, "一番目", None, Vec::new()),
            group(RpgMakerSource::map(2), 0, "二番目", None, Vec::new()),
        ]);

        let (_, _, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                corpus,
                RpgMakerTranslationInput::new(None, None),
            )
            .await
            .expect("两个 Map 应分别规划")
            .into_parts();

        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].index().get(), 0);
        assert_eq!(tasks[1].index().get(), 1);
    }

    #[tokio::test]
    async fn map_groups_preserve_reader_natural_order() {
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let display = map_group(TextGroupKind::Map, None, None, None, "始まりの町");
        let event_1_page_0_command_2 = map_group(
            TextGroupKind::EventDialogue,
            Some(1),
            Some(0),
            Some(2),
            "最初の会話",
        );
        let event_1_page_1_command_0 = map_group(
            TextGroupKind::EventDialogue,
            Some(1),
            Some(1),
            Some(0),
            "次のページ",
        );
        let event_2_page_0_command_0 = map_group(
            TextGroupKind::EventDialogue,
            Some(2),
            Some(0),
            Some(0),
            "次のイベント",
        );

        let (_, _, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                RpgMakerTranslationCorpus::new(vec![
                    display,
                    event_1_page_0_command_2,
                    event_1_page_1_command_0,
                    event_2_page_0_command_0,
                ]),
                RpgMakerTranslationInput::new(None, None),
            )
            .await
            .expect("Reader 已验证的 Map 自然顺序应原样进入计划")
            .into_parts();

        assert_eq!(tasks.len(), 1);
        let user = tasks[0].messages()[1].content();
        let display = user.find("始まりの町").expect("地图名应在请求中");
        let first = user.find("最初の会話").expect("首个事件应在请求中");
        let next_page = user.find("次のページ").expect("下一页应在请求中");
        let next_event = user.find("次のイベント").expect("下一事件应在请求中");
        assert!(display < first && first < next_page && next_page < next_event);
    }

    #[tokio::test]
    async fn fully_protected_source_text_becomes_virtual_context() {
        let placeholder_path = PathBuf::from("C:/input/placeholders.toml");
        let reader = TranslationPlanningResourceReadingService::new(
            MemoryFileReader {
                files: Arc::new(BTreeMap::from([(
                    placeholder_path.clone(),
                    r#"
                        [[rule]]
                        scopes = ["database_entry"]
                        pattern = '保護対象'
                    "#
                    .as_bytes()
                    .to_vec(),
                )])),
            },
            ImmediateCpu,
        );
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            reader,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let corpus = RpgMakerTranslationCorpus::new(vec![
            group(
                RpgMakerSource::data(StandardDataFile::Items),
                1,
                "保護対象",
                None,
                Vec::new(),
            ),
            group(
                RpgMakerSource::data(StandardDataFile::Items),
                2,
                "翻訳対象",
                None,
                Vec::new(),
            ),
        ]);

        let (_, _, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                corpus,
                RpgMakerTranslationInput::new(None, Some(placeholder_path)),
            )
            .await
            .expect("整段保护应取消该单元的翻译要求")
            .into_parts();

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].expected_outputs().len(), 1);
        assert_eq!(tasks[0].expected_outputs()[0].id(), task_id(0));
        assert!(!tasks[0].messages()[1].content().contains("保護対象"));
        assert!(!tasks[0].messages()[1].content().contains("仅上下文"));
    }

    #[tokio::test]
    async fn terminology_for_prompt_is_the_same_natural_segment_match_prepared_for_state() {
        let terminology_path = PathBuf::from("C:/input/terms.toml");
        let placeholder_path = PathBuf::from("C:/input/placeholders.toml");
        let reader = TranslationPlanningResourceReadingService::new(
            MemoryFileReader {
                files: Arc::new(BTreeMap::from([
                    (
                        terminology_path.clone(),
                        r#"
                            [[term]]
                            term = "秘密"
                            translation = "机密"

                            [[term]]
                            term = "前後"
                            translation = "前后"

                            [[term]]
                            term = "勇者"
                            translation = "英雄"

                            [[term]]
                            term = "プフクス"
                            translation = "普芙库丝"

                            [[term]]
                            term = "プフクスッ"
                            translation = "噗呼咯"
                        "#
                        .as_bytes()
                        .to_vec(),
                    ),
                    (
                        placeholder_path.clone(),
                        r#"
                            [[rule]]
                            pattern = '<code:[^>]+>'
                        "#
                        .as_bytes()
                        .to_vec(),
                    ),
                ])),
            },
            ImmediateCpu,
        );
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            reader,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );

        let (_, _, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                RpgMakerTranslationCorpus::new(vec![group(
                    RpgMakerSource::data(StandardDataFile::Items),
                    1,
                    r"<code:秘密>前\C[2]後勇者とプフクスッ翻訳",
                    None,
                    Vec::new(),
                )]),
                RpgMakerTranslationInput::new(Some(terminology_path), Some(placeholder_path)),
            )
            .await
            .expect("自然段术语应可建立任务")
            .into_parts();

        let user = user_message_json(&tasks[0]);
        assert_eq!(
            user["terminology"],
            serde_json::json!([
                {
                    "source": "勇者",
                    "translation": "英雄"
                },
                {
                    "source": "プフクスッ",
                    "translation": "噗呼咯"
                }
            ]),
            "术语数组必须只包含自然文本段实际命中的条目，并保持术语文件顺序"
        );
    }

    #[tokio::test]
    async fn terminology_prompt_does_not_join_distinct_lines_elements() {
        let terminology_path = PathBuf::from("C:/input/terms.toml");
        let reader = TranslationPlanningResourceReadingService::new(
            MemoryFileReader {
                files: Arc::new(BTreeMap::from([(
                    terminology_path.clone(),
                    r#"
                        [[term]]
                        term = "跨元素"
                        translation = "不应命中"
                        triggers = ["海へ\n出よう"]

                    "#
                    .as_bytes()
                    .to_vec(),
                )])),
            },
            ImmediateCpu,
        );
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            reader,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let body = map_unit_group(
            TextGroupKind::EventDialogue,
            0,
            vec![(
                TextUnitRole::DialogueBody,
                TextUnitContent::Lines(vec![
                    "海へ".to_owned(),
                    "出よう".to_owned(),
                    "別の翻訳".to_owned(),
                ]),
                "{}",
            )],
        );

        let (_, _, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                RpgMakerTranslationCorpus::new(vec![body]),
                RpgMakerTranslationInput::new(Some(terminology_path), None),
            )
            .await
            .expect("Lines 术语扫描域应可建立任务")
            .into_parts();

        let user = tasks[0].messages()[1].content();
        assert!(!user.contains("- 跨元素 →"));
    }

    #[tokio::test]
    async fn global_deduplication_assigns_one_id_but_keeps_every_duplicate_as_context() {
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let first = group(
            RpgMakerSource::data(StandardDataFile::Items),
            1,
            "保存しますか？",
            None,
            Vec::new(),
        );
        let last_duplicate = first.assets()[0].identity().clone();
        let duplicate = group(
            RpgMakerSource::data(StandardDataFile::Items),
            2,
            "保存しますか？",
            None,
            Vec::new(),
        );
        let natural_leader = duplicate.assets()[0].identity().clone();
        let neighbouring = group(
            RpgMakerSource::data(StandardDataFile::Items),
            3,
            "別の翻訳対象です。",
            None,
            Vec::new(),
        );

        let (_, preparation, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                RpgMakerTranslationCorpus::new(vec![neighbouring, duplicate, first]),
                RpgMakerTranslationInput::new(None, None),
            )
            .await
            .expect("跨语义范围的相同原文应只建立一个输出")
            .into_parts();

        assert!(preparation.invalidations().is_empty());
        assert!(preparation.reuses().is_empty());
        assert_eq!(tasks.len(), 1);
        let deduplicated = tasks[0]
            .expected_outputs()
            .iter()
            .find(|output| output.identity() == &natural_leader)
            .expect("Reader 自然顺序中的首个重复单元应成为代表");
        assert_eq!(deduplicated.propagation_targets(), &[last_duplicate]);
        let user = user_message_json(&tasks[0]);
        let duplicate_units = user["groups"]
            .as_array()
            .expect("groups 必须是数组")
            .iter()
            .flat_map(|group| group["units"].as_array().expect("units 必须是数组"))
            .filter(|unit| unit["text"] == serde_json::json!(["保存しますか？"]))
            .collect::<Vec<_>>();
        assert_eq!(
            duplicate_units.len(),
            2,
            "去重只合并模型责任，不能从完整 TaskBlock 删除重复原文语境"
        );
        assert!(duplicate_units.iter().any(|unit| unit["id"] == "1"));
        assert!(duplicate_units.iter().any(|unit| unit.get("id").is_none()));
        assert_eq!(tasks[0].expected_outputs().len(), 2);
    }

    #[tokio::test]
    async fn valid_existing_translation_reuses_without_creating_an_llm_task() {
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let seed = group(
            RpgMakerSource::data(StandardDataFile::Items),
            1,
            "保存",
            Some("Save"),
            Vec::new(),
        );
        let target = group(
            RpgMakerSource::data(StandardDataFile::Items),
            2,
            "保存",
            None,
            Vec::new(),
        );

        let (_, preparation, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                RpgMakerTranslationCorpus::new(vec![target, seed]),
                RpgMakerTranslationInput::new(None, None),
            )
            .await
            .expect("已有唯一有效译文应在准备阶段直接复用")
            .into_parts();

        assert!(preparation.invalidations().is_empty());
        assert_eq!(preparation.reuses().len(), 1);
        assert_eq!(
            preparation.reuses()[0].seed().expected_translation(),
            &TextUnitContent::Value("Save".to_owned())
        );
        assert_eq!(preparation.reuses()[0].targets().len(), 1);
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn exact_state_is_current_without_renormalizing_repeated_original_placeholders() {
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let corpus = RpgMakerTranslationCorpus::new(vec![group(
            RpgMakerSource::data(StandardDataFile::Items),
            1,
            r"\C[2]翻訳\C[2]",
            Some(r"\C[2]译文\C[2]"),
            Vec::new(),
        )]);

        let (_, preparation, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                corpus,
                RpgMakerTranslationInput::new(None, None),
            )
            .await
            .expect("精确 state 应直接复用既有译文")
            .into_parts();

        assert_eq!(preparation.retained(), 1);
        assert!(preparation.invalidations().is_empty());
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn inserting_an_unmatched_placeholder_rule_keeps_the_existing_translation_current() {
        let placeholder_path = PathBuf::from("C:/input/placeholders.toml");
        let reader = TranslationPlanningResourceReadingService::new(
            MemoryFileReader {
                files: Arc::new(BTreeMap::from([(
                    placeholder_path.clone(),
                    r#"
                        [[rule]]
                        pattern = 'DOES_NOT_MATCH'

                        [[rule]]
                        pattern = '<TOKEN>'
                    "#
                    .as_bytes()
                    .to_vec(),
                )])),
            },
            ImmediateCpu,
        );
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            reader,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let original = "翻訳<TOKEN>";
        let translation = "译文<TOKEN>";
        let base = group(
            RpgMakerSource::data(StandardDataFile::Items),
            1,
            original,
            None,
            Vec::new(),
        );
        let identity = base.assets()[0].identity().clone();
        let state = translation_state_for(
            &identity,
            original,
            translation,
            &[],
            vec![super::super::placeholder::PlaceholderRuleDefinition::new(
                None, "<TOKEN>",
            )],
        );
        let corpus = RpgMakerTranslationCorpus::new(vec![RpgMakerTranslationGroup::new(
            base.kind(),
            base.group_location().clone(),
            vec![RpgMakerTranslationAsset::new(
                identity,
                Some(TextUnitContent::Value(translation.to_owned())),
                Some(state),
            )],
        )]);

        let (_, preparation, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                corpus,
                RpgMakerTranslationInput::new(None, Some(placeholder_path)),
            )
            .await
            .expect("不命中规则的插入不应使有效译文失效")
            .into_parts();

        assert_eq!(preparation.retained(), 1);
        assert!(preparation.invalidations().is_empty());
        assert!(tasks.is_empty());
    }

    #[test]
    fn planning_failure_projection_preserves_typed_backend_fields() {
        let cases = vec![
            (
                LanguageTextProjectionError::TokenIndexConstruction,
                TranslationPlaceholderProjectionFailure::TokenIndexConstruction,
            ),
            (
                LanguageTextProjectionError::EmptyToken,
                TranslationPlaceholderProjectionFailure::EmptyToken,
            ),
            (
                LanguageTextProjectionError::MissingToken {
                    token: "<ATT_MISSING>".to_owned(),
                },
                TranslationPlaceholderProjectionFailure::MissingToken {
                    token: "<ATT_MISSING>".to_owned(),
                },
            ),
            (
                LanguageTextProjectionError::RepeatedToken {
                    token: "<ATT_REPEAT>".to_owned(),
                },
                TranslationPlaceholderProjectionFailure::RepeatedToken {
                    token: "<ATT_REPEAT>".to_owned(),
                },
            ),
            (
                LanguageTextProjectionError::OverlappingToken {
                    token: "<ATT_OVERLAP>".to_owned(),
                },
                TranslationPlaceholderProjectionFailure::OverlappingToken {
                    token: "<ATT_OVERLAP>".to_owned(),
                },
            ),
            (
                LanguageTextProjectionError::ChangedTokenOrder {
                    position: 3,
                    expected_token: "<ATT_EXPECTED>".to_owned(),
                    actual_token: "<ATT_ACTUAL>".to_owned(),
                },
                TranslationPlaceholderProjectionFailure::ChangedTokenOrder {
                    position: 3,
                    expected_token: "<ATT_EXPECTED>".to_owned(),
                    actual_token: "<ATT_ACTUAL>".to_owned(),
                },
            ),
            (
                LanguageTextProjectionError::ChangedSegmentCount {
                    expected: 5,
                    actual: 4,
                },
                TranslationPlaceholderProjectionFailure::ChangedSegmentCount {
                    expected: 5,
                    actual: 4,
                },
            ),
            (
                LanguageTextProjectionError::ChangedSegmentKind { segment_index: 7 },
                TranslationPlaceholderProjectionFailure::ChangedSegmentKind { segment_index: 7 },
            ),
            (
                LanguageTextProjectionError::MissingOrderedToken { segment_index: 2 },
                TranslationPlaceholderProjectionFailure::MissingOrderedToken { segment_index: 2 },
            ),
            (
                LanguageTextProjectionError::UnusedOrderedToken,
                TranslationPlaceholderProjectionFailure::UnusedOrderedToken,
            ),
        ];

        for (source, expected) in cases {
            assert_eq!(placeholder_projection_planning_failure(source), expected);
        }
    }

    #[test]
    fn planning_failure_protection_preserves_worker_fields() {
        let worker_source = io::Error::from_raw_os_error(8);
        let expected_io_kind = worker_source.kind();
        assert_eq!(
            placeholder_protection_planning_failure(PlaceholderProtectionError::StartWorker {
                operation: PlaceholderWorkerOperation::MatchText,
                source: worker_source,
            }),
            TranslationPlaceholderProtectionFailure::WorkerStart {
                operation: PlaceholderWorkerOperation::MatchText,
                io_kind: expected_io_kind,
                raw_os_code: Some(8),
            }
        );
    }

    #[tokio::test]
    async fn missing_text_capture_keeps_rule_range_and_unit_locator_without_source_body() {
        let placeholder_path = PathBuf::from("C:/input/placeholders.toml");
        let reader = TranslationPlanningResourceReadingService::new(
            MemoryFileReader {
                files: Arc::new(BTreeMap::from([(
                    placeholder_path.clone(),
                    r#"
                        [[rule]]
                        pattern = '(?:(?<text>保留)|欠落)'
                    "#
                    .as_bytes()
                    .to_vec(),
                )])),
            },
            ImmediateCpu,
        );
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            reader,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let sensitive_source = "翻译欠落です";

        let failure = expect_unit_preparation_failure(
            planner
                .plan(
                    &project(),
                    &profile(10_000),
                    RpgMakerTranslationCorpus::new(vec![group(
                        RpgMakerSource::data(StandardDataFile::Items),
                        4,
                        sensitive_source,
                        None,
                        Vec::new(),
                    )]),
                    RpgMakerTranslationInput::new(None, Some(placeholder_path)),
                )
                .await,
        );
        assert_eq!(
            failure.reason(),
            &TranslationPlanningFailureReason::PlaceholderProtection {
                failure: TranslationPlaceholderProtectionFailure::MissingTextCapture {
                    rule_number: 1,
                    whole_match_start_byte: "翻译".len(),
                    whole_match_end_byte: "翻译欠落".len(),
                },
            }
        );
        assert_eq!(
            failure.identity().group_location().source(),
            &RpgMakerSource::data(StandardDataFile::Items)
        );
        assert_eq!(
            failure.identity().role(),
            &TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法"))
        );
        let diagnostic =
            serde_json::to_value(failure.diagnostic_report()).expect("规划失败诊断必须可序列化");
        assert_eq!(
            diagnostic["primary"]["code"],
            "translation.placeholder.missing_text_capture"
        );
        assert_eq!(
            diagnostic["primary"]["issue"]["details"]["problem"]["rule_source"]["path"],
            "C:/input/placeholders.toml"
        );
        assert_eq!(
            diagnostic["primary"]["issue"]["details"]["problem"]["unit"]["role"]["field"],
            "name"
        );
        assert!(!format!("{:?}", failure.reason()).contains(sensitive_source));
        assert!(!diagnostic.to_string().contains(sensitive_source));
    }

    #[tokio::test]
    async fn empty_placeholder_match_keeps_rule_and_utf8_byte_range() {
        let placeholder_path = PathBuf::from("C:/input/placeholders.toml");
        let reader = TranslationPlanningResourceReadingService::new(
            MemoryFileReader {
                files: Arc::new(BTreeMap::from([(
                    placeholder_path.clone(),
                    r#"
                        [[rule]]
                        pattern = '(?=欠落)'
                    "#
                    .as_bytes()
                    .to_vec(),
                )])),
            },
            ImmediateCpu,
        );
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            reader,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );

        let failure = expect_unit_preparation_failure(
            planner
                .plan(
                    &project(),
                    &profile(10_000),
                    RpgMakerTranslationCorpus::new(vec![group(
                        RpgMakerSource::data(StandardDataFile::Items),
                        4,
                        "翻译欠落です",
                        None,
                        Vec::new(),
                    )]),
                    RpgMakerTranslationInput::new(None, Some(placeholder_path)),
                )
                .await,
        );
        let TranslationPlanningFailureReason::PlaceholderProtection {
            failure: TranslationPlaceholderProtectionFailure::EmptyMatch { matched },
        } = failure.reason()
        else {
            panic!("应保留类型化的 Placeholder 空匹配")
        };
        assert_eq!(matched.rule().origin(), PlaceholderRuleOrigin::Custom);
        assert_eq!(matched.rule().rule_number(), Some(1));
        assert_eq!(matched.start_byte(), "翻译".len());
        assert_eq!(matched.end_byte(), "翻译".len());
    }

    #[tokio::test]
    async fn placeholder_overlap_failure_aborts_the_complete_translate_plan() {
        let placeholder_path = PathBuf::from("C:/input/placeholders.toml");
        let reader = TranslationPlanningResourceReadingService::new(
            MemoryFileReader {
                files: Arc::new(BTreeMap::from([(
                    placeholder_path.clone(),
                    br#"
                        [[rule]]
                        pattern = '<BAD>'

                        [[rule]]
                        pattern = 'BAD'
                    "#
                    .to_vec(),
                )])),
            },
            ImmediateCpu,
        );
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            reader,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let bad = group(
            RpgMakerSource::data(StandardDataFile::Items),
            1,
            "翻訳<BAD>",
            Some("旧译文<BAD>"),
            Vec::new(),
        );
        let good = group(
            RpgMakerSource::data(StandardDataFile::Items),
            2,
            "正常な翻訳",
            None,
            Vec::new(),
        );

        let failure = expect_unit_preparation_failure(
            planner
                .plan(
                    &project(),
                    &profile(10_000),
                    RpgMakerTranslationCorpus::new(vec![bad, good]),
                    RpgMakerTranslationInput::new(None, Some(placeholder_path)),
                )
                .await,
        );
        let TranslationPlanningFailureReason::PlaceholderProtection {
            failure: TranslationPlaceholderProtectionFailure::OverlappingMatches { first, second },
        } = failure.reason()
        else {
            panic!("应保留类型化的 Placeholder 重叠失败")
        };
        assert_eq!(first.rule().origin(), PlaceholderRuleOrigin::Custom);
        assert_eq!(first.rule().rule_number(), Some(1));
        assert_eq!(first.start_byte(), "翻译".len());
        assert_eq!(first.end_byte(), "翻译<BAD>".len());
        assert_eq!(second.rule().origin(), PlaceholderRuleOrigin::Custom);
        assert_eq!(second.rule().rule_number(), Some(2));
        assert_eq!(second.start_byte(), "翻译<".len());
        assert_eq!(second.end_byte(), "翻译<BAD".len());
        assert_eq!(
            failure.identity().group_location().source(),
            &RpgMakerSource::data(StandardDataFile::Items)
        );
        assert_eq!(
            failure.identity().role(),
            &TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法"))
        );
    }

    #[tokio::test]
    async fn line_crossing_placeholder_failure_aborts_other_scope_blocks_too() {
        let placeholder_path = PathBuf::from("C:/input/placeholders.toml");
        let reader = TranslationPlanningResourceReadingService::new(
            MemoryFileReader {
                files: Arc::new(BTreeMap::from([(
                    placeholder_path.clone(),
                    br#"
                        [[rule]]
                        scopes = ["event_choices"]
                        pattern = '(?s)<opaque>.*?</opaque>'
                    "#
                    .to_vec(),
                )])),
            },
            ImmediateCpu,
        );
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            reader,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let bad = map_unit_group(
            TextGroupKind::EventChoices,
            0,
            vec![(
                TextUnitRole::Choices,
                TextUnitContent::Lines(vec![
                    "翻訳<opaque>前半".to_owned(),
                    "後半</opaque>続き".to_owned(),
                ]),
                "{}",
            )],
        );
        let good = group(
            RpgMakerSource::data(StandardDataFile::Items),
            1,
            "正常な翻訳",
            None,
            Vec::new(),
        );

        let failure = expect_unit_preparation_failure(
            planner
                .plan(
                    &project(),
                    &profile(10_000),
                    RpgMakerTranslationCorpus::new(vec![bad, good]),
                    RpgMakerTranslationInput::new(None, Some(placeholder_path)),
                )
                .await,
        );
        let TranslationPlanningFailureReason::PlaceholderProtection {
            failure:
                TranslationPlaceholderProtectionFailure::CrossesLineBoundary {
                    matched,
                    source_line_index,
                },
        } = failure.reason()
        else {
            panic!("应保留类型化的跨行 Placeholder 失败")
        };
        assert_eq!(matched.rule().origin(), PlaceholderRuleOrigin::Custom);
        assert_eq!(matched.rule().rule_number(), Some(1));
        assert_eq!(*source_line_index, 0);
        assert!(matched.start_byte() < matched.end_byte());
    }

    #[tokio::test]
    async fn failed_block_aborts_before_healthy_duplicate_can_receive_model_responsibility() {
        let placeholder_path = PathBuf::from("C:/input/placeholders.toml");
        let reader = TranslationPlanningResourceReadingService::new(
            MemoryFileReader {
                files: Arc::new(BTreeMap::from([(
                    placeholder_path.clone(),
                    br#"
                        [[rule]]
                        pattern = '<BAD>'

                        [[rule]]
                        pattern = 'BAD'
                    "#
                    .to_vec(),
                )])),
            },
            ImmediateCpu,
        );
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            reader,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let duplicate = "重複テキスト";
        let bad = group(RpgMakerSource::map(1), 1, "翻訳<BAD>", None, Vec::new());
        let blocked_duplicate = group(RpgMakerSource::map(1), 2, duplicate, None, Vec::new());
        let healthy_duplicate = group(RpgMakerSource::map(2), 1, duplicate, None, Vec::new());

        let failure = expect_unit_preparation_failure(
            planner
                .plan(
                    &project(),
                    &profile(10_000),
                    RpgMakerTranslationCorpus::new(vec![bad, blocked_duplicate, healthy_duplicate]),
                    RpgMakerTranslationInput::new(None, Some(placeholder_path)),
                )
                .await,
        );
        assert_eq!(
            failure.identity().group_location().source(),
            &RpgMakerSource::map(1)
        );
    }

    #[tokio::test]
    async fn different_existing_translations_are_retained_without_model_work() {
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let corpus = RpgMakerTranslationCorpus::new(vec![
            group(
                RpgMakerSource::data(StandardDataFile::Items),
                1,
                "保存",
                Some("Save"),
                Vec::new(),
            ),
            group(
                RpgMakerSource::data(StandardDataFile::Items),
                2,
                "保存",
                Some("Store"),
                Vec::new(),
            ),
        ]);

        let (_, preparation, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                corpus,
                RpgMakerTranslationInput::new(None, None),
            )
            .await
            .expect("同族不同 Current 必须并存")
            .into_parts();

        assert_eq!(preparation.retained(), 2);
        assert_eq!(preparation.reused(), 0);
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn user_message_target_splits_only_between_groups_inside_the_same_scope() {
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let first = group(
            RpgMakerSource::data(StandardDataFile::Items),
            1,
            "あ".repeat(120),
            None,
            Vec::new(),
        );
        let second = group(
            RpgMakerSource::data(StandardDataFile::Items),
            2,
            "い".repeat(120),
            None,
            Vec::new(),
        );
        let (_, _, single) = planner
            .plan(
                &project(),
                &profile(10_000),
                RpgMakerTranslationCorpus::new(vec![first.clone()]),
                RpgMakerTranslationInput::new(None, None),
            )
            .await
            .expect("单组应该规划成功")
            .into_parts();
        let exact_single_user_message_characters = user_message(&single[0]).chars().count();

        let (_, _, split) = planner
            .plan(
                &project(),
                &profile(exact_single_user_message_characters),
                RpgMakerTranslationCorpus::new(vec![first, second]),
                RpgMakerTranslationInput::new(None, None),
            )
            .await
            .expect("同一范围应在复合组边界切块")
            .into_parts();

        assert_eq!(split.len(), 2);
        assert_eq!(split[0].expected_outputs()[0].id(), task_id(0));
        assert_eq!(split[1].expected_outputs()[0].id(), task_id(0));
    }

    #[tokio::test]
    async fn system_prompt_is_independent_from_exact_user_message_target() {
        let system_markdown = format!("# System\n{}", "固定规则。".repeat(200));
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources_for_with_system("ja", "zh-Hans", system_markdown.clone()),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let seed = group(
            RpgMakerSource::data(StandardDataFile::Items),
            1,
            "あ".repeat(120),
            None,
            Vec::new(),
        );
        let (_, _, planned) = planner
            .plan(
                &project(),
                &profile(10_000),
                RpgMakerTranslationCorpus::new(vec![seed.clone()]),
                RpgMakerTranslationInput::new(None, None),
            )
            .await
            .expect("宽松 user message 目标应该规划成功")
            .into_parts();
        let exact_user_message_characters = user_message(&planned[0]).chars().count();
        assert!(system_markdown.chars().count() > exact_user_message_characters);

        let (_, _, bounded) = planner
            .plan(
                &project(),
                &profile(exact_user_message_characters),
                RpgMakerTranslationCorpus::new(vec![seed.clone()]),
                RpgMakerTranslationInput::new(None, None),
            )
            .await
            .expect("system Prompt 不应占用 user message 目标")
            .into_parts();
        assert_eq!(bounded.len(), 1);
        assert_eq!(
            user_message(&bounded[0]).chars().count(),
            exact_user_message_characters
        );

        let (_, _, oversized) = planner
            .plan(
                &project(),
                &profile(exact_user_message_characters - 1),
                RpgMakerTranslationCorpus::new(vec![seed]),
                RpgMakerTranslationInput::new(None, None),
            )
            .await
            .expect("单组最终 user message 超过目标时仍应完整规划")
            .into_parts();
        assert_eq!(oversized.len(), 1);
        assert_eq!(
            user_message(&oversized[0]).chars().count(),
            exact_user_message_characters
        );
    }

    #[tokio::test]
    async fn oversized_group_is_isolated_and_later_groups_return_to_target() {
        const TARGET_USER_MESSAGE_CHARACTERS: usize = 2_048;
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let first_original = "あ".repeat(1_100);
        let oversized_original = "い".repeat(4_330);
        let third_original = "う".repeat(1_100);
        let fourth_original = "え".repeat(1_100);
        let first = group(
            RpgMakerSource::data(StandardDataFile::Items),
            1,
            first_original.clone(),
            None,
            Vec::new(),
        );
        let oversized = group(
            RpgMakerSource::data(StandardDataFile::Items),
            2,
            oversized_original.clone(),
            None,
            Vec::new(),
        );
        let third = group(
            RpgMakerSource::data(StandardDataFile::Items),
            3,
            third_original.clone(),
            None,
            Vec::new(),
        );
        let fourth = group(
            RpgMakerSource::data(StandardDataFile::Items),
            4,
            fourth_original.clone(),
            None,
            Vec::new(),
        );
        let (_, _, planned) = planner
            .plan(
                &project(),
                &profile(TARGET_USER_MESSAGE_CHARACTERS),
                RpgMakerTranslationCorpus::new(vec![first, oversized, third, fourth]),
                RpgMakerTranslationInput::new(None, None),
            )
            .await
            .expect("超目标组应该独立成块，不得阻断规划")
            .into_parts();

        assert_eq!(planned.len(), 4);
        let user_messages = planned.iter().map(user_message).collect::<Vec<_>>();
        assert!(user_messages[0].chars().count() <= TARGET_USER_MESSAGE_CHARACTERS);
        assert!(user_messages[1].chars().count() > TARGET_USER_MESSAGE_CHARACTERS);
        assert!(user_messages[2].chars().count() <= TARGET_USER_MESSAGE_CHARACTERS);
        assert!(user_messages[3].chars().count() <= TARGET_USER_MESSAGE_CHARACTERS);
        let expected = planned
            .iter()
            .map(|task| {
                let outputs = task.expected_outputs();
                assert_eq!(outputs.len(), 1);
                assert_eq!(outputs[0].id(), task_id(0));
                outputs[0].protected_text()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            expected,
            [
                first_original.as_str(),
                oversized_original.as_str(),
                third_original.as_str(),
                fourth_original.as_str(),
            ]
        );
    }

    #[test]
    fn sparse_terminology_prompt_visits_only_matches_and_preserves_natural_order() {
        let entries = (0..4_096)
            .map(|index| {
                crate::translation::planning_resource::TerminologyEntry::new(
                    format!("术语-{index:04}-末"),
                    format!("译文-{index:04}-末"),
                    vec![format!("触发-{index:04}-末")],
                )
            })
            .collect();
        let terminology = crate::translation::planning_resource::compile_terminology(entries)
            .expect("术语索引应建立");
        let prompt = TerminologyPromptIndex::new(&terminology);
        let selected = [4_095, 1, 2_048].into_iter().collect::<BTreeSet<_>>();

        let rendered = render_user_message(&[], &prompt, &selected);
        let value = parse_user_message_json(&rendered);
        let terms = value["terminology"]
            .as_array()
            .expect("命中的术语必须形成数组");
        assert_eq!(terms.len(), 3, "只应渲染实际命中的术语");
        assert_eq!(terms[0]["source"], "术语-0001-末");
        assert_eq!(terms[1]["source"], "术语-2048-末");
        assert_eq!(terms[2]["source"], "术语-4095-末");
        assert_eq!(terms[2]["translation"], "译文-4095-末");
    }

    #[tokio::test]
    async fn translated_or_non_source_assets_are_context_only_and_do_not_create_empty_tasks() {
        let planner = RpgMakerTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let corpus = RpgMakerTranslationCorpus::new(vec![
            group(
                RpgMakerSource::data(StandardDataFile::Items),
                1,
                "翻訳済み",
                Some("已翻译"),
                Vec::new(),
            ),
            group(
                RpgMakerSource::data(StandardDataFile::Items),
                2,
                "12345",
                Some("12345"),
                Vec::new(),
            ),
        ]);

        let (_, preparation, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                corpus,
                RpgMakerTranslationInput::new(None, None),
            )
            .await
            .expect("纯上下文语料应该形成空计划")
            .into_parts();
        assert!(tasks.is_empty());
        assert_eq!(preparation.not_applicable(), 1);
        assert_eq!(preparation.invalidated(), 0);
        assert_eq!(preparation.invalidations().len(), 1);
    }
}
