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
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, RecoveryFact, SafeDiagnostic, SafeDiagnosticSource,
};
use crate::execution::CooperativeCancellation;
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::execution::isolated::{IsolatedOperationError, run_isolated_operation};
use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::json_diagnostic::JsonErrorCategory;
use crate::language::{LanguageAnalysis, LanguageOperationCancelled, LanguagePair};
use crate::llm::{ChatMessage, ChatMessageRole, LlmClientConcurrency, LlmClientSemanticIdentity};
use crate::rpg_maker::RpgMakerEngine;
use crate::rpg_maker::location_codec::{RpgMakerLocationCodec, RpgMakerProjectionCodec};
use crate::rpg_maker::model::{TextUnitContent, TextUnitRole};
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::semantic_order::{RpgMakerSemanticOrderKey, RpgMakerSemanticScopeKey};
use crate::rpg_maker::text::{RpgMakerSource, StandardDataFile, TextGroupKind};
use crate::storage::file_system::ReadFileError;
use crate::translation::placeholder_projection::LanguageTextProjectionError;
use crate::translation::task_planning::{
    AssignedTaskBlock, StableGroupCharacters, TaskId, TaskPlanningError, TaskPlanningGroupLayout,
    TaskPlanningLayout, TaskPlanningScopeLayout, UnitTaskResponsibility, assign_task_ids,
    pack_complete_task_blocks,
};

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
    TerminologyDependency, TranslationInvalidation, TranslationPlanPreparation,
    TranslationPlanPreparationCounts, TranslationPlanningFailure, TranslationPlanningFailureReason,
    TranslationPropagationTarget, TranslationStateContext, TranslationUnitIdentity,
    TranslationVirtualReason,
};
use super::placeholder::{
    Pcre2PlaceholderService, PlaceholderProtectionError, PlaceholderRuleCompilationError,
};
use super::profile::{ResolvedRpgMakerTranslationResources, RpgMakerTranslationProfile};
#[cfg(test)]
use super::semantics::manual_translation_state_fingerprint;
use super::semantics::{
    GroupContextFingerprintError, ManualTranslationStateError, PreparedTranslationStatus,
    ResolvedTranslationSemanticError, ResolvedTranslationSemantics,
    group_context_fingerprint_with_cancellation as shared_group_context_fingerprint_with_cancellation,
    manual_translation_state_fingerprint_with_cancellation,
};
use crate::translation::planning_resource::{
    CompiledTerminology, PlaceholderDefinitionError, TerminologyDefinitionError,
    TranslationPlanningResourceReader, TranslationPlanningResourceReadingError,
};

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
                    source_language_id.as_str(),
                    target_language_id.as_str(),
                    language_semantics,
                    &system_markdown,
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
                result.map_err(RpgMakerTranslationTaskPlanningError::InvalidPlaceholderRules)?
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
        })
    }
}

impl<R, C, L> RpgMakerTranslationTaskPlanner for RpgMakerTranslationTaskPlanningService<R, C, L>
where
    R: TranslationPlanningResourceReader,
    C: CpuTaskExecutor,
    L: LlmClientConcurrency + LlmClientSemanticIdentity + 'static,
{
    type Profile = Arc<RpgMakerTranslationProfile<L>>;
    type Error = RpgMakerTranslationTaskPlanningError<R::Error, C::Error>;

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
        let (scopes, invalidations, reuses, planning_failures, preparation_counts, complete_plan) =
            self.cpu
                .execute(move || {
                    match run_isolated_operation(
                        "att-rpg-maker-deduplication",
                        move || {
                            let mut scopes = Vec::with_capacity(preprocessed_scopes.len());
                            let mut planning_failures = Vec::new();
                            let mut preprocessing_invalidations = Vec::new();
                            let mut preprocessing_invalidated = 0;
                            for (scope, result) in preprocessed_scopes {
                                let result = result.map_err(|source| match source {
                                    ScopePreprocessingFailure::Cancelled => {
                                        GlobalPreparationFailure::Cancelled
                                    }
                                    ScopePreprocessingFailure::Invalid(source) => {
                                        GlobalPreparationFailure::InvalidScopePreprocessing {
                                            scope,
                                            source,
                                        }
                                    }
                                })?;
                                planning_failures.extend(result.planning_failures);
                                preprocessing_invalidations.extend(result.invalidations);
                                preprocessing_invalidated += result.invalidated;
                                scopes.push(result.scope);
                            }
                            let (candidates, positions, mut invalidations) =
                                collect_deduplication_inputs(&scopes, &complete_plan);
                            invalidations.extend(preprocessing_invalidations);
                            let deduplicated = deduplicate_translation_candidates(candidates);
                            let (outcomes, deduplication_invalidations, reuses) =
                                deduplicated.into_parts();
                            invalidations.extend(deduplication_invalidations);
                            apply_deduplication_outcomes(&mut scopes, positions, outcomes);

                            let retained = scopes
                                .iter()
                                .flat_map(|scope| &scope.groups)
                                .flat_map(|group| &group.units)
                                .filter_map(Option::as_ref)
                                .filter(|unit| unit.current)
                                .count();
                            let not_applicable = scopes
                                .iter()
                                .flat_map(|scope| &scope.groups)
                                .flat_map(|group| &group.units)
                                .filter_map(Option::as_ref)
                                .filter(|unit| unit.not_applicable)
                                .count();
                            let invalidated = scopes
                                .iter()
                                .flat_map(|scope| &scope.groups)
                                .flat_map(|group| &group.units)
                                .filter_map(Option::as_ref)
                                .filter(|unit| unit.invalidated && !unit.not_applicable)
                                .count()
                                + preprocessing_invalidated;

                            Ok::<_, GlobalPreparationFailure>((
                                scopes,
                                invalidations,
                                reuses,
                                planning_failures,
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
                    if let Some(task) = materialized? {
                        let index = RpgMakerTranslationTaskIndex::new(tasks.len());
                        tasks.push(task.with_index(index, task_language_pair.clone()));
                    }
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
            TranslationPlanPreparation::with_baseline_and_planning_failures(
                invalidations,
                reuses,
                terminology_json,
                placeholder_rules_json,
                preparation_counts,
                snapshot_baseline,
                planning_failures,
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
                let (identity, semantic_order_key, translation, translation_state) =
                    asset.into_parts();
                assets.push(PreparedAsset {
                    identity,
                    semantic_order_key,
                    translation,
                    translation_state,
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
            let mut markdown = format!("## {}\n", human_group_kind(group.kind));
            for asset in &group.assets {
                ensure_planner_cpu_running(cancellation)
                    .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
                markdown.push('\n');
                let source_text = task_content_text_with_cancellation(
                    asset.identity.source_content(),
                    cancellation,
                )?;
                render_task_unit_with_cancellation(
                    &mut markdown,
                    &asset.identity,
                    asset.identity.role_label().as_str(),
                    None,
                    &source_text,
                    cancellation,
                )?;
            }
            let first_in_block =
                planner_character_count_with_cancellation(&markdown, cancellation)?;
            let following_in_block =
                first_in_block
                    .checked_add(1)
                    .ok_or(ScopeTaskPlanningFailure::TaskPlanning(
                        TaskPlanningError::CharacterCountOverflow,
                    ))?;
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
    planning_failures: Vec<TranslationPlanningFailure>,
    invalidations: Vec<TranslationInvalidation>,
    invalidated: usize,
}

struct PreprocessedGroup {
    kind: TextGroupKind,
    /// 完整 Group 内所有 Unit 实际命中的术语文件索引。
    triggered_terms: Vec<usize>,
    /// `None` 保留无法完成源文保护或语言投影的原始 Unit 槽位。
    /// 完整装箱仍按 Extract 的 Unit 数量进行；最终包含该槽位的块不会发送。
    units: Vec<Option<PreprocessedUnit>>,
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
    let mut planning_failures = Vec::new();
    let mut invalidations = Vec::new();
    let mut invalidated = 0;
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
                    if let Some(translation) = asset.translation {
                        invalidations.push(TranslationInvalidation::new(
                            asset.identity.clone(),
                            translation,
                            asset
                                .translation_state
                                .expect("已有译文必须同时具有 translation_state"),
                        ));
                        invalidated += 1;
                    }
                    planning_failures.push(TranslationPlanningFailure::new(asset.identity, reason));
                    prepared_assets.push(None);
                    continue;
                }
                Err(()) => return Err(ScopePreprocessingFailure::Cancelled),
            };
            prepared_assets.push(Some((asset, prepared)));
        }

        // 术语属于完整 Group：任一 Unit 命中的术语都必须进入该 Group 的全部自动状态，
        // 并且按术语文件中的自然顺序只保留一次。
        let mut group_terminology = BTreeMap::<usize, TerminologyDependency>::new();
        for (_, prepared) in prepared_assets.iter().flatten() {
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
        for prepared_asset in prepared_assets {
            let Some((asset, prepared)) = prepared_asset else {
                units.push(None);
                continue;
            };
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
            let manual_state = manual_translation_state_fingerprint_with_cancellation(
                semantics.engine(),
                semantics.language_pair(),
                group_context,
                &asset.identity,
                &placeholders,
                || ensure_planner_cpu_running(cancellation),
            )
            .map_err(|()| ScopePreprocessingFailure::Cancelled)?
            .map_err(|source| {
                ScopePreprocessingFailure::Invalid(match source {
                    ManualTranslationStateError::EncodeLocation(source) => {
                        ScopePreprocessingError::StateLocation(source)
                    }
                    ManualTranslationStateError::EncodeRole(source) => {
                        ScopePreprocessingError::StateRole(source)
                    }
                })
            })?;
            let not_applicable = prepared.status() != PreparedTranslationStatus::Active;
            let current = asset.translation.as_ref().is_some_and(|translation| {
                asset.translation_state == Some(manual_state)
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
            units.push(Some(PreprocessedUnit {
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
            }));
        }
        groups.push(PreprocessedGroup {
            kind: group.kind,
            triggered_terms,
            units,
        });
    }
    Ok(PreprocessedScopeResult {
        scope: PreprocessedScope { groups },
        planning_failures,
        invalidations,
        invalidated,
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
                message: placeholder_protection_failure_detail(&source),
            }
        }
        ResolvedTranslationSemanticError::ProjectLanguageText(source) => {
            TranslationPlanningFailureReason::PlaceholderProjection {
                message: placeholder_projection_failure_detail(&source),
            }
        }
        #[cfg(test)]
        ResolvedTranslationSemanticError::AcceptCandidate(_) => {
            unreachable!("译前准备不会执行候选译文验收")
        }
    }
}

fn placeholder_protection_failure_detail(source: &PlaceholderProtectionError) -> String {
    match source {
        PlaceholderProtectionError::StartWorker { operation, source } => format!(
            "placeholder_match_worker_start_failed; operation={operation}; os_code={:?}; reason={source}",
            source.raw_os_error()
        ),
        PlaceholderProtectionError::Match(source) => format!(
            "pcre2_match_failed; kind={}; code={}; offset={:?}",
            pcre2_error_kind(source),
            source.code(),
            source.offset()
        ),
        PlaceholderProtectionError::EmptyMatch { .. } => "placeholder_empty_match".to_owned(),
        PlaceholderProtectionError::MissingTextCapture { rule_number } => {
            format!("placeholder_text_capture_missing; rule={rule_number}")
        }
        PlaceholderProtectionError::InvalidMatchRange { rule_number } => {
            format!("placeholder_match_range_invalid; rule={rule_number}")
        }
        PlaceholderProtectionError::OverlappingMatches { .. } => {
            "placeholder_matches_overlap".to_owned()
        }
        PlaceholderProtectionError::CrossesLineBoundary {
            rule_number,
            source_line_index,
        } => {
            let rule = rule_number.map_or_else(|| "builtin".to_owned(), |value| value.to_string());
            format!(
                "placeholder_crosses_line_boundary; rule={rule}; source_line_index={source_line_index}"
            )
        }
        PlaceholderProtectionError::ReservedTokenNamespace => {
            "source_uses_reserved_token_namespace".to_owned()
        }
    }
}

fn placeholder_projection_failure_detail(source: &LanguageTextProjectionError) -> String {
    match source {
        LanguageTextProjectionError::TokenIndexConstruction => {
            "placeholder_token_index_construction_failed".to_owned()
        }
        LanguageTextProjectionError::EmptyToken => "empty_placeholder_token".to_owned(),
        LanguageTextProjectionError::MissingToken { .. } => {
            "protected_text_missing_placeholder_token".to_owned()
        }
        LanguageTextProjectionError::RepeatedToken { .. } => {
            "protected_text_repeats_placeholder_token".to_owned()
        }
        LanguageTextProjectionError::OverlappingToken { .. } => {
            "placeholder_tokens_overlap".to_owned()
        }
        LanguageTextProjectionError::ChangedTokenOrder { position, .. } => {
            format!("placeholder_token_order_changed; position={position}")
        }
        LanguageTextProjectionError::ChangedSegmentCount { expected, actual } => {
            format!("segment_count_changed; expected={expected}; actual={actual}")
        }
        LanguageTextProjectionError::ChangedSegmentKind { segment_index } => {
            format!("segment_kind_changed; segment={segment_index}")
        }
        LanguageTextProjectionError::MissingOrderedToken { segment_index } => {
            format!("ordered_token_missing; segment={segment_index}")
        }
        LanguageTextProjectionError::UnusedOrderedToken => "ordered_token_unused".to_owned(),
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

fn planner_character_count_with_cancellation(
    text: &str,
    cancellation: &CooperativeCancellation,
) -> Result<usize, ScopeTaskPlanningFailure> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;
    let mut count = 0_usize;
    for chunk in text.as_bytes().chunks(CANCELLATION_CHECK_BYTES) {
        ensure_planner_cpu_running(cancellation)
            .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
        count = count
            .checked_add(
                chunk
                    .iter()
                    .filter(|byte| (**byte & 0b1100_0000) != 0b1000_0000)
                    .count(),
            )
            .ok_or(ScopeTaskPlanningFailure::TaskPlanning(
                TaskPlanningError::CharacterCountOverflow,
            ))?;
    }
    ensure_planner_cpu_running(cancellation).map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
    Ok(count)
}

#[cfg(test)]
pub(crate) fn global_translation_semantics(
    engine: RpgMakerEngine,
    source_language: &str,
    target_language: &str,
    language_semantics: Sha256Fingerprint,
    system_markdown: &str,
    client_semantics: Sha256Fingerprint,
) -> Sha256Fingerprint {
    match global_translation_semantics_with_cancellation(
        engine,
        source_language,
        target_language,
        language_semantics,
        system_markdown,
        client_semantics,
        || Ok::<_, Infallible>(()),
    ) {
        Ok(fingerprint) => fingerprint,
        Err(unreachable) => match unreachable {},
    }
}

pub(crate) fn global_translation_semantics_with_cancellation<E>(
    engine: RpgMakerEngine,
    source_language: &str,
    target_language: &str,
    language_semantics: Sha256Fingerprint,
    system_markdown: &str,
    client_semantics: Sha256Fingerprint,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Sha256Fingerprint, E> {
    let chunk_size = NonZeroUsize::new(64 * 1024).expect("全局翻译语义指纹取消检查块大小必须非零");
    ensure_running()?;
    let mut hasher = Sha256FramedHasher::new(b"att.rpg_maker.translation-global");
    hasher
        .frame(1, source_language.as_bytes())
        .frame(2, target_language.as_bytes())
        .frame(3, language_semantics.as_bytes());
    hasher
        .try_frame_chunks(
            4,
            system_markdown.as_bytes(),
            chunk_size,
            &mut ensure_running,
        )?
        .frame(5, client_semantics.as_bytes())
        .frame(6, engine.storage_name().as_bytes());
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
    let mut model_representative_eligibility = vec![true; complete_plan.total_units()];
    for block in complete_plan.blocks() {
        let scope = scopes
            .get(block.scope_index())
            .expect("共享 Planner 的 Scope 索引必须来自当前完整语料");
        let groups = scope
            .groups
            .get(block.group_range())
            .expect("共享 Planner 的 Group 范围必须来自当前完整语料");
        if groups
            .iter()
            .flat_map(|group| &group.units)
            .any(Option::is_none)
        {
            model_representative_eligibility[block.unit_range()].fill(false);
        }
    }
    let mut candidates = Vec::new();
    let mut positions = Vec::new();
    let mut invalidations = Vec::new();
    let mut global_unit_index = 0_usize;
    for (scope_index, scope) in scopes.iter().enumerate() {
        for (group_index, group) in scope.groups.iter().enumerate() {
            for (unit_index, unit) in group.units.iter().enumerate() {
                let Some(unit) = unit.as_ref() else {
                    global_unit_index += 1;
                    continue;
                };
                if matches!(
                    unit.responsibility,
                    PreparedUnitResponsibility::AwaitingDeduplication
                ) {
                    candidates.push(
                        TranslationDeduplicationCandidate::new(
                            unit.identity.clone(),
                            unit.protected_text.clone(),
                            unit.placeholders.clone(),
                            unit.translation.clone(),
                            unit.translation_state,
                            unit.state_context,
                            unit.invalidated,
                        )
                        .with_model_representative_eligibility(
                            model_representative_eligibility[global_unit_index],
                        ),
                    );
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
        let unit = scopes[scope_index].groups[group_index].units[unit_index]
            .as_mut()
            .expect("去重位置只能指向成功完成译前准备的 Unit");
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
                responsibilities.push(match unit.as_ref().map(|unit| &unit.responsibility) {
                    Some(PreparedUnitResponsibility::Active { .. }) => {
                        UnitTaskResponsibility::ModelRepresentative
                    }
                    Some(PreparedUnitResponsibility::AwaitingDeduplication) => {
                        unreachable!("分配 Task ID 前必须完成全局去重")
                    }
                    Some(PreparedUnitResponsibility::Virtual { .. }) | None => {
                        UnitTaskResponsibility::Context
                    }
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
) -> Result<Option<UnindexedTask>, ScopeTaskPlanningFailure> {
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

    // 原文准备失败的 Unit 仍占据完整块中的原位置。它没有安全模型表示，因此整个块不发送。
    if groups
        .iter()
        .flat_map(|group| &group.units)
        .any(Option::is_none)
    {
        return Ok(None);
    }

    let mut task_ids = block.unit_task_ids().iter().copied();
    let mut selected_terms = BTreeSet::new();
    let mut rendered_groups = Vec::with_capacity(groups.len());
    for group in groups {
        ensure_planner_cpu_running(cancellation)
            .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
        let mut markdown = format!("## {}\n", human_group_kind(group.kind));
        let mut expected = Vec::new();
        for term in &group.triggered_terms {
            selected_terms.insert(*term);
        }
        for unit in &group.units {
            ensure_planner_cpu_running(cancellation)
                .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
            let unit = unit.as_ref().expect("原文准备失败的完整块已在渲染前排除");
            let task_id = task_ids
                .next()
                .expect("AssignedTaskBlock 的 Unit ID 槽必须覆盖完整块");
            let display_text = if task_id.is_some() {
                unit.protected_text.clone()
            } else {
                context_model_text_with_cancellation(unit, semantics, cancellation)?
            };
            markdown.push('\n');
            render_task_unit_with_cancellation(
                &mut markdown,
                &unit.identity,
                unit.identity.role_label().as_str(),
                task_id,
                &display_text,
                cancellation,
            )?;

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
                line_shape: expected_line_shape_with_cancellation(&unit.identity, cancellation)?,
                identity: unit.identity.clone(),
                propagation_targets: propagation_targets.clone(),
                protected_text: unit.protected_text.clone(),
                placeholders: unit.placeholders.clone(),
                language_analysis: unit.language_analysis.clone(),
                state_context: unit.state_context,
            });
        }
        rendered_groups.push(RenderedGroup { markdown, expected });
    }
    assert!(
        task_ids.next().is_none(),
        "AssignedTaskBlock 不得包含超出完整块的 Unit ID 槽"
    );

    let user_markdown = render_user_markdown_with_cancellation(
        &rendered_groups,
        None,
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
    Ok(Some(UnindexedTask {
        messages: vec![
            ChatMessage::new(ChatMessageRole::System, system_markdown),
            ChatMessage::new(ChatMessageRole::User, user_markdown),
        ],
        expected_outputs,
    }))
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
    markdown: String,
    expected: Vec<ExpectedBase>,
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

fn append_task_id_slot(markdown: &mut String, task_id: Option<TaskId>) {
    markdown.push('[');
    match task_id {
        Some(task_id) => markdown.push_str(&task_id.to_string()),
        None => markdown.push('-'),
    }
    markdown.push(']');
}

/// 用同一种角色格式渲染模型代表和无编号语境。
///
/// 稳定装箱传入完整原文和 `None`；最终消息只改变 ID 槽和显示文本，不改变角色格式。
fn render_task_unit_with_cancellation(
    markdown: &mut String,
    identity: &TranslationUnitIdentity,
    field_name: &str,
    task_id: Option<TaskId>,
    text: &str,
    cancellation: &CooperativeCancellation,
) -> Result<(), ScopeTaskPlanningFailure> {
    ensure_planner_cpu_running(cancellation).map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
    match identity.role() {
        TextUnitRole::DialogueSpeaker => {
            markdown.push_str("Speaker ");
            append_task_id_slot(markdown, task_id);
            markdown.push_str(" (single line):");
            append_planner_text_with_cancellation(markdown, text, cancellation)?;
            markdown.push('\n');
        }
        TextUnitRole::DialogueBody => {
            markdown.push_str("Body ");
            append_task_id_slot(markdown, task_id);
            markdown.push_str(" (free line breaking):\n\n");
            push_blockquote_with_cancellation(markdown, text, cancellation)?;
        }
        TextUnitRole::Choices => {
            markdown.push_str("Choices ");
            append_task_id_slot(markdown, task_id);
            markdown.push_str(" (");
            markdown.push_str(&source_line_count(identity).to_string());
            markdown.push_str(" items, corresponding item by item):\n\n");
            push_blockquote_with_cancellation(markdown, text, cancellation)?;
        }
        TextUnitRole::ScrollingText => {
            markdown.push_str("Scrolling Text ");
            append_task_id_slot(markdown, task_id);
            markdown.push_str(" (");
            markdown.push_str(&source_line_count(identity).to_string());
            markdown.push_str(" lines, corresponding line by line):\n\n");
            push_blockquote_with_cancellation(markdown, text, cancellation)?;
        }
        TextUnitRole::Scalar(_)
            if expected_line_shape_with_cancellation(identity, cancellation)?
                == ExpectedLineShape::Reflow =>
        {
            markdown.push_str(human_scalar_label(field_name));
            markdown.push(' ');
            append_task_id_slot(markdown, task_id);
            markdown.push_str(" (free line breaking):\n\n");
            push_blockquote_with_cancellation(markdown, text, cancellation)?;
        }
        TextUnitRole::Scalar(_) => {
            markdown.push_str(human_scalar_label(field_name));
            markdown.push(' ');
            append_task_id_slot(markdown, task_id);
            markdown.push_str(" (single line):");
            append_planner_text_with_cancellation(markdown, text, cancellation)?;
            markdown.push('\n');
        }
    }
    ensure_planner_cpu_running(cancellation).map_err(|()| ScopeTaskPlanningFailure::Cancelled)
}

fn human_scalar_label(field_name: &str) -> &str {
    match field_name {
        "name" => "Name",
        "displayName" => "Map Name",
        "nickname" => "Nickname",
        "profile" => "Profile",
        "description" => "Description",
        _ => field_name,
    }
}

#[cfg(test)]
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

fn push_blockquote_with_cancellation(
    markdown: &mut String,
    text: &str,
    cancellation: &CooperativeCancellation,
) -> Result<(), ScopeTaskPlanningFailure> {
    for line in text.split('\n') {
        ensure_planner_cpu_running(cancellation)
            .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
        markdown.push_str("> ");
        append_planner_text_with_cancellation(markdown, line, cancellation)?;
        markdown.push('\n');
    }
    ensure_planner_cpu_running(cancellation).map_err(|()| ScopeTaskPlanningFailure::Cancelled)
}

/// 一次规划运行共享的术语提示词索引。
///
/// `lines` 严格沿用术语文件自然顺序。每一行只做一次 Markdown 转义，Scope 和 Task
/// 随后只根据已经由 Aho-Corasick 得到的命中下标访问实际需要的行。
struct TerminologyPromptIndex {
    lines: Vec<TerminologyPromptLine>,
}

struct TerminologyPromptLine {
    markdown: String,
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
            let mut markdown = String::new();
            markdown.push_str("- ");
            push_markdown_literal_with_cancellation(&mut markdown, entry.term(), cancellation)?;
            markdown.push_str(" → ");
            push_markdown_literal_with_cancellation(
                &mut markdown,
                entry.translation(),
                cancellation,
            )?;
            markdown.push('\n');
            lines.push(TerminologyPromptLine { markdown });
        }
        ensure_planner_cpu_running(cancellation)
            .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
        Ok(Self { lines })
    }

    /// 追加自然有序的稀疏命中，并返回实际访问的术语行数供复杂度测试观察。
    #[cfg(test)]
    fn append_selected(&self, markdown: &mut String, selected: &BTreeSet<usize>) -> usize {
        match self.append_selected_with_cancellation(
            markdown,
            selected,
            &CooperativeCancellation::default(),
        ) {
            Ok(visited) => visited,
            Err(ScopeTaskPlanningFailure::Cancelled) => {
                unreachable!("未请求取消的术语渲染不能取消")
            }
            Err(ScopeTaskPlanningFailure::TaskPlanning(_)) => {
                unreachable!("术语渲染不执行 TaskBlock 规划")
            }
            Err(ScopeTaskPlanningFailure::InvalidContract(_)) => {
                unreachable!("术语渲染不验证输出契约")
            }
        }
    }

    fn append_selected_with_cancellation(
        &self,
        markdown: &mut String,
        selected: &BTreeSet<usize>,
        cancellation: &CooperativeCancellation,
    ) -> Result<usize, ScopeTaskPlanningFailure> {
        for &index in selected {
            ensure_planner_cpu_running(cancellation)
                .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
            append_planner_text_with_cancellation(
                markdown,
                &self.lines[index].markdown,
                cancellation,
            )?;
        }
        Ok(selected.len())
    }
}

#[cfg(test)]
fn render_user_markdown(
    groups: &[RenderedGroup],
    additional_group: Option<&RenderedGroup>,
    terminology: &TerminologyPromptIndex,
    selected_terms: &BTreeSet<usize>,
) -> String {
    match render_user_markdown_with_cancellation(
        groups,
        additional_group,
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

fn render_user_markdown_with_cancellation(
    groups: &[RenderedGroup],
    additional_group: Option<&RenderedGroup>,
    terminology: &TerminologyPromptIndex,
    selected_terms: &BTreeSet<usize>,
    cancellation: &CooperativeCancellation,
) -> Result<String, ScopeTaskPlanningFailure> {
    ensure_planner_cpu_running(cancellation).map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
    let mut markdown = String::new();
    if !selected_terms.is_empty() {
        markdown.push_str("Terminology:\n\n");
        terminology.append_selected_with_cancellation(
            &mut markdown,
            selected_terms,
            cancellation,
        )?;
    }
    for group in groups {
        ensure_planner_cpu_running(cancellation)
            .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
        if !markdown.is_empty() {
            markdown.push('\n');
        }
        append_planner_text_with_cancellation(&mut markdown, &group.markdown, cancellation)?;
    }
    if let Some(group) = additional_group {
        ensure_planner_cpu_running(cancellation)
            .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
        if !markdown.is_empty() {
            markdown.push('\n');
        }
        append_planner_text_with_cancellation(&mut markdown, &group.markdown, cancellation)?;
    }
    Ok(markdown)
}

#[cfg(test)]
fn push_markdown_literal(markdown: &mut String, value: &str) {
    match push_markdown_literal_with_cancellation(
        markdown,
        value,
        &CooperativeCancellation::default(),
    ) {
        Ok(()) => {}
        Err(ScopeTaskPlanningFailure::Cancelled) => {
            unreachable!("未请求取消的 Markdown 转义不能取消")
        }
        Err(ScopeTaskPlanningFailure::TaskPlanning(_)) => {
            unreachable!("Markdown 转义不执行 TaskBlock 规划")
        }
        Err(ScopeTaskPlanningFailure::InvalidContract(_)) => {
            unreachable!("Markdown 转义不验证输出契约")
        }
    }
}

fn push_markdown_literal_with_cancellation(
    markdown: &mut String,
    value: &str,
    cancellation: &CooperativeCancellation,
) -> Result<(), ScopeTaskPlanningFailure> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;
    let mut next_check = 0_usize;
    for (offset, character) in value.char_indices() {
        if offset >= next_check {
            ensure_planner_cpu_running(cancellation)
                .map_err(|()| ScopeTaskPlanningFailure::Cancelled)?;
            next_check = offset.saturating_add(CANCELLATION_CHECK_BYTES);
        }
        if character.is_ascii_punctuation() {
            markdown.push('\\');
        }
        markdown.push(character);
    }
    ensure_planner_cpu_running(cancellation).map_err(|()| ScopeTaskPlanningFailure::Cancelled)
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

const fn human_group_kind(kind: TextGroupKind) -> &'static str {
    match kind {
        TextGroupKind::DatabaseEntry => "Database Text",
        TextGroupKind::System => "System Text",
        TextGroupKind::Map => "Map Text",
        TextGroupKind::EventDialogue => "Dialogue",
        TextGroupKind::EventChoices => "Choices",
        TextGroupKind::EventScrollingText => "Scrolling Text",
        TextGroupKind::EventCommand => "Event Command",
        TextGroupKind::PluginParameter => "Plugin Parameters",
    }
}

const fn group_kind_name(kind: TextGroupKind) -> &'static [u8] {
    kind.storage_name().as_bytes()
}

enum GlobalPreparationFailure {
    Cancelled,
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
    InvalidPlaceholderRules(PlaceholderRuleCompilationError),
    PrepareCorpusCompute(CpuTaskExecutionError<C>),
    InvalidCorpus(CorpusPlanningError),
    PreprocessScopesCompute(CpuTaskExecutionError<C>),
    InvalidScopePreprocessing {
        scope: RpgMakerSemanticScopeKey,
        source: ScopePreprocessingError,
    },
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
            Self::InvalidPlaceholderRules(source) => write!(formatter, "占位符规则无效：{source}"),
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
            Self::InvalidPlaceholderRules(source) => Some(source),
            Self::PrepareCorpusCompute(source) => Some(source),
            Self::InvalidCorpus(source) => Some(source),
            Self::PreprocessScopesCompute(source) => Some(source),
            Self::InvalidScopePreprocessing { source, .. } => Some(source),
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

impl<R, C> SafeDiagnosticSource for RpgMakerTranslationTaskPlanningError<R, C>
where
    R: SafeDiagnosticSource,
    CpuTaskExecutionError<C>: SafeDiagnosticSource,
{
    fn safe_diagnostic_source(
        &self,
        stage: DiagnosticStage,
        impact: DiagnosticImpact,
        _fallback_action: DiagnosticAction,
    ) -> SafeDiagnostic {
        match self {
            Self::ResolvedLanguagePairMismatch {
                project_source,
                project_target,
                resolved_source,
                resolved_target,
            } => SafeDiagnostic::new(
                DiagnosticCode::ConfigurationInvalidValue,
                stage,
                DiagnosticSubject::field("rpg_maker.translation.language_pair"),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::ConflictingValues,
                    format!(
                        "project={project_source}->{project_target}; resolved={resolved_source}->{resolved_target}"
                    ),
                ),
                impact,
                DiagnosticAction::FixConfiguration,
            ),
            Self::ReadResources(source) => {
                source.safe_diagnostic_source(stage, impact, DiagnosticAction::FixInput)
            }
            Self::PrepareResourcesCompute(source) => planning_cpu_diagnostic(
                source,
                stage,
                impact,
                "prepare_translation_planning_context",
                None,
            ),
            Self::CompilePlaceholdersCompute(source) => {
                planning_cpu_diagnostic(source, stage, impact, "compile_placeholder_rules", None)
            }
            Self::InvalidPlaceholderRules(source) => {
                placeholder_compilation_diagnostic(source, stage, impact)
            }
            Self::PrepareCorpusCompute(source) => {
                planning_cpu_diagnostic(source, stage, impact, "prepare_translation_corpus", None)
            }
            Self::InvalidCorpus(source) => SafeDiagnostic::new(
                DiagnosticCode::ProjectState,
                stage,
                DiagnosticSubject::operation("rpg_maker_translation_corpus"),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::StateMismatch,
                    source.to_string(),
                ),
                impact,
                DiagnosticAction::CheckProjectState,
            ),
            Self::PreprocessScopesCompute(source) => planning_cpu_diagnostic(
                source,
                stage,
                impact,
                "preprocess_translation_scopes",
                None,
            ),
            Self::InvalidScopePreprocessing { scope, source } => SafeDiagnostic::new(
                DiagnosticCode::InternalOperation,
                stage,
                DiagnosticSubject::operation("translation_scope_preprocessing"),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::InternalInvariant,
                    scope_preprocessing_detail(source),
                ),
                impact,
                DiagnosticAction::ReportBug,
            )
            .with_recovery(RecoveryFact::component(format!(
                "scope={}",
                safe_scope_label(scope)
            ))),
            Self::DeduplicateCompute(source) => planning_cpu_diagnostic(
                source,
                stage,
                impact,
                "deduplicate_translation_corpus",
                None,
            ),
            Self::StartDeduplicationWorker { operation, source } => SafeDiagnostic::io(
                DiagnosticCode::InternalOperation,
                stage,
                DiagnosticSubject::operation("deduplicate_translation_corpus"),
                "spawn_worker",
                source,
                impact,
                DiagnosticAction::Retry,
            )
            .with_recovery(RecoveryFact::component(*operation)),
            Self::PlanScopesCompute(source) => {
                planning_cpu_diagnostic(source, stage, impact, "plan_translation_scopes", None)
            }
            Self::FinalizePlanCompute(source) => planning_cpu_diagnostic(
                source,
                stage,
                impact,
                "finalize_translation_task_order",
                None,
            ),
            Self::TaskPlanning(source) => SafeDiagnostic::new(
                if source.is_cancelled() {
                    DiagnosticCode::CommandInput
                } else {
                    DiagnosticCode::InternalOperation
                },
                stage,
                DiagnosticSubject::operation("task_block_planning"),
                DiagnosticReason::failure_with_detail(
                    if source.is_cancelled() {
                        DiagnosticFailureKind::LockCancelled
                    } else {
                        DiagnosticFailureKind::InternalInvariant
                    },
                    source.to_string(),
                ),
                impact,
                if source.is_cancelled() {
                    DiagnosticAction::Retry
                } else {
                    DiagnosticAction::ReportBug
                },
            ),
            Self::InvalidOutputContract(source) => SafeDiagnostic::new(
                DiagnosticCode::InternalOperation,
                stage,
                source.diagnostic_subject(),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::InternalInvariant,
                    source.safe_detail(),
                ),
                impact,
                DiagnosticAction::ReportBug,
            ),
        }
    }
}

impl<F, C> SafeDiagnosticSource for TranslationPlanningResourceReadingError<F, C>
where
    F: SafeDiagnosticSource,
    CpuTaskExecutionError<C>: SafeDiagnosticSource,
{
    fn safe_diagnostic_source(
        &self,
        stage: DiagnosticStage,
        impact: DiagnosticImpact,
        _fallback_action: DiagnosticAction,
    ) -> SafeDiagnostic {
        match self {
            Self::Cancelled => SafeDiagnostic::new(
                DiagnosticCode::CommandInput,
                stage,
                DiagnosticSubject::operation("translation_planning_resources"),
                DiagnosticReason::failure(DiagnosticFailureKind::LockCancelled),
                impact,
                DiagnosticAction::Retry,
            ),
            Self::ReadTerminology { source, .. } => {
                read_planning_resource_diagnostic(source, stage, impact, "terminology")
            }
            Self::ReadPlaceholderRules { source, .. } => {
                read_planning_resource_diagnostic(source, stage, impact, "placeholder_rules")
            }
            Self::ParseTerminologyCompute { path, source } => planning_resource_cpu_diagnostic(
                source,
                path.as_deref(),
                stage,
                impact,
                "parse_terminology",
            ),
            Self::InvalidTerminology { path, source } => {
                terminology_definition_diagnostic(path.as_deref(), source, stage, impact)
            }
            Self::ParsePlaceholderRulesCompute { path, source } => {
                planning_resource_cpu_diagnostic(
                    source,
                    path.as_deref(),
                    stage,
                    impact,
                    "parse_placeholder_rules",
                )
            }
            Self::InvalidPlaceholderRules { path, source } => {
                placeholder_definition_diagnostic(path.as_deref(), source, stage, impact)
            }
        }
    }
}

fn planning_cpu_diagnostic<C>(
    source: &CpuTaskExecutionError<C>,
    stage: DiagnosticStage,
    impact: DiagnosticImpact,
    operation: &'static str,
    scope: Option<&RpgMakerSemanticScopeKey>,
) -> SafeDiagnostic
where
    CpuTaskExecutionError<C>: SafeDiagnosticSource,
{
    let mut diagnostic = source
        .safe_diagnostic_source(stage, impact, DiagnosticAction::Retry)
        .with_recovery(RecoveryFact::component(operation));
    if let Some(scope) = scope {
        diagnostic = diagnostic.with_recovery(RecoveryFact::component(format!(
            "scope={}",
            safe_scope_label(scope)
        )));
    }
    diagnostic
}

fn planning_resource_cpu_diagnostic<C>(
    source: &CpuTaskExecutionError<C>,
    path: Option<&std::path::Path>,
    stage: DiagnosticStage,
    impact: DiagnosticImpact,
    operation: &'static str,
) -> SafeDiagnostic
where
    CpuTaskExecutionError<C>: SafeDiagnosticSource,
{
    let mut diagnostic = planning_cpu_diagnostic(source, stage, impact, operation, None);
    if let Some(path) = path {
        diagnostic.subject = DiagnosticSubject::path(path);
    } else {
        diagnostic = diagnostic.with_recovery(RecoveryFact::component("project_resource_snapshot"));
    }
    diagnostic
}

fn read_planning_resource_diagnostic<F>(
    source: &ReadFileError<F>,
    stage: DiagnosticStage,
    impact: DiagnosticImpact,
    resource: &'static str,
) -> SafeDiagnostic
where
    F: SafeDiagnosticSource,
{
    match source {
        ReadFileError::NotFound { path } => SafeDiagnostic::new(
            DiagnosticCode::CommandInput,
            stage,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure(DiagnosticFailureKind::NotFound),
            impact,
            DiagnosticAction::CheckPathAndPermissions,
        )
        .with_recovery(RecoveryFact::component(resource)),
        ReadFileError::NotFile { path } => SafeDiagnostic::new(
            DiagnosticCode::CommandInput,
            stage,
            DiagnosticSubject::path(path),
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::InvalidValue,
                "path_is_not_a_regular_file",
            ),
            impact,
            DiagnosticAction::CheckPathAndPermissions,
        )
        .with_recovery(RecoveryFact::component(resource)),
        ReadFileError::Io { path, source } => {
            let mut diagnostic = source.safe_diagnostic_source(
                stage,
                impact,
                DiagnosticAction::CheckPathAndPermissions,
            );
            diagnostic.subject = DiagnosticSubject::path(path);
            diagnostic.with_recovery(RecoveryFact::component(resource))
        }
    }
}

fn terminology_definition_diagnostic(
    path: Option<&std::path::Path>,
    source: &TerminologyDefinitionError,
    stage: DiagnosticStage,
    impact: DiagnosticImpact,
) -> SafeDiagnostic {
    match source {
        TerminologyDefinitionError::Cancelled => SafeDiagnostic::new(
            DiagnosticCode::CommandInput,
            stage,
            DiagnosticSubject::operation("terminology"),
            DiagnosticReason::failure(DiagnosticFailureKind::LockCancelled),
            impact,
            DiagnosticAction::Retry,
        ),
        TerminologyDefinitionError::StartWorker { operation, source } => {
            resource_definition_diagnostic(
                path,
                "terminology",
                DiagnosticReason::io(*operation, source),
                stage,
                impact,
                true,
            )
        }
        TerminologyDefinitionError::InvalidUtf8(source) => resource_definition_diagnostic(
            path,
            "terminology",
            DiagnosticReason::InvalidUtf8 {
                valid_up_to: usize_as_u64(source.valid_up_to()),
                error_len: source.error_len().map(usize_as_u64),
            },
            stage,
            impact,
            false,
        ),
        TerminologyDefinitionError::InvalidToml(source) => resource_definition_diagnostic(
            path,
            "terminology",
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::InvalidSyntax,
                toml_error_detail(source),
            ),
            stage,
            impact,
            false,
        ),
        TerminologyDefinitionError::InvalidSnapshot(source) => resource_definition_diagnostic(
            path,
            "terminology",
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::StateMismatch,
                format!("invalid_snapshot_json; {}", serde_json_error_detail(source)),
            ),
            stage,
            impact,
            false,
        ),
        TerminologyDefinitionError::EncodeSnapshot(source) => resource_definition_diagnostic(
            path,
            "terminology",
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::InternalInvariant,
                format!(
                    "encode_snapshot_failed; {}",
                    serde_json_error_detail(source)
                ),
            ),
            stage,
            impact,
            true,
        ),
        TerminologyDefinitionError::BlankField {
            entry_number,
            field,
        } => resource_definition_diagnostic(
            path,
            "terminology",
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::InvalidValue,
                format!("blank_field; entry={entry_number}; field={field}"),
            ),
            stage,
            impact,
            false,
        ),
        TerminologyDefinitionError::SurroundingWhitespace {
            entry_number,
            field,
        } => resource_definition_diagnostic(
            path,
            "terminology",
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::InvalidValue,
                format!("surrounding_whitespace; entry={entry_number}; field={field}"),
            ),
            stage,
            impact,
            false,
        ),
        TerminologyDefinitionError::ControlCharacter {
            entry_number,
            field,
            character,
        } => resource_definition_diagnostic(
            path,
            "terminology",
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::InvalidValue,
                format!(
                    "control_character; entry={entry_number}; field={field}; codepoint=U+{:04X}",
                    u32::from(*character)
                ),
            ),
            stage,
            impact,
            false,
        ),
        TerminologyDefinitionError::EmptyTriggers { entry_number } => {
            resource_definition_diagnostic(
                path,
                "terminology",
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::InvalidValue,
                    format!("empty_triggers; entry={entry_number}"),
                ),
                stage,
                impact,
                false,
            )
        }
        TerminologyDefinitionError::DuplicateTerm { .. } => resource_definition_diagnostic(
            path,
            "terminology",
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::ConflictingValues,
                "duplicate_term",
            ),
            stage,
            impact,
            false,
        ),
        TerminologyDefinitionError::DuplicateTrigger { .. } => resource_definition_diagnostic(
            path,
            "terminology",
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::ConflictingValues,
                "duplicate_trigger",
            ),
            stage,
            impact,
            false,
        ),
        TerminologyDefinitionError::CompileMatcher(_) => resource_definition_diagnostic(
            path,
            "terminology",
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::InvalidValue,
                "terminology_matcher_build_failed",
            ),
            stage,
            impact,
            false,
        ),
    }
}

fn placeholder_definition_diagnostic(
    path: Option<&std::path::Path>,
    source: &PlaceholderDefinitionError,
    stage: DiagnosticStage,
    impact: DiagnosticImpact,
) -> SafeDiagnostic {
    match source {
        PlaceholderDefinitionError::Cancelled => SafeDiagnostic::new(
            DiagnosticCode::CommandInput,
            stage,
            DiagnosticSubject::operation("placeholder_rules"),
            DiagnosticReason::failure(DiagnosticFailureKind::LockCancelled),
            impact,
            DiagnosticAction::Retry,
        ),
        PlaceholderDefinitionError::StartWorker { operation, source } => {
            resource_definition_diagnostic(
                path,
                "placeholder_rules",
                DiagnosticReason::io(*operation, source),
                stage,
                impact,
                true,
            )
        }
        PlaceholderDefinitionError::InvalidUtf8(source) => resource_definition_diagnostic(
            path,
            "placeholder_rules",
            DiagnosticReason::InvalidUtf8 {
                valid_up_to: usize_as_u64(source.valid_up_to()),
                error_len: source.error_len().map(usize_as_u64),
            },
            stage,
            impact,
            false,
        ),
        PlaceholderDefinitionError::InvalidToml(source) => resource_definition_diagnostic(
            path,
            "placeholder_rules",
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::InvalidSyntax,
                toml_error_detail(source),
            ),
            stage,
            impact,
            false,
        ),
        PlaceholderDefinitionError::InvalidSnapshot(source) => resource_definition_diagnostic(
            path,
            "placeholder_rules",
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::StateMismatch,
                format!("invalid_snapshot_json; {}", serde_json_error_detail(source)),
            ),
            stage,
            impact,
            false,
        ),
        PlaceholderDefinitionError::EncodeSnapshot(source) => resource_definition_diagnostic(
            path,
            "placeholder_rules",
            DiagnosticReason::failure_with_detail(
                DiagnosticFailureKind::InternalInvariant,
                format!(
                    "encode_snapshot_failed; {}",
                    serde_json_error_detail(source)
                ),
            ),
            stage,
            impact,
            true,
        ),
    }
}

fn resource_definition_diagnostic(
    path: Option<&std::path::Path>,
    resource: &'static str,
    reason: DiagnosticReason,
    stage: DiagnosticStage,
    impact: DiagnosticImpact,
    internal: bool,
) -> SafeDiagnostic {
    let external = path.is_some();
    let code = if internal {
        DiagnosticCode::InternalOperation
    } else if external {
        DiagnosticCode::CommandInput
    } else {
        DiagnosticCode::ProjectState
    };
    let action = if internal {
        DiagnosticAction::ReportBug
    } else if external {
        DiagnosticAction::FixInput
    } else {
        DiagnosticAction::CheckProjectState
    };
    let subject = path.map_or_else(
        || DiagnosticSubject::component(format!("project_{resource}_snapshot")),
        DiagnosticSubject::path,
    );
    SafeDiagnostic::new(code, stage, subject, reason, impact, action)
}

fn placeholder_compilation_diagnostic(
    source: &PlaceholderRuleCompilationError,
    stage: DiagnosticStage,
    impact: DiagnosticImpact,
) -> SafeDiagnostic {
    let (rule_number, detail) = match source {
        PlaceholderRuleCompilationError::StartWorker { operation, source } => {
            return SafeDiagnostic::io(
                DiagnosticCode::InternalOperation,
                stage,
                DiagnosticSubject::operation("custom_placeholder_compile"),
                *operation,
                source,
                impact,
                DiagnosticAction::ReportBug,
            );
        }
        PlaceholderRuleCompilationError::EmptyScopes { rule_number } => {
            (*rule_number, "empty_scopes".to_owned())
        }
        PlaceholderRuleCompilationError::UnknownScope { rule_number, .. } => {
            (*rule_number, "unknown_scope".to_owned())
        }
        PlaceholderRuleCompilationError::DuplicateScope { rule_number, .. } => {
            (*rule_number, "duplicate_scope".to_owned())
        }
        PlaceholderRuleCompilationError::EmptyPattern { rule_number } => {
            (*rule_number, "empty_pattern".to_owned())
        }
        PlaceholderRuleCompilationError::InvalidPattern {
            rule_number,
            source,
        } => (
            *rule_number,
            format!(
                "invalid_pcre2_pattern; kind={}; code={}; offset={:?}",
                pcre2_error_kind(source),
                source.code(),
                source.offset()
            ),
        ),
        PlaceholderRuleCompilationError::InvalidNamedCaptures {
            rule_number,
            captures,
        } => (
            *rule_number,
            format!("invalid_named_capture_set; actual_count={}", captures.len()),
        ),
    };
    SafeDiagnostic::new(
        DiagnosticCode::CommandInput,
        stage,
        DiagnosticSubject::operation(format!("placeholder_rule_{rule_number}")),
        DiagnosticReason::failure_with_detail(DiagnosticFailureKind::InvalidValue, detail),
        impact,
        DiagnosticAction::FixInput,
    )
}

pub(crate) fn scope_preprocessing_detail(source: &ScopePreprocessingError) -> String {
    match source {
        ScopePreprocessingError::StateLocation(source) => {
            format!(
                "encode_translation_state_location: {}",
                location_codec_detail(source)
            )
        }
        ScopePreprocessingError::StateRole(source) => format!(
            "encode_translation_state_role: {}",
            projection_codec_detail(source)
        ),
        ScopePreprocessingError::SemanticOrder(source) => {
            format!("encode_translation_semantic_order: {source}")
        }
    }
}

fn location_codec_detail(
    source: &crate::rpg_maker::location_codec::RpgMakerLocationCodecError,
) -> String {
    source.safe_diagnostic_detail()
}

fn projection_codec_detail(
    source: &crate::rpg_maker::location_codec::RpgMakerProjectionCodecError,
) -> String {
    source.safe_diagnostic_detail()
}

fn toml_error_detail(source: &toml::de::Error) -> String {
    source.span().map_or_else(
        || "invalid_toml".to_owned(),
        |span| {
            format!(
                "invalid_toml; byte_start={}; byte_end={}",
                span.start, span.end
            )
        },
    )
}

fn serde_json_error_detail(source: &serde_json::Error) -> String {
    let category = JsonErrorCategory::from(source);
    format!(
        "json_category={category}; line={}; column={}",
        source.line(),
        source.column()
    )
}

fn pcre2_error_kind(source: &pcre2::Error) -> &'static str {
    match source.kind() {
        pcre2::ErrorKind::Compile => "compile",
        pcre2::ErrorKind::JIT => "jit",
        pcre2::ErrorKind::Match => "match",
        pcre2::ErrorKind::Info => "info",
        pcre2::ErrorKind::Option => "option",
        _ => "unknown",
    }
}

fn safe_scope_label(scope: &RpgMakerSemanticScopeKey) -> String {
    match scope {
        RpgMakerSemanticScopeKey::StandardDatabase(file) => format!("data/{}", file.file_name()),
        RpgMakerSemanticScopeKey::DataFile(file) => format!("data/{file}"),
        RpgMakerSemanticScopeKey::System => "data/System.json".to_owned(),
        RpgMakerSemanticScopeKey::Map(map_id) => format!("Map{:03}", map_id.get()),
        RpgMakerSemanticScopeKey::CommonEvent(event_id) => format!("CommonEvent[{event_id}]"),
        RpgMakerSemanticScopeKey::Troop(troop_id) => format!("Troop[{troop_id}]"),
        RpgMakerSemanticScopeKey::Plugin { plugin_index, .. } => {
            format!("Plugin[index={plugin_index}]")
        }
    }
}

fn usize_as_u64(value: usize) -> u64 {
    u64::try_from(value).expect("当前目标平台的 usize 必须可表示为 u64")
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
    use crate::runtime::cpu::CpuExecutorUnavailable;
    use crate::runtime::filesystem::SystemFileSystemError;
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
        TranslationResponseEnvelope,
    };
    use crate::translation::planning_resource::{
        TranslationPlanningResourceReadingService, TranslationPlanningResources,
    };
    use crate::translation::profile::TranslationRequestConfiguration;

    fn task_id(value: usize) -> TaskId {
        TaskId::new(value).expect("测试 Task ID 必须非零")
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

    type ProductionResourceError =
        TranslationPlanningResourceReadingError<SystemFileSystemError, CpuExecutorUnavailable>;
    type ProductionPlanningError =
        RpgMakerTranslationTaskPlanningError<ProductionResourceError, CpuExecutorUnavailable>;

    #[test]
    fn cancellable_global_semantics_preserves_fingerprint_and_observes_chunks() {
        let system_markdown = "系统提示。".repeat(32 * 1024);
        let language_semantics = Sha256Fingerprint::from_bytes([1; 32]);
        let client_semantics = Sha256Fingerprint::from_bytes([2; 32]);
        let expected = global_translation_semantics(
            RpgMakerEngine::Mz,
            "ja",
            "zh-Hans",
            language_semantics,
            &system_markdown,
            client_semantics,
        );
        let mut polls = 0_usize;
        let actual = global_translation_semantics_with_cancellation(
            RpgMakerEngine::Mz,
            "ja",
            "zh-Hans",
            language_semantics,
            &system_markdown,
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
            "ja",
            "zh-Hans",
            language_semantics,
            &system_markdown,
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
                    },
                    PreparedAsset {
                        identity: identity(2, second_source, second_context),
                        semantic_order_key: RpgMakerSemanticOrderKey::new(vec![second_order], 1),
                        translation: with_translation
                            .then(|| TextUnitContent::Value("译文二".to_owned())),
                        translation_state: with_translation
                            .then(|| Sha256Fingerprint::from_bytes([2; 32])),
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

    #[test]
    fn planning_diagnostic_preserves_resource_path_and_utf8_offset() {
        let invalid_bytes = vec![0xff];
        let invalid_utf8 = std::str::from_utf8(&invalid_bytes).expect_err("测试字节必须不是 UTF-8");
        let error: ProductionPlanningError = RpgMakerTranslationTaskPlanningError::ReadResources(
            TranslationPlanningResourceReadingError::InvalidTerminology {
                path: Some(PathBuf::from("C:/game/terms.toml")),
                source: TerminologyDefinitionError::InvalidUtf8(invalid_utf8),
            },
        );

        let diagnostic = error.safe_diagnostic_source(
            DiagnosticStage::Translate,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixInput,
        );

        assert!(matches!(
            diagnostic.subject,
            DiagnosticSubject::Path { ref path } if path.ends_with("terms.toml")
        ));
        assert_eq!(
            diagnostic.reason,
            DiagnosticReason::InvalidUtf8 {
                valid_up_to: 0,
                error_len: Some(1),
            }
        );
        assert_eq!(diagnostic.action, DiagnosticAction::FixInput);
    }

    #[test]
    fn planning_diagnostic_uses_stable_resource_facts_and_preserves_rule_number() {
        let sentinel = "TRANSLATION_RESOURCE_VALUE_SENTINEL";
        let terminology: ProductionPlanningError =
            RpgMakerTranslationTaskPlanningError::ReadResources(
                TranslationPlanningResourceReadingError::InvalidTerminology {
                    path: Some(PathBuf::from("C:/game/terms.toml")),
                    source: TerminologyDefinitionError::DuplicateTerm {
                        term: sentinel.to_owned(),
                    },
                },
            );
        let terminology = terminology.safe_diagnostic_source(
            DiagnosticStage::Translate,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixInput,
        );
        assert!(!terminology.reason.render().contains(sentinel));
        assert!(terminology.reason.render().contains("duplicate_term"));

        let placeholder: ProductionPlanningError =
            RpgMakerTranslationTaskPlanningError::InvalidPlaceholderRules(
                PlaceholderRuleCompilationError::UnknownScope {
                    rule_number: 7,
                    scope: sentinel.to_owned(),
                },
            );
        let placeholder = placeholder.safe_diagnostic_source(
            DiagnosticStage::Translate,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::FixInput,
        );
        assert!(matches!(
            placeholder.subject,
            DiagnosticSubject::Operation { ref name } if name == "placeholder_rule_7"
        ));
        assert!(!placeholder.reason.render().contains(sentinel));
        assert!(placeholder.reason.render().contains("unknown_scope"));
    }

    #[test]
    fn planning_diagnostic_preserves_cpu_cancellation() {
        let cancelled: ProductionPlanningError =
            RpgMakerTranslationTaskPlanningError::PrepareCorpusCompute(
                CpuTaskExecutionError::Cancelled,
            );
        let cancelled = cancelled.safe_diagnostic_source(
            DiagnosticStage::Translate,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::Retry,
        );
        assert!(matches!(
            cancelled.reason,
            DiagnosticReason::Failure {
                failure: DiagnosticFailureKind::LockCancelled
            }
        ));
    }

    #[test]
    fn invalid_output_contract_is_a_safe_planner_failure() {
        let sentinel = "PLANNER_PLACEHOLDER_TOKEN_SENTINEL";
        let identity = TranslationUnitIdentity::new(
            RpgMakerAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            RpgMakerLocation::value(RpgMakerSource::map(9), vec![RpgMakerLocationStep::index(2)]),
            TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法")),
            TextUnitContent::Value("原文".to_owned()),
            "{}",
        );
        let error: ProductionPlanningError =
            RpgMakerTranslationTaskPlanningError::InvalidOutputContract(
                ExpectedTranslationOutputContractError::placeholder_index_invalid(
                    task_id(4),
                    &identity,
                    LanguageTextProjectionError::MissingToken {
                        token: sentinel.to_owned(),
                    },
                ),
            );

        let diagnostic = error.safe_diagnostic_source(
            DiagnosticStage::Translate,
            DiagnosticImpact::Unchanged,
            DiagnosticAction::Retry,
        );

        assert_eq!(diagnostic.code, DiagnosticCode::InternalOperation);
        assert_eq!(diagnostic.action, DiagnosticAction::ReportBug);
        assert!(matches!(
            diagnostic.subject,
            DiagnosticSubject::Operation { ref name }
                if name.contains("translation_output_contract")
                    && name.contains("unit=4")
                    && name.contains("owner=builtin")
                    && name.contains("group_kind=database_entry")
                    && name.contains("Map009.json[2]")
                    && name.contains("role=name")
        ));
        assert!(
            diagnostic
                .reason
                .render()
                .contains("placeholder_index_invalid")
        );
        assert!(diagnostic.reason.render().contains("unit=4"));
        assert!(diagnostic.reason.render().contains("owner=builtin"));
        assert!(
            diagnostic
                .reason
                .render()
                .contains("group_kind=database_entry")
        );
        assert!(diagnostic.reason.render().contains("Map009.json[2]"));
        assert!(diagnostic.reason.render().contains("role=name"));
        assert!(
            diagnostic
                .reason
                .render()
                .contains("missing_required_placeholder_token")
        );
        assert!(!diagnostic.reason.render().contains(sentinel));
        assert!(!format!("{error:?}").contains(sentinel));
    }

    #[test]
    fn prompt_protocol_uses_canonical_english_labels() {
        assert_eq!(
            [
                TextGroupKind::DatabaseEntry,
                TextGroupKind::System,
                TextGroupKind::Map,
                TextGroupKind::EventDialogue,
                TextGroupKind::EventChoices,
                TextGroupKind::EventScrollingText,
                TextGroupKind::EventCommand,
                TextGroupKind::PluginParameter,
            ]
            .map(human_group_kind),
            [
                "Database Text",
                "System Text",
                "Map Text",
                "Dialogue",
                "Choices",
                "Scrolling Text",
                "Event Command",
                "Plugin Parameters",
            ]
        );
        for (field_name, expected) in [
            ("name", "Name"),
            ("displayName", "Map Name"),
            ("nickname", "Nickname"),
            ("profile", "Profile"),
            ("description", "Description"),
        ] {
            assert_eq!(human_scalar_label(field_name), expected);
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
            None,
        ));
        let pair = LanguagePair::new(
            LanguageId::parse(source_language).expect("测试源语言应合法"),
            LanguageId::parse(target_language).expect("测试目标语言应合法"),
        );
        let prompt =
            RpgMakerSystemPrompt::new(pair, system_markdown, TranslationResponseEnvelope::JsonOnly)
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
        placeholder_definitions: Vec<super::super::placeholder::PlaceholderRuleDefinition>,
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
        let semantics =
            ResolvedTranslationSemantics::for_test_with_placeholders(placeholder_definitions);
        let prepared = semantics
            .prepare_content(identity.kind(), identity.source_content())
            .expect("人工 Current 测试原文应可准备");
        let group_context = shared_group_context_fingerprint_with_cancellation(
            TextGroupKind::DatabaseEntry,
            &group_order,
            std::iter::once((&unit_order, &identity)),
            || Ok::<_, Infallible>(()),
        )
        .expect("人工 Current 测试 Group 指纹不能取消")
        .expect("人工 Current 测试 Group 指纹应可建立");
        let translation_state = manual_translation_state_fingerprint(
            semantics.engine(),
            semantics.language_pair(),
            group_context,
            &identity,
            prepared.placeholders(),
        )
        .expect("人工 Current 测试状态应可建立");

        RpgMakerTranslationGroup::with_semantic_order_key(
            TextGroupKind::DatabaseEntry,
            group_location,
            group_order,
            vec![RpgMakerTranslationAsset::with_semantic_order_key(
                identity,
                unit_order,
                Some(TextUnitContent::Value(translation.to_owned())),
                Some(translation_state),
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
            let prepared = semantics
                .prepare_content(identity.kind(), identity.source_content())
                .expect("测试原文应可准备");
            let group_order = RpgMakerSemanticOrderKey::new(vec![1], 0);
            let unit_order = RpgMakerSemanticOrderKey::new(vec![1], 0);
            let group_context = shared_group_context_fingerprint_with_cancellation(
                TextGroupKind::DatabaseEntry,
                &group_order,
                std::iter::once((&unit_order, &identity)),
                || Ok::<_, Infallible>(()),
            )
            .expect("测试 Group 指纹不能取消")
            .expect("测试 Group 指纹应可建立");
            let state = manual_translation_state_fingerprint(
                semantics.engine(),
                semantics.language_pair(),
                group_context,
                &identity,
                prepared.placeholders(),
            )
            .expect("人工状态应可建立");
            let scope = PreparedScope {
                key: RpgMakerSemanticScopeKey::StandardDatabase(StandardDataFile::Actors),
                groups: vec![PreparedGroup {
                    kind: TextGroupKind::DatabaseEntry,
                    semantic_order_key: group_order,
                    assets: vec![PreparedAsset {
                        identity,
                        semantic_order_key: unit_order,
                        translation: Some(TextUnitContent::Value("人工译文".to_owned())),
                        translation_state: Some(state),
                    }],
                }],
            };

            let result = preprocess_scope(scope, semantics, &CooperativeCancellation::default())
                .expect("人工 Current 应可预处理");
            let unit = result.scope.groups[0].units[0]
                .as_ref()
                .expect("人工 Current Unit 应完成译前准备");
            assert!(unit.current);
            assert!(!unit.invalidated);
            assert_eq!(result.invalidated, 0);
            assert!(result.invalidations.is_empty());
        }
    }

    #[test]
    fn manual_current_tracks_sibling_source_but_ignores_sibling_translation() {
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
        let original_body = body("こんにちは");
        let group_context = shared_group_context_fingerprint_with_cancellation(
            TextGroupKind::EventDialogue,
            &group_order,
            [(&speaker_order, &speaker), (&body_order, &original_body)].into_iter(),
            || Ok::<_, Infallible>(()),
        )
        .expect("测试 Group 指纹不能取消")
        .expect("测试 Group 指纹应可建立");
        let prepared_speaker = semantics
            .prepare_content(speaker.kind(), speaker.source_content())
            .expect("测试说话者应可准备");
        let speaker_state = manual_translation_state_fingerprint(
            semantics.engine(),
            semantics.language_pair(),
            group_context,
            &speaker,
            prepared_speaker.placeholders(),
        )
        .expect("人工状态应可建立");
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
                    },
                    PreparedAsset {
                        identity: body(body_source),
                        semantic_order_key: body_order.clone(),
                        translation: body_translation
                            .map(|value| TextUnitContent::Value(value.to_owned())),
                        translation_state: body_translation
                            .map(|_| Sha256Fingerprint::from_bytes([0x91; 32])),
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
            translation_only.scope.groups[0].units[0]
                .as_ref()
                .expect("说话者应完成准备")
                .current,
            "兄弟目标译文不能使人工译文失去 Current"
        );

        let source_changed = preprocess_scope(
            scope("こんばんは", None),
            semantics,
            &CooperativeCancellation::default(),
        )
        .expect("兄弟原文变化应可预处理");
        assert!(
            !source_changed.scope.groups[0].units[0]
                .as_ref()
                .expect("说话者应完成准备")
                .current,
            "跨 owner 兄弟 Unit 的原文变化必须使人工译文失去 Current"
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
                    },
                    PreparedAsset {
                        identity: body(body_source),
                        semantic_order_key: body_order,
                        translation: body_translation
                            .map(|value| TextUnitContent::Value(value.to_owned())),
                        translation_state: body_translation
                            .map(|_| Sha256Fingerprint::from_bytes([0x91; 32])),
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
                .as_ref()
                .expect("说话者应完成准备")
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

    #[test]
    fn terminology_values_are_rendered_as_markdown_literals() {
        let mut markdown = String::new();
        push_markdown_literal(&mut markdown, r"A*[B] <C> \");
        assert_eq!(markdown, r"A\*\[B\] \<C\> \\");
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
        assert_eq!(tasks[0].expected_outputs()[0].id(), task_id(1));
        assert_eq!(
            tasks[0].expected_outputs()[0].applied_placeholders().len(),
            1
        );
        assert_eq!(
            tasks[0].messages()[0].content(),
            "# System\n完整且由外部提供。"
        );
        let user = tasks[0].messages()[1].content();
        assert!(user.starts_with("Terminology:\n\n"));
        assert!(user.contains("- 魔法剣 → 魔法之剑"));
        assert!(user.contains("Name [1] (single line):"));
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
        assert!(tasks[0].messages()[1].content().contains(original));
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
        let prompt = tasks[0].messages()[1].content();
        for expected in [
            concat!(
                "<json>.text[0] [1] (free line breaking):\n",
                "\n",
                "> ゲームパッドが接続されていません\n",
                "> ボタンを押して再度試してください\n",
            ),
            concat!(
                "<json>.text[0] [2] (free line breaking):\n",
                "\n",
                "> コンフィグを終了するためには\n",
                "> ボタンから手を放してください。\n",
            ),
        ] {
            assert!(prompt.contains(expected), "多行标量应使用自由断行契约");
        }
        assert!(!prompt.contains("(single line)"));
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
            vec![task_id(1), task_id(2), task_id(3), task_id(4), task_id(5)]
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
            user,
            concat!(
                "## Scrolling Text\n",
                "\n",
                "Scrolling Text [1] (3 lines, corresponding line by line):\n",
                "\n",
                "> 制作\n",
                "> \n",
                "> 終わり\n",
                "\n",
                "## Choices\n",
                "\n",
                "Choices [2] (2 items, corresponding item by item):\n",
                "\n",
                "> はい\n",
                "> いいえ\n",
                "\n",
                "## Dialogue\n",
                "\n",
                "Speaker [3] (single line):アリス\n",
                "\n",
                "Body [4] (free line breaking):\n",
                "\n",
                "> 今日はいい天気ですね。\n",
                "> 一緒に町へ\n",
                "> 行きませんか？\n",
                "\n",
                "## Map Text\n",
                "\n",
                "Map Name [5] (single line):始まりの町\n",
            )
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
        assert_eq!(tasks[0].expected_outputs()[0].id(), task_id(1));
        let user = tasks[0].messages()[1].content();
        assert!(user.contains("Speaker [-] (single line):爱丽丝"));
        assert!(!user.contains("アリス"));
        assert!(user.contains("Body [1] (free line breaking):"));
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
        let user = user_message(&tasks[0]);
        assert_eq!(
            user.matches("- 魔王 → 魔王（Demon King）").count(),
            1,
            "完整块术语应按文件顺序只提供一次"
        );
        assert!(user.contains("Name [-] (single line):已有上下文 ⟦ATT_"));
        assert!(user.contains("Name [1] (single line):こんにちは"));
        assert!(!user.contains("{hero}"));
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
        let non_source = manual_current_group(
            source.clone(),
            1,
            "12345 {hero}",
            "人工数字语境 {hero}",
            definitions.clone(),
        );
        let fully_protected = manual_current_group(
            source.clone(),
            2,
            "保護対象",
            "完整保护语境 保護対象",
            definitions,
        );
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
        let user = user_message(&tasks[0]);
        assert!(user.contains("Name [-] (single line):人工数字语境 ⟦ATT_"));
        assert!(user.contains("Name [-] (single line):完整保护语境 ⟦ATT_"));
        assert!(user.contains("Name [1] (single line):翻訳対象"));
        assert!(!user.contains("12345 {hero}"));
        assert!(!user.contains("{hero}"));
        assert!(!user.contains("保護対象"));
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
        assert_eq!(tasks[0].expected_outputs()[0].id(), task_id(1));
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

        let user = tasks[0].messages()[1].content();
        assert!(user.contains("- 勇者 → 英雄"));
        assert!(user.contains("- プフクスッ → 噗呼咯"));
        assert!(
            !user.contains("- プフクス → 普芙库丝"),
            "同一起点被最长 trigger 抑制的姓名术语不得进入 Prompt"
        );
        assert!(!user.contains("- 秘密 →"), "协议壳不得触发术语");
        assert!(!user.contains("- 前後 →"), "术语不得跨不透明边界拼接");
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
        let user = tasks[0].messages()[1].content();
        assert_eq!(
            user.matches("保存しますか？").count(),
            2,
            "去重只合并模型责任，不能从完整 TaskBlock 删除重复原文语境"
        );
        assert!(user.contains("Name [2] (single line):保存しますか？"));
        assert!(user.contains("Name [-] (single line):保存しますか？"));
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

    #[tokio::test]
    async fn placeholder_projection_failure_suppresses_its_complete_task_block_and_invalidates_old_translation()
     {
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

        let (_, preparation, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                RpgMakerTranslationCorpus::new(vec![bad, good]),
                RpgMakerTranslationInput::new(None, Some(placeholder_path)),
            )
            .await
            .expect("Placeholder 冲突应形成规划失败并禁止发送残缺 TaskBlock")
            .into_parts();

        assert_eq!(preparation.planning_failures().len(), 1);
        assert!(matches!(
            preparation.planning_failures()[0].reason(),
            TranslationPlanningFailureReason::PlaceholderProtection { .. }
        ));
        assert_eq!(preparation.invalidations().len(), 1);
        assert_eq!(preparation.invalidated(), 1);
        assert!(
            tasks.is_empty(),
            "失败 Unit 与正常兄弟 Group 同属一个完整块时，不能删除失败 Unit 后只发送正常原文"
        );
    }

    #[tokio::test]
    async fn line_crossing_placeholder_failure_skips_its_block_but_keeps_another_scope_block() {
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

        let (_, preparation, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                RpgMakerTranslationCorpus::new(vec![bad, good]),
                RpgMakerTranslationInput::new(None, Some(placeholder_path)),
            )
            .await
            .expect("一个完整块失败不应阻断另一 Semantic Scope 的完整块")
            .into_parts();

        assert_eq!(preparation.planning_failures().len(), 1);
        assert_eq!(
            preparation.planning_failures()[0].reason(),
            &TranslationPlanningFailureReason::PlaceholderProtection {
                message: "placeholder_crosses_line_boundary; rule=1; source_line_index=0"
                    .to_owned(),
            }
        );
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].expected_outputs().len(), 1);
        assert!(tasks[0].messages()[1].content().contains("正常な翻訳"));
        assert!(!tasks[0].messages()[1].content().contains("翻訳<opaque>"));
    }

    #[tokio::test]
    async fn failed_block_dedup_member_cannot_take_healthy_block_model_responsibility() {
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

        let (_, preparation, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                RpgMakerTranslationCorpus::new(vec![bad, blocked_duplicate, healthy_duplicate]),
                RpgMakerTranslationInput::new(None, Some(placeholder_path)),
            )
            .await
            .expect("坏块不得阻止健康重复项取得模型责任")
            .into_parts();

        assert_eq!(preparation.planning_failures().len(), 1);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].expected_outputs().len(), 1);
        let output = &tasks[0].expected_outputs()[0];
        assert_eq!(output.id(), task_id(1));
        assert_eq!(
            output.identity().group_location().source(),
            &RpgMakerSource::map(2)
        );
        assert_eq!(output.propagation_targets().len(), 1);
        assert_eq!(
            output.propagation_targets()[0].group_location().source(),
            &RpgMakerSource::map(1)
        );
        assert!(user_message(&tasks[0]).contains("Name [1] (single line):重複テキスト"));
        assert!(!user_message(&tasks[0]).contains("翻訳<BAD>"));
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
        assert_eq!(split[0].expected_outputs()[0].id(), task_id(1));
        assert_eq!(split[1].expected_outputs()[0].id(), task_id(1));
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
                assert_eq!(outputs[0].id(), task_id(1));
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

        let mut sparse_lines = String::new();
        let visited = prompt.append_selected(&mut sparse_lines, &selected);
        assert_eq!(visited, 3, "渲染工作量必须只随实际命中数增长");
        assert_eq!(
            sparse_lines,
            concat!(
                "- 术语\\-0001\\-末 → 译文\\-0001\\-末\n",
                "- 术语\\-2048\\-末 → 译文\\-2048\\-末\n",
                "- 术语\\-4095\\-末 → 译文\\-4095\\-末\n",
            ),
            "稀疏索引不得改变术语文件自然顺序或 Markdown 转义"
        );

        let rendered = render_user_markdown(&[], None, &prompt, &selected);
        assert_eq!(rendered, format!("Terminology:\n\n{sparse_lines}"));
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
