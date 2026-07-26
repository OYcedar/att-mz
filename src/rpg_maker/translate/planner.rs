//! RPG Maker 标准翻译任务规划：自然排序、语义范围、虚原文、术语和占位符。

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::sync::Arc;

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, RecoveryFact, SafeDiagnostic, SafeDiagnosticSource,
};
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::json_diagnostic::JsonErrorCategory;
use crate::language::{LanguageAnalysis, LanguagePair};
use crate::llm::{ChatMessage, ChatMessageRole, LlmClientConcurrency, LlmClientSemanticIdentity};
use crate::rpg_maker::RpgMakerEngine;
use crate::rpg_maker::location_codec::{RpgMakerLocationCodec, RpgMakerProjectionCodec};
use crate::rpg_maker::model::{TextUnitContent, TextUnitRole};
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::text::{DataFileName, MapId};
use crate::rpg_maker::text::{
    RpgMakerLocationStep, RpgMakerSource, StandardDataFile, TextGroupKind,
};
use crate::storage::file_system::ReadFileError;

use super::deduplication::{
    TranslationDeduplicationCandidate, TranslationDeduplicationError,
    TranslationDeduplicationOutcome, deduplicate_translation_candidates,
};
use super::language_projection::LanguageTextProjectionError;
use super::placeholder::{
    Pcre2PlaceholderService, PlaceholderProtectionError, PlaceholderRuleCompilationError,
};
use super::planning_resource::{
    CompiledTerminology, PlaceholderDefinitionError, TerminologyDefinitionError,
    TranslationPlanningResourceReader, TranslationPlanningResourceReadingError,
};
use super::profile::{ResolvedRpgMakerTranslationResources, RpgMakerTranslationProfile};
use super::semantics::{
    PreparedTranslationStatus, ResolvedTranslationSemanticError, ResolvedTranslationSemantics,
};
use super::standard::{
    ExpectedLineShape, ExpectedTranslationOutput, ExpectedTranslationValidation,
    StandardTranslationCorpus, StandardTranslationGroup, StandardTranslationInput,
    StandardTranslationPlan, StandardTranslationTaskIndex, StandardTranslationTaskPlanner,
    TerminologyDependency, TranslationInvalidation, TranslationPlanPreparation,
    TranslationPlanPreparationCounts, TranslationPlanningFailure, TranslationPlanningFailureReason,
    TranslationPropagationTarget, TranslationStateContext, TranslationTaskBlock,
    TranslationUnitIdentity, TranslationVirtualReason,
};

/// 使用三个职责模块与 CPU 根建立确定性 RPG Maker 翻译计划。
pub(crate) struct RpgMakerStandardTranslationTaskPlanningService<R, C, L> {
    resources: R,
    translation_resources: Arc<ResolvedRpgMakerTranslationResources>,
    placeholders: Pcre2PlaceholderService,
    cpu: C,
    llm_client: PhantomData<fn() -> L>,
}

impl<R, C, L> RpgMakerStandardTranslationTaskPlanningService<R, C, L> {
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
            llm_client: PhantomData,
        }
    }
}

struct ResolvedCorpusSemantics {
    groups: Vec<StandardTranslationGroup>,
    snapshot_baseline: super::standard::TranslationSnapshotBaseline,
    terminology: Arc<CompiledTerminology>,
    terminology_json: String,
    placeholder_rules_json: String,
    semantics: Arc<ResolvedTranslationSemantics>,
    system_markdown: String,
    task_language_pair: LanguagePair,
}

impl<R, C, L> RpgMakerStandardTranslationTaskPlanningService<R, C, L>
where
    R: TranslationPlanningResourceReader,
    C: CpuTaskExecutor,
    L: LlmClientConcurrency + LlmClientSemanticIdentity + 'static,
{
    async fn resolve_corpus_semantics(
        &self,
        project: &OpenedProject,
        profile: &Arc<RpgMakerTranslationProfile<L>>,
        corpus: StandardTranslationCorpus,
        input: StandardTranslationInput,
    ) -> Result<
        ResolvedCorpusSemantics,
        RpgMakerStandardTranslationTaskPlanningError<R::Error, C::Error>,
    > {
        let resolved_pair = self.translation_resources.language_pair();
        if project.source_language() != resolved_pair.source()
            || project.target_language() != resolved_pair.target()
        {
            return Err(
                RpgMakerStandardTranslationTaskPlanningError::ResolvedLanguagePairMismatch {
                    project_source: project.source_language().to_string(),
                    project_target: project.target_language().to_string(),
                    resolved_source: resolved_pair.source().to_string(),
                    resolved_target: resolved_pair.target().to_string(),
                },
            );
        }
        let system_markdown = self
            .translation_resources
            .system_prompt()
            .markdown()
            .to_owned();
        let source_language = self.translation_resources.source_language();
        let (groups, snapshot_baseline) = corpus.into_parts();
        let current_terminology_json = snapshot_baseline.terminology_json().to_owned();
        let current_placeholder_rules_json = snapshot_baseline.placeholder_rules_json().to_owned();
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
            .map_err(RpgMakerStandardTranslationTaskPlanningError::ReadResources)?;
        let (terminology, placeholder_definitions, terminology_json, placeholder_rules_json) =
            resources.into_parts();

        let placeholder_service = self.placeholders.clone();
        let custom_placeholders = self
            .cpu
            .execute(move || placeholder_service.compile_custom(placeholder_definitions))
            .await
            .map_err(RpgMakerStandardTranslationTaskPlanningError::CompilePlaceholdersCompute)?
            .map_err(RpgMakerStandardTranslationTaskPlanningError::InvalidPlaceholderRules)?;
        let source_language_id = project.source_language().to_owned();
        let target_language_id = project.target_language().to_owned();
        let engine = project.layout().rpg_maker_layout().engine();
        let global_semantics = global_translation_semantics(
            engine,
            source_language_id.as_str(),
            target_language_id.as_str(),
            source_language.semantic_fingerprint(),
            &system_markdown,
            profile.llm_client().semantic_fingerprint(),
        );
        let task_language_pair = LanguagePair::new(source_language_id, target_language_id);
        let semantics = Arc::new(ResolvedTranslationSemantics::new(
            engine,
            system_markdown.clone(),
            task_language_pair.clone(),
            Arc::clone(&terminology),
            self.placeholders.clone(),
            custom_placeholders,
            source_language,
            global_semantics,
        ));
        Ok(ResolvedCorpusSemantics {
            groups,
            snapshot_baseline,
            terminology,
            terminology_json,
            placeholder_rules_json,
            semantics,
            system_markdown,
            task_language_pair,
        })
    }

    /// 使用项目当前 canonical 资源打开无副作用的人工候选语义会话。
    pub(crate) async fn open_candidate_session(
        &self,
        project: &OpenedProject,
        profile: &Arc<RpgMakerTranslationProfile<L>>,
        corpus: StandardTranslationCorpus,
    ) -> Result<
        super::candidate::StandardCandidateSession,
        OpenStandardCandidateSessionError<R::Error, C::Error>,
    > {
        let resolved = self
            .resolve_corpus_semantics(
                project,
                profile,
                corpus,
                StandardTranslationInput::new(None, None),
            )
            .await
            .map_err(OpenStandardCandidateSessionError::Planning)?;
        let ResolvedCorpusSemantics {
            groups,
            snapshot_baseline,
            terminology_json,
            placeholder_rules_json,
            semantics,
            ..
        } = resolved;
        let baseline = super::standard::TranslationSnapshotBaseline::new(
            snapshot_baseline.source_snapshot_fingerprint(),
            snapshot_baseline.owner_snapshots().to_vec(),
            terminology_json,
            placeholder_rules_json,
        );
        let prepared = self
            .cpu
            .execute(move || prepare_corpus(groups))
            .await
            .map_err(OpenStandardCandidateSessionError::ScheduleBuild)?
            .map_err(|source| {
                OpenStandardCandidateSessionError::Build(
                    super::candidate::StandardCandidateSessionBuildError::Corpus(source),
                )
            })?;
        let scope_semantics = semantics;
        let prepared_scopes = self
            .cpu
            .execute_ordered_map(prepared.into_scopes(), move |scope| {
                super::candidate::prepare_candidate_scope(Arc::clone(&scope_semantics), scope)
            })
            .await
            .map_err(OpenStandardCandidateSessionError::ScheduleBuild)?
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map_err(OpenStandardCandidateSessionError::Build)?;
        let session = self
            .cpu
            .execute(move || {
                super::candidate::StandardCandidateSession::from_prepared_scopes(
                    baseline,
                    prepared_scopes,
                )
            })
            .await
            .map_err(OpenStandardCandidateSessionError::ScheduleBuild)?;
        Ok(session)
    }
}

impl<R, C, L> StandardTranslationTaskPlanner
    for RpgMakerStandardTranslationTaskPlanningService<R, C, L>
where
    R: TranslationPlanningResourceReader,
    C: CpuTaskExecutor,
    L: LlmClientConcurrency + LlmClientSemanticIdentity + 'static,
{
    type Profile = Arc<RpgMakerTranslationProfile<L>>;
    type Error = RpgMakerStandardTranslationTaskPlanningError<R::Error, C::Error>;

    async fn plan(
        &self,
        project: &OpenedProject,
        profile: &Self::Profile,
        corpus: StandardTranslationCorpus,
        input: StandardTranslationInput,
    ) -> Result<StandardTranslationPlan, Self::Error> {
        let planning = profile.planning();
        let ResolvedCorpusSemantics {
            groups,
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
            .execute(move || prepare_corpus(groups))
            .await
            .map_err(RpgMakerStandardTranslationTaskPlanningError::PrepareCorpusCompute)?
            .map_err(RpgMakerStandardTranslationTaskPlanningError::InvalidCorpus)?;

        let target_user_message_characters = planning.target_user_message_characters().get();
        let scope_semantics = Arc::clone(&semantics);
        let preprocessed_scopes = self
            .cpu
            .execute_ordered_map(prepared.scopes, move |scope| {
                let scope_name = scope.key.clone();
                (
                    scope_name,
                    preprocess_scope(scope, Arc::clone(&scope_semantics)),
                )
            })
            .await
            .map_err(RpgMakerStandardTranslationTaskPlanningError::PreprocessScopesCompute)?;

        let (scopes, invalidations, reuses, planning_failures, preparation_counts) = self
            .cpu
            .execute(move || {
                let mut scopes = Vec::with_capacity(preprocessed_scopes.len());
                let mut planning_failures = Vec::new();
                let mut preprocessing_invalidations = Vec::new();
                let mut preprocessing_invalidated = 0;
                for (scope, result) in preprocessed_scopes {
                    let result = result.map_err(|source| {
                        GlobalPreparationFailure::InvalidScopePreprocessing { scope, source }
                    })?;
                    planning_failures.extend(result.planning_failures);
                    preprocessing_invalidations.extend(result.invalidations);
                    preprocessing_invalidated += result.invalidated;
                    scopes.push(result.scope);
                }
                let (candidates, positions, mut invalidations) =
                    collect_deduplication_inputs(&scopes);
                invalidations.extend(preprocessing_invalidations);
                let deduplicated = deduplicate_translation_candidates(candidates)
                    .map_err(GlobalPreparationFailure::InvalidDeduplication)?;
                let (outcomes, deduplication_invalidations, reuses) = deduplicated.into_parts();
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
                    .count()
                    + preprocessing_invalidated;

                Ok::<_, GlobalPreparationFailure>((
                    scopes,
                    invalidations,
                    reuses,
                    planning_failures,
                    TranslationPlanPreparationCounts::new(retained, invalidated, not_applicable),
                ))
            })
            .await
            .map_err(RpgMakerStandardTranslationTaskPlanningError::DeduplicateCompute)?
            .map_err(|failure| match failure {
                GlobalPreparationFailure::InvalidScopePreprocessing { scope, source } => {
                    RpgMakerStandardTranslationTaskPlanningError::InvalidScopePreprocessing {
                        scope,
                        source,
                    }
                }
                GlobalPreparationFailure::InvalidDeduplication(source) => {
                    RpgMakerStandardTranslationTaskPlanningError::InvalidDeduplication(source)
                }
            })?;

        // 术语提示词行只渲染一次。后续每个 Scope/Task 只按实际命中的稀疏下标取用，
        // 不能再为了少数命中反复扫描整份术语表。
        let terminology_prompt = Arc::new(TerminologyPromptIndex::new(&terminology));
        let scope_terminology_prompt = Arc::clone(&terminology_prompt);
        let scope_system_markdown = Arc::new(system_markdown.clone());
        let planned_scopes = self
            .cpu
            .execute_ordered_map(scopes, move |scope| {
                build_scope_tasks(
                    scope,
                    Arc::clone(&scope_terminology_prompt),
                    scope_system_markdown.as_str(),
                    target_user_message_characters,
                )
            })
            .await
            .map_err(RpgMakerStandardTranslationTaskPlanningError::PlanScopesCompute)?;

        let tasks = self
            .cpu
            .execute(move || {
                planned_scopes
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .map(|(index, task)| {
                        task.with_index(
                            StandardTranslationTaskIndex::new(index),
                            task_language_pair.clone(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .map_err(RpgMakerStandardTranslationTaskPlanningError::FinalizePlanCompute)?;

        Ok(StandardTranslationPlan::new(
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

impl PreparedCorpus {
    pub(super) fn into_scopes(self) -> Vec<PreparedScope> {
        self.scopes
    }
}

pub(super) struct PreparedScope {
    key: SemanticScopeKey,
    groups: Vec<PreparedGroup>,
}

impl PreparedScope {
    pub(super) fn into_groups(self) -> Vec<PreparedGroup> {
        self.groups
    }
}

pub(super) struct PreparedGroup {
    kind: TextGroupKind,
    assets: Vec<PreparedAsset>,
}

impl PreparedGroup {
    pub(super) fn into_parts(self) -> (TextGroupKind, Vec<PreparedAsset>) {
        (self.kind, self.assets)
    }
}

pub(super) struct PreparedAsset {
    identity: TranslationUnitIdentity,
    translation: Option<TextUnitContent>,
    translation_state: Option<Sha256Fingerprint>,
}

impl PreparedAsset {
    pub(super) fn into_parts(
        self,
    ) -> (
        TranslationUnitIdentity,
        Option<TextUnitContent>,
        Option<Sha256Fingerprint>,
    ) {
        (self.identity, self.translation, self.translation_state)
    }
}

pub(super) fn prepare_corpus(
    groups: Vec<StandardTranslationGroup>,
) -> Result<PreparedCorpus, CorpusPlanningError> {
    let mut scopes = Vec::<PreparedScope>::new();
    for group in groups {
        let scope = SemanticScopeKey::from_group(&group)?;
        let kind = group.kind();
        let mut assets = Vec::new();
        for asset in group.into_assets() {
            let (identity, translation, translation_state) = asset.into_parts();
            assets.push(PreparedAsset {
                identity,
                translation,
                translation_state,
            });
        }
        let prepared_group = PreparedGroup { kind, assets };
        if let Some(current_scope) = scopes.last_mut()
            && current_scope.key == scope
        {
            current_scope.groups.push(prepared_group);
        } else {
            scopes.push(PreparedScope {
                key: scope,
                groups: vec![prepared_group],
            });
        }
    }

    Ok(PreparedCorpus { scopes })
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum SemanticScopeKey {
    StandardDatabase(StandardDataFile),
    DataFile(DataFileName),
    System,
    Map(MapId),
    CommonEvent(usize),
    Troop(usize),
    Plugin {
        plugin_index: usize,
        plugin_name: String,
    },
}

impl SemanticScopeKey {
    fn from_group(group: &StandardTranslationGroup) -> Result<Self, CorpusPlanningError> {
        match group.group_location().source() {
            RpgMakerSource::Data(StandardDataFile::System) => Ok(Self::System),
            RpgMakerSource::Data(StandardDataFile::CommonEvents) => {
                first_array_index(group).map(Self::CommonEvent)
            }
            RpgMakerSource::Data(StandardDataFile::Troops) => {
                first_array_index(group).map(Self::Troop)
            }
            RpgMakerSource::Data(file) => Ok(Self::StandardDatabase(*file)),
            RpgMakerSource::DataFile(file) => Ok(Self::DataFile(file.clone())),
            RpgMakerSource::Map(map_id) => Ok(Self::Map(*map_id)),
            RpgMakerSource::PluginParameter {
                plugin_index,
                plugin_name,
                ..
            } => Ok(Self::Plugin {
                plugin_index: *plugin_index,
                plugin_name: plugin_name.clone(),
            }),
        }
    }
}

impl fmt::Display for SemanticScopeKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StandardDatabase(file) => write!(formatter, "data/{}", file.file_name()),
            Self::DataFile(file) => write!(formatter, "data/{file}"),
            Self::System => formatter.write_str("data/System.json"),
            Self::Map(map_id) => write!(formatter, "Map{:03}", map_id.get()),
            Self::CommonEvent(event_id) => write!(formatter, "CommonEvent[{event_id}]"),
            Self::Troop(troop_id) => write!(formatter, "Troop[{troop_id}]"),
            Self::Plugin { plugin_name, .. } => write!(formatter, "Plugin[{plugin_name}]"),
        }
    }
}

fn first_array_index(group: &StandardTranslationGroup) -> Result<usize, CorpusPlanningError> {
    group
        .group_location()
        .steps()
        .iter()
        .find_map(|step| match step {
            RpgMakerLocationStep::ArrayIndex(index) => Some(*index),
            _ => None,
        })
        .ok_or_else(|| CorpusPlanningError::MissingSemanticIndex {
            location: group.group_location().to_string(),
        })
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
    units: Vec<PreprocessedUnit>,
}

struct PreprocessedUnit {
    identity: TranslationUnitIdentity,
    protected_text: String,
    placeholders: Vec<super::standard::AppliedPlaceholder>,
    language_analysis: LanguageAnalysis,
    triggered_terms: Vec<usize>,
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
) -> Result<PreprocessedScopeResult, ScopePreprocessingError> {
    let mut groups = Vec::with_capacity(scope.groups.len());
    let mut planning_failures = Vec::new();
    let mut invalidations = Vec::new();
    let mut invalidated = 0;
    for group in scope.groups {
        let mut units = Vec::with_capacity(group.assets.len());
        for asset in group.assets {
            let prepared = match semantics
                .prepare_content(group.kind, asset.identity.source_content())
            {
                Ok(prepared) => prepared,
                Err(source) => {
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
                    continue;
                }
            };
            let protected_text = prepared.model_text().to_owned();
            let placeholders = prepared.placeholders().to_vec();
            let language_analysis = prepared.language_analysis().clone();
            let triggered_terms = prepared.term_indices().to_vec();
            let terminology_dependencies = prepared.terms().to_vec();
            let state_context = translation_state_context(
                semantics.global_fingerprint(),
                &asset.identity,
                &protected_text,
                &placeholders,
                &terminology_dependencies,
            )?;
            let not_applicable = prepared.status() != PreparedTranslationStatus::Active;
            let current = if not_applicable {
                false
            } else if let Some(translation) = asset.translation.as_ref() {
                asset.translation_state == Some(state_context.finish(translation))
            } else {
                false
            };
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
                triggered_terms,
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
        ResolvedTranslationSemanticError::AcceptCandidate(_) => {
            unreachable!("译前准备不会执行候选译文验收")
        }
    }
}

fn placeholder_protection_failure_detail(source: &PlaceholderProtectionError) -> String {
    match source {
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

fn semantic_text(content: &TextUnitContent) -> String {
    match content {
        TextUnitContent::Value(value) => value.clone(),
        TextUnitContent::Lines(lines) => lines.join("\n"),
    }
}

pub(crate) fn global_translation_semantics(
    engine: RpgMakerEngine,
    source_language: &str,
    target_language: &str,
    language_semantics: Sha256Fingerprint,
    system_markdown: &str,
    client_semantics: Sha256Fingerprint,
) -> Sha256Fingerprint {
    let mut hasher = Sha256FramedHasher::new(b"att.rpg_maker.translation-global");
    hasher
        .frame(1, source_language.as_bytes())
        .frame(2, target_language.as_bytes())
        .frame(3, language_semantics.as_bytes())
        .frame(4, system_markdown.as_bytes())
        .frame(5, client_semantics.as_bytes())
        .frame(6, engine.storage_name().as_bytes());
    hasher.finish()
}

pub(crate) fn translation_state_context(
    global_semantics: Sha256Fingerprint,
    identity: &TranslationUnitIdentity,
    protected_text: &str,
    placeholders: &[super::standard::AppliedPlaceholder],
    terminology: &[TerminologyDependency],
) -> Result<TranslationStateContext, ScopePreprocessingError> {
    let group_location = RpgMakerLocationCodec::encode(identity.group_location())
        .map_err(ScopePreprocessingError::EncodeStateLocation)?;
    let role = RpgMakerProjectionCodec::encode_role(identity.role())
        .map_err(ScopePreprocessingError::EncodeStateRole)?;
    let mut hasher = Sha256FramedHasher::new(b"att.rpg_maker.translation-unit-context");
    hasher
        .frame(1, global_semantics.as_bytes())
        .frame(2, identity.owner().storage_name().as_bytes())
        .frame(3, group_kind_name(identity.kind()))
        .frame(4, group_location.as_bytes())
        .frame(5, role.as_bytes())
        .frame(6, identity.source_context_json().as_bytes());
    match identity.source_content() {
        TextUnitContent::Value(value) => {
            hasher.frame(7, b"value").frame(8, value.as_bytes());
        }
        TextUnitContent::Lines(lines) => {
            let count = u64::try_from(lines.len())
                .expect("源行数必须能表示为 u64")
                .to_le_bytes();
            hasher.frame(7, b"lines").frame(8, &count);
            for line in lines {
                hasher.frame(9, line.as_bytes());
            }
        }
    }
    hasher.frame(10, protected_text.as_bytes());
    for placeholder in placeholders {
        let origin = match placeholder.origin() {
            super::standard::PlaceholderRuleOrigin::BuiltIn => b"builtin".as_slice(),
            super::standard::PlaceholderRuleOrigin::Custom => b"custom".as_slice(),
        };
        let segment = match placeholder.segment() {
            super::standard::PlaceholderSegment::Whole => b"whole".as_slice(),
            super::standard::PlaceholderSegment::Begin => b"begin".as_slice(),
            super::standard::PlaceholderSegment::End => b"end".as_slice(),
        };
        hasher
            .frame(20, placeholder.token().as_bytes())
            .frame(21, placeholder.original().as_bytes())
            .frame(22, origin)
            .frame(23, placeholder.label().as_bytes())
            .frame(24, placeholder.scope().as_bytes())
            .frame(25, segment);
    }
    for dependency in terminology {
        hasher
            .frame(30, dependency.term().as_bytes())
            .frame(31, dependency.translation().as_bytes());
    }
    Ok(TranslationStateContext::new(hasher.finish()))
}

type UnitPosition = (usize, usize, usize);

fn collect_deduplication_inputs(
    scopes: &[PreprocessedScope],
) -> (
    Vec<TranslationDeduplicationCandidate>,
    Vec<UnitPosition>,
    Vec<TranslationInvalidation>,
) {
    let mut candidates = Vec::new();
    let mut positions = Vec::new();
    let mut invalidations = Vec::new();
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
            }
        }
    }
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

#[derive(Clone)]
struct PreparedTaskGroup {
    kind: TextGroupKind,
    units: Vec<PreparedUnit>,
    triggered_terms: Vec<usize>,
}

#[derive(Clone)]
struct PreparedUnit {
    field_name: String,
    identity: TranslationUnitIdentity,
    protected_text: String,
    translation: Option<TextUnitContent>,
    placeholders: Vec<super::standard::AppliedPlaceholder>,
    language_analysis: LanguageAnalysis,
    triggered_terms: Vec<usize>,
    state_context: TranslationStateContext,
    responsibility: PreparedUnitResponsibility,
}

fn build_scope_tasks(
    scope: PreprocessedScope,
    terminology: Arc<TerminologyPromptIndex>,
    system_markdown: &str,
    target_user_message_characters: usize,
) -> Vec<UnindexedTask> {
    let mut prepared_groups = Vec::with_capacity(scope.groups.len());
    for group in scope.groups {
        let units = group
            .units
            .into_iter()
            .map(|unit| PreparedUnit {
                field_name: unit.identity.role_label(),
                identity: unit.identity,
                protected_text: unit.protected_text,
                translation: unit.translation,
                placeholders: unit.placeholders,
                language_analysis: unit.language_analysis,
                triggered_terms: unit.triggered_terms,
                state_context: unit.state_context,
                responsibility: unit.responsibility,
            })
            .collect::<Vec<_>>();
        let mut triggered_terms = BTreeSet::new();
        for unit in units.iter().filter(|unit| {
            matches!(
                unit.responsibility,
                PreparedUnitResponsibility::Active { .. }
            )
        }) {
            triggered_terms.extend(unit.triggered_terms.iter().copied());
        }
        prepared_groups.push(PreparedTaskGroup {
            kind: group.kind,
            units,
            triggered_terms: triggered_terms.into_iter().collect(),
        });
    }

    pack_scope(
        prepared_groups,
        &terminology,
        system_markdown,
        target_user_message_characters,
    )
}

/// 已经完成语义筛选、但尚未分配任务内 ID 的组。
///
/// Markdown 中只保存 ID 插入位置，切块阶段据此精确计算最终 user message 字符数；
/// 最终任务确定后才渲染一次，从而避免任务边界上的整组克隆和重复字符串构造。
struct PackedGroup {
    markdown_template: String,
    task_id_offsets: Vec<usize>,
    markdown_characters_without_ids: usize,
    expected: Vec<ExpectedBase>,
    triggered_terms: Vec<usize>,
}

impl PackedGroup {
    fn active_count(&self) -> usize {
        self.task_id_offsets.len()
    }

    fn markdown_characters(&self, first_active_id: usize) -> usize {
        let id_characters = (0..self.active_count()).fold(0usize, |characters, offset| {
            characters.saturating_add(decimal_characters(first_active_id.saturating_add(offset)))
        });
        self.markdown_characters_without_ids
            .saturating_add(id_characters)
    }
}

struct RenderedGroup {
    markdown: String,
    expected: Vec<ExpectedBase>,
}

struct ExpectedBase {
    id: usize,
    line_shape: ExpectedLineShape,
    identity: TranslationUnitIdentity,
    propagation_targets: Vec<TranslationPropagationTarget>,
    protected_text: String,
    placeholders: Vec<super::standard::AppliedPlaceholder>,
    language_analysis: LanguageAnalysis,
    state_context: TranslationStateContext,
}

fn prepare_group(seed: PreparedTaskGroup) -> PackedGroup {
    let mut expected = Vec::new();
    let active_count = seed
        .units
        .iter()
        .filter(|unit| is_active_unit(unit))
        .count();
    if active_count == 0 {
        return PackedGroup {
            markdown_template: String::new(),
            task_id_offsets: Vec::new(),
            markdown_characters_without_ids: 0,
            expected,
            triggered_terms: seed.triggered_terms,
        };
    }

    let mut markdown = format!("## {}\n", human_group_kind(seed.kind));
    let mut task_id_offsets = Vec::with_capacity(active_count);
    let active_dialogue_body = seed.units.iter().any(|unit| {
        is_active_unit(unit) && matches!(unit.identity.role(), TextUnitRole::DialogueBody)
    });
    let active_database_value = seed.units.iter().any(|unit| {
        is_active_unit(unit)
            && matches!(
                unit.identity.role(),
                TextUnitRole::Scalar(key) if key.as_str() != "name"
            )
    });

    for unit in &seed.units {
        if is_useful_context(unit, seed.kind, active_dialogue_body, active_database_value) {
            markdown.push('\n');
            markdown.push_str(human_context_label(unit.identity.role()));
            markdown.push(':');
            markdown.push_str(&context_text(unit));
            markdown.push('\n');
        }
    }

    for unit in seed.units {
        if is_active_unit(&unit) {
            markdown.push('\n');
            render_active_unit_template(&mut markdown, &mut task_id_offsets, &unit);
            let line_shape = expected_line_shape(&unit.identity);
            let PreparedUnitResponsibility::Active {
                propagation_targets,
            } = unit.responsibility
            else {
                unreachable!("已确认的活跃单元必须携带传播目标")
            };
            expected.push(ExpectedBase {
                id: 0,
                line_shape,
                identity: unit.identity,
                propagation_targets,
                protected_text: unit.protected_text,
                placeholders: unit.placeholders,
                language_analysis: unit.language_analysis,
                state_context: unit.state_context,
            });
        } else if matches!(
            unit.responsibility,
            PreparedUnitResponsibility::AwaitingDeduplication
        ) {
            unreachable!("任务切块前必须完成全局去重")
        }
    }

    let markdown_characters_without_ids = markdown.chars().count();
    PackedGroup {
        markdown_template: markdown,
        task_id_offsets,
        markdown_characters_without_ids,
        expected,
        triggered_terms: seed.triggered_terms,
    }
}

fn render_group(group: PackedGroup, first_active_id: usize) -> RenderedGroup {
    let final_characters = group.markdown_characters(first_active_id);
    let mut markdown =
        String::with_capacity(group.markdown_template.len().saturating_add(
            final_characters.saturating_sub(group.markdown_characters_without_ids),
        ));
    let mut template_offset = 0usize;
    for (task_offset, id_offset) in group.task_id_offsets.into_iter().enumerate() {
        markdown.push_str(&group.markdown_template[template_offset..id_offset]);
        markdown.push_str(&first_active_id.saturating_add(task_offset).to_string());
        template_offset = id_offset;
    }
    markdown.push_str(&group.markdown_template[template_offset..]);

    let expected = group
        .expected
        .into_iter()
        .enumerate()
        .map(|(offset, mut expected)| {
            expected.id = first_active_id.saturating_add(offset);
            expected
        })
        .collect();
    RenderedGroup { markdown, expected }
}

fn decimal_characters(mut value: usize) -> usize {
    let mut characters = 1usize;
    while value >= 10 {
        value /= 10;
        characters += 1;
    }
    characters
}

fn is_active_unit(unit: &PreparedUnit) -> bool {
    matches!(
        unit.responsibility,
        PreparedUnitResponsibility::Active { .. }
    )
}

fn is_useful_context(
    unit: &PreparedUnit,
    kind: TextGroupKind,
    active_dialogue_body: bool,
    active_database_value: bool,
) -> bool {
    if !matches!(
        unit.responsibility,
        PreparedUnitResponsibility::Virtual { .. }
    ) {
        return false;
    }
    (kind == TextGroupKind::EventDialogue
        && active_dialogue_body
        && matches!(unit.identity.role(), TextUnitRole::DialogueSpeaker))
        || (kind == TextGroupKind::DatabaseEntry
            && active_database_value
            && matches!(
                unit.identity.role(),
                TextUnitRole::Scalar(key) if key.as_str() == "name"
            ))
}

fn context_text(unit: &PreparedUnit) -> String {
    match &unit.responsibility {
        PreparedUnitResponsibility::Virtual {
            reason: TranslationVirtualReason::ExistingTranslation,
        } => unit
            .translation
            .as_ref()
            .map(semantic_text)
            .unwrap_or_else(|| semantic_text(unit.identity.source_content())),
        PreparedUnitResponsibility::Virtual {
            reason: TranslationVirtualReason::Reused { translation, .. },
        } => semantic_text(translation),
        PreparedUnitResponsibility::Virtual { .. }
        | PreparedUnitResponsibility::AwaitingDeduplication
        | PreparedUnitResponsibility::Active { .. } => {
            semantic_text(unit.identity.source_content())
        }
    }
}

fn human_context_label(role: &TextUnitRole) -> &'static str {
    match role {
        TextUnitRole::DialogueSpeaker => "Speaker",
        TextUnitRole::Scalar(_) => "Name",
        TextUnitRole::DialogueBody | TextUnitRole::Choices | TextUnitRole::ScrollingText => {
            unreachable!("只有说话人和数据库名称可以作为无编号语境")
        }
    }
}

fn render_active_unit_template(
    markdown: &mut String,
    task_id_offsets: &mut Vec<usize>,
    unit: &PreparedUnit,
) {
    match unit.identity.role() {
        TextUnitRole::DialogueSpeaker => {
            markdown.push_str("Speaker [");
            task_id_offsets.push(markdown.len());
            markdown.push_str("] (single line):");
            markdown.push_str(&unit.protected_text);
            markdown.push('\n');
        }
        TextUnitRole::DialogueBody => {
            markdown.push_str("Body [");
            task_id_offsets.push(markdown.len());
            markdown.push_str("] (free line breaking):\n\n");
            push_blockquote(markdown, &unit.protected_text);
        }
        TextUnitRole::Choices => {
            let count = source_line_count(&unit.identity);
            markdown.push_str("Choices [");
            task_id_offsets.push(markdown.len());
            markdown.push_str("] (");
            markdown.push_str(&count.to_string());
            markdown.push_str(" items, corresponding item by item):\n\n");
            push_blockquote(markdown, &unit.protected_text);
        }
        TextUnitRole::ScrollingText => {
            let count = source_line_count(&unit.identity);
            markdown.push_str("Scrolling Text [");
            task_id_offsets.push(markdown.len());
            markdown.push_str("] (");
            markdown.push_str(&count.to_string());
            markdown.push_str(" lines, corresponding line by line):\n\n");
            push_blockquote(markdown, &unit.protected_text);
        }
        TextUnitRole::Scalar(_)
            if expected_line_shape(&unit.identity) == ExpectedLineShape::Reflow =>
        {
            markdown.push_str(human_scalar_label(&unit.field_name));
            markdown.push_str(" [");
            task_id_offsets.push(markdown.len());
            markdown.push_str("] (free line breaking):\n\n");
            push_blockquote(markdown, &unit.protected_text);
        }
        TextUnitRole::Scalar(_) => {
            markdown.push_str(human_scalar_label(&unit.field_name));
            markdown.push_str(" [");
            task_id_offsets.push(markdown.len());
            markdown.push_str("] (single line):");
            markdown.push_str(&unit.protected_text);
            markdown.push('\n');
        }
    }
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

pub(crate) fn expected_line_shape(identity: &TranslationUnitIdentity) -> ExpectedLineShape {
    match identity.role() {
        TextUnitRole::DialogueBody => ExpectedLineShape::Reflow,
        TextUnitRole::Choices | TextUnitRole::ScrollingText => ExpectedLineShape::Aligned(
            NonZeroUsize::new(source_line_count(identity))
                .expect("选项与滚动文本必须至少包含一个语义槽"),
        ),
        TextUnitRole::DialogueSpeaker => ExpectedLineShape::Aligned(NonZeroUsize::MIN),
        TextUnitRole::Scalar(key) if scalar_allows_reflow(identity, key.as_str()) => {
            ExpectedLineShape::Reflow
        }
        TextUnitRole::Scalar(_) => ExpectedLineShape::Aligned(NonZeroUsize::MIN),
    }
}

fn source_line_count(identity: &TranslationUnitIdentity) -> usize {
    identity
        .source_content()
        .as_lines()
        .expect("复合行角色必须保存完整行序列")
        .len()
}

fn scalar_allows_reflow(identity: &TranslationUnitIdentity, field_name: &str) -> bool {
    let value = identity
        .source_content()
        .as_value()
        .expect("Scalar 角色必须保存单个 Value");
    if value.contains('\n') {
        return true;
    }
    matches!(
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
    )
}

fn push_blockquote(markdown: &mut String, text: &str) {
    for line in text.split('\n') {
        markdown.push_str("> ");
        markdown.push_str(line);
        markdown.push('\n');
    }
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
    characters: usize,
}

impl TerminologyPromptIndex {
    fn new(terminology: &CompiledTerminology) -> Self {
        let lines = terminology
            .entries()
            .iter()
            .map(|entry| {
                let mut markdown = String::new();
                markdown.push_str("- ");
                push_markdown_literal(&mut markdown, entry.term());
                markdown.push_str(" → ");
                push_markdown_literal(&mut markdown, entry.translation());
                markdown.push('\n');
                let characters = markdown.chars().count();
                TerminologyPromptLine {
                    markdown,
                    characters,
                }
            })
            .collect();
        Self { lines }
    }

    fn line_characters(&self, index: usize) -> usize {
        self.lines[index].characters
    }

    /// 追加自然有序的稀疏命中，并返回实际访问的术语行数供复杂度测试观察。
    fn append_selected(&self, markdown: &mut String, selected: &BTreeSet<usize>) -> usize {
        for &index in selected {
            markdown.push_str(&self.lines[index].markdown);
        }
        selected.len()
    }
}

fn pack_scope(
    groups: Vec<PreparedTaskGroup>,
    terminology: &TerminologyPromptIndex,
    system_markdown: &str,
    target_user_message_characters: usize,
) -> Vec<UnindexedTask> {
    let mut tasks = Vec::new();
    let mut current_groups = Vec::<PackedGroup>::new();
    let mut current_active = 0usize;
    let mut current_terms = BTreeSet::new();
    let mut current_user_message_characters = 0usize;

    for seed in groups {
        let packed = prepare_group(seed);
        if packed.active_count() == 0 {
            continue;
        }
        let (additional_term_count, additional_term_characters) =
            additional_terminology_size(&packed.triggered_terms, &current_terms, terminology);
        let candidate_user_message_characters = candidate_user_message_character_count(
            current_user_message_characters,
            !current_groups.is_empty(),
            current_terms.len(),
            additional_term_count,
            additional_term_characters,
            packed.markdown_characters(current_active + 1),
        );
        if candidate_user_message_characters <= target_user_message_characters {
            current_active += packed.active_count();
            current_terms.extend(packed.triggered_terms.iter().copied());
            current_user_message_characters = candidate_user_message_characters;
            current_groups.push(packed);
            continue;
        }

        if current_active > 0 {
            tasks.push(finalize_task(
                std::mem::take(&mut current_groups),
                terminology,
                &current_terms,
                system_markdown,
            ));
        } else {
            current_groups.clear();
        }
        current_terms.clear();

        let (term_count, term_characters) =
            additional_terminology_size(&packed.triggered_terms, &current_terms, terminology);
        let group_user_message_characters = candidate_user_message_character_count(
            0,
            false,
            0,
            term_count,
            term_characters,
            packed.markdown_characters(1),
        );
        current_active = packed.active_count();
        current_terms.extend(packed.triggered_terms.iter().copied());
        current_user_message_characters = group_user_message_characters;
        current_groups.push(packed);
        if current_user_message_characters > target_user_message_characters {
            // 完整语义组可以自然超过装箱目标。立即完成可确保它独占任务，并让下一组
            // 继续使用原目标；不能把本次实际长度反向提升为后续任务的新目标。
            tasks.push(finalize_task(
                std::mem::take(&mut current_groups),
                terminology,
                &current_terms,
                system_markdown,
            ));
            current_active = 0;
            current_terms.clear();
            current_user_message_characters = 0;
        }
    }

    if current_active > 0 {
        tasks.push(finalize_task(
            current_groups,
            terminology,
            &current_terms,
            system_markdown,
        ));
    }
    tasks
}

fn additional_terminology_size(
    triggered_terms: &[usize],
    current_terms: &BTreeSet<usize>,
    terminology: &TerminologyPromptIndex,
) -> (usize, usize) {
    let mut count = 0usize;
    let mut characters = 0usize;
    for &index in triggered_terms {
        if !current_terms.contains(&index) {
            count += 1;
            characters = characters.saturating_add(terminology.line_characters(index));
        }
    }
    (count, characters)
}

fn candidate_user_message_character_count(
    current_characters: usize,
    has_groups: bool,
    current_term_count: usize,
    additional_term_count: usize,
    additional_term_characters: usize,
    group_characters: usize,
) -> usize {
    let mut characters = current_characters;
    if additional_term_count > 0 {
        if current_term_count == 0 {
            characters = characters.saturating_add("Terminology:\n\n".chars().count());
            if has_groups {
                // 术语区位于既有组之前，首次加入时会多出一个区段分隔换行。
                characters = characters.saturating_add(1);
            }
        }
        characters = characters.saturating_add(additional_term_characters);
    }
    if characters > 0 {
        characters = characters.saturating_add(1);
    }
    characters.saturating_add(group_characters)
}

#[cfg(test)]
fn terminology_line_character_count(entry: &super::planning_resource::TerminologyEntry) -> usize {
    "- ".chars().count()
        + markdown_literal_character_count(entry.term())
        + " → ".chars().count()
        + markdown_literal_character_count(entry.translation())
        + 1
}

#[cfg(test)]
fn markdown_literal_character_count(value: &str) -> usize {
    value
        .chars()
        .map(|character| usize::from(character.is_ascii_punctuation()) + 1)
        .sum()
}

fn render_user_markdown(
    groups: &[RenderedGroup],
    additional_group: Option<&RenderedGroup>,
    terminology: &TerminologyPromptIndex,
    selected_terms: &BTreeSet<usize>,
) -> String {
    let mut markdown = String::new();
    if !selected_terms.is_empty() {
        markdown.push_str("Terminology:\n\n");
        terminology.append_selected(&mut markdown, selected_terms);
    }
    for group in groups {
        if !markdown.is_empty() {
            markdown.push('\n');
        }
        markdown.push_str(&group.markdown);
    }
    if let Some(group) = additional_group {
        if !markdown.is_empty() {
            markdown.push('\n');
        }
        markdown.push_str(&group.markdown);
    }
    markdown
}

fn push_markdown_literal(markdown: &mut String, value: &str) {
    for character in value.chars() {
        if character.is_ascii_punctuation() {
            markdown.push('\\');
        }
        markdown.push(character);
    }
}

fn finalize_task(
    groups: Vec<PackedGroup>,
    terminology: &TerminologyPromptIndex,
    selected_terms: &BTreeSet<usize>,
    system_markdown: &str,
) -> UnindexedTask {
    let mut next_active_id = 1usize;
    let groups = groups
        .into_iter()
        .map(|group| {
            let active_count = group.active_count();
            let rendered = render_group(group, next_active_id);
            next_active_id = next_active_id.saturating_add(active_count);
            rendered
        })
        .collect::<Vec<_>>();
    let user_markdown = render_user_markdown(&groups, None, terminology, selected_terms);
    let mut expected_outputs = Vec::new();
    for group in groups {
        expected_outputs.extend(group.expected.into_iter().map(|expected| {
            let (propagation_targets, propagation_state_contexts) = expected
                .propagation_targets
                .into_iter()
                .map(|target| (target.identity().clone(), target.state_context()))
                .unzip();
            ExpectedTranslationOutput::new(
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
            )
        }));
    }
    UnindexedTask {
        messages: vec![
            ChatMessage::new(ChatMessageRole::System, system_markdown),
            ChatMessage::new(ChatMessageRole::User, user_markdown),
        ],
        expected_outputs,
    }
}

struct UnindexedTask {
    messages: Vec<ChatMessage>,
    expected_outputs: Vec<ExpectedTranslationOutput>,
}

impl UnindexedTask {
    fn with_index(
        self,
        index: StandardTranslationTaskIndex,
        language_pair: LanguagePair,
    ) -> TranslationTaskBlock {
        TranslationTaskBlock::new(index, language_pair, self.messages, self.expected_outputs)
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
    InvalidScopePreprocessing {
        scope: SemanticScopeKey,
        source: ScopePreprocessingError,
    },
    InvalidDeduplication(TranslationDeduplicationError),
}

#[derive(Debug)]
pub(crate) enum OpenStandardCandidateSessionError<R, C> {
    Planning(RpgMakerStandardTranslationTaskPlanningError<R, C>),
    ScheduleBuild(CpuTaskExecutionError<C>),
    Build(super::candidate::StandardCandidateSessionBuildError),
}

impl<R: fmt::Display, C: fmt::Display> fmt::Display for OpenStandardCandidateSessionError<R, C> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Planning(source) => write!(formatter, "无法解析 Standard 人工候选语义：{source}"),
            Self::ScheduleBuild(source) => {
                write!(formatter, "无法调度 Standard 人工候选会话构建：{source}")
            }
            Self::Build(source) => write!(formatter, "无法建立 Standard 人工候选会话：{source}"),
        }
    }
}

impl<R: Error + 'static, C: Error + 'static> Error for OpenStandardCandidateSessionError<R, C> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Planning(source) => Some(source),
            Self::ScheduleBuild(source) => Some(source),
            Self::Build(source) => Some(source),
        }
    }
}

impl<R, C> SafeDiagnosticSource for OpenStandardCandidateSessionError<R, C>
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
            Self::Planning(source) => {
                source.safe_diagnostic_source(stage, impact, DiagnosticAction::FixInput)
            }
            Self::ScheduleBuild(source) => {
                source.safe_diagnostic_source(stage, impact, DiagnosticAction::Retry)
            }
            Self::Build(source) => SafeDiagnostic::new(
                DiagnosticCode::InternalOperation,
                stage,
                DiagnosticSubject::component("standard_candidate_session"),
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

#[derive(Debug)]
pub(crate) enum RpgMakerStandardTranslationTaskPlanningError<R, C> {
    ResolvedLanguagePairMismatch {
        project_source: String,
        project_target: String,
        resolved_source: String,
        resolved_target: String,
    },
    ReadResources(R),
    CompilePlaceholdersCompute(CpuTaskExecutionError<C>),
    InvalidPlaceholderRules(PlaceholderRuleCompilationError),
    PrepareCorpusCompute(CpuTaskExecutionError<C>),
    InvalidCorpus(CorpusPlanningError),
    PreprocessScopesCompute(CpuTaskExecutionError<C>),
    InvalidScopePreprocessing {
        scope: SemanticScopeKey,
        source: ScopePreprocessingError,
    },
    DeduplicateCompute(CpuTaskExecutionError<C>),
    InvalidDeduplication(TranslationDeduplicationError),
    PlanScopesCompute(CpuTaskExecutionError<C>),
    FinalizePlanCompute(CpuTaskExecutionError<C>),
}

impl<R: fmt::Display, C: fmt::Display> fmt::Display
    for RpgMakerStandardTranslationTaskPlanningError<R, C>
{
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
            Self::CompilePlaceholdersCompute(source) => {
                write!(formatter, "无法调度占位符规则编译：{source}")
            }
            Self::InvalidPlaceholderRules(source) => write!(formatter, "占位符规则无效：{source}"),
            Self::PrepareCorpusCompute(source) => {
                write!(formatter, "无法调度标准语料排序：{source}")
            }
            Self::InvalidCorpus(source) => write!(formatter, "标准语料无法建立语义范围：{source}"),
            Self::PreprocessScopesCompute(source) => {
                write!(formatter, "无法调度语义范围的并行译前处理：{source}")
            }
            Self::InvalidScopePreprocessing { scope, source } => {
                write!(formatter, "语义范围 {scope} 无法完成译前处理：{source}")
            }
            Self::DeduplicateCompute(source) => {
                write!(formatter, "无法调度标准语料全局去重：{source}")
            }
            Self::InvalidDeduplication(source) => {
                write!(formatter, "标准语料无法建立唯一翻译责任：{source}")
            }
            Self::PlanScopesCompute(source) => {
                write!(formatter, "无法调度语义范围的并行任务规划：{source}")
            }
            Self::FinalizePlanCompute(source) => {
                write!(formatter, "无法调度翻译任务最终编号：{source}")
            }
        }
    }
}

impl<R: Error + 'static, C: Error + 'static> Error
    for RpgMakerStandardTranslationTaskPlanningError<R, C>
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadResources(source) => Some(source),
            Self::CompilePlaceholdersCompute(source) => Some(source),
            Self::InvalidPlaceholderRules(source) => Some(source),
            Self::PrepareCorpusCompute(source) => Some(source),
            Self::InvalidCorpus(source) => Some(source),
            Self::PreprocessScopesCompute(source) => Some(source),
            Self::InvalidScopePreprocessing { source, .. } => Some(source),
            Self::DeduplicateCompute(source) => Some(source),
            Self::InvalidDeduplication(source) => Some(source),
            Self::PlanScopesCompute(source) => Some(source),
            Self::FinalizePlanCompute(source) => Some(source),
            Self::ResolvedLanguagePairMismatch { .. } => None,
        }
    }
}

impl<R, C> SafeDiagnosticSource for RpgMakerStandardTranslationTaskPlanningError<R, C>
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
            Self::CompilePlaceholdersCompute(source) => {
                planning_cpu_diagnostic(source, stage, impact, "compile_placeholder_rules", None)
            }
            Self::InvalidPlaceholderRules(source) => {
                placeholder_compilation_diagnostic(source, stage, impact)
            }
            Self::PrepareCorpusCompute(source) => {
                planning_cpu_diagnostic(source, stage, impact, "prepare_translation_corpus", None)
            }
            Self::InvalidCorpus(CorpusPlanningError::MissingSemanticIndex { location }) => {
                SafeDiagnostic::new(
                    DiagnosticCode::ProjectState,
                    stage,
                    DiagnosticSubject::operation("standard_translation_corpus"),
                    DiagnosticReason::failure_with_detail(
                        DiagnosticFailureKind::StateMismatch,
                        format!("missing_semantic_index; location={location}"),
                    ),
                    impact,
                    DiagnosticAction::CheckProjectState,
                )
            }
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
            Self::InvalidDeduplication(
                TranslationDeduplicationError::ConflictingReusableTranslations {
                    conflicts, ..
                },
            ) => SafeDiagnostic::new(
                DiagnosticCode::ProjectState,
                stage,
                DiagnosticSubject::operation("global_translation_deduplication"),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::ConflictingValues,
                    format!("conflicting_reusable_translations={}", conflicts.len()),
                ),
                impact,
                DiagnosticAction::CheckProjectState,
            ),
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
    scope: Option<&SemanticScopeKey>,
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
        ScopePreprocessingError::EncodeStateLocation(source) => {
            format!(
                "encode_translation_state_location: {}",
                location_codec_detail(source)
            )
        }
        ScopePreprocessingError::EncodeStateRole(source) => format!(
            "encode_translation_state_role: {}",
            projection_codec_detail(source)
        ),
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

fn safe_scope_label(scope: &SemanticScopeKey) -> String {
    match scope {
        SemanticScopeKey::StandardDatabase(file) => format!("data/{}", file.file_name()),
        SemanticScopeKey::DataFile(file) => format!("data/{file}"),
        SemanticScopeKey::System => "data/System.json".to_owned(),
        SemanticScopeKey::Map(map_id) => format!("Map{:03}", map_id.get()),
        SemanticScopeKey::CommonEvent(event_id) => format!("CommonEvent[{event_id}]"),
        SemanticScopeKey::Troop(troop_id) => format!("Troop[{troop_id}]"),
        SemanticScopeKey::Plugin { plugin_index, .. } => {
            format!("Plugin[index={plugin_index}]")
        }
    }
}

fn usize_as_u64(value: usize) -> u64 {
    u64::try_from(value).expect("当前目标平台的 usize 必须可表示为 u64")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CorpusPlanningError {
    MissingSemanticIndex { location: String },
}

impl fmt::Display for CorpusPlanningError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSemanticIndex { location } => {
                write!(formatter, "CommonEvent/Troop 位置缺少对象索引：{location}")
            }
        }
    }
}

impl Error for CorpusPlanningError {}

#[derive(Debug)]
pub(crate) enum ScopePreprocessingError {
    EncodeStateLocation(crate::rpg_maker::location_codec::RpgMakerLocationCodecError),
    EncodeStateRole(crate::rpg_maker::location_codec::RpgMakerProjectionCodecError),
}

impl fmt::Display for ScopePreprocessingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EncodeStateLocation(source) => {
                write!(formatter, "无法编码译文状态位置：{source}")
            }
            Self::EncodeStateRole(source) => write!(formatter, "无法编码译文状态角色：{source}"),
        }
    }
}

impl Error for ScopePreprocessingError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::EncodeStateLocation(source) => Some(source),
            Self::EncodeStateRole(source) => Some(source),
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
    use crate::rpg_maker::standard_asset::RpgMakerStandardAssetOwner;
    use crate::rpg_maker::translate::planning_resource::{
        TranslationPlanningResourceReadingService, TranslationPlanningResources,
    };
    use crate::rpg_maker::translate::profile::{
        ResolvedRpgMakerTranslationResources, RpgMakerSystemPrompt,
        RpgMakerTranslationPlanningConfiguration, RpgMakerTranslationProfile,
        RpgMakerTranslationRequestConfiguration, TranslationResponseEnvelope,
    };
    use crate::rpg_maker::translate::standard::StandardTranslationAsset;

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
    type ProductionPlanningError = RpgMakerStandardTranslationTaskPlanningError<
        ProductionResourceError,
        CpuExecutorUnavailable,
    >;

    #[test]
    fn planning_diagnostic_preserves_resource_path_and_utf8_offset() {
        let invalid_bytes = vec![0xff];
        let invalid_utf8 = std::str::from_utf8(&invalid_bytes).expect_err("测试字节必须不是 UTF-8");
        let error: ProductionPlanningError =
            RpgMakerStandardTranslationTaskPlanningError::ReadResources(
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
            RpgMakerStandardTranslationTaskPlanningError::ReadResources(
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
            RpgMakerStandardTranslationTaskPlanningError::InvalidPlaceholderRules(
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
            RpgMakerStandardTranslationTaskPlanningError::PrepareCorpusCompute(
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
        assert_eq!(
            human_context_label(&TextUnitRole::DialogueSpeaker),
            "Speaker"
        );
        assert_eq!(
            human_context_label(&TextUnitRole::Scalar(
                ScalarFieldKey::new("name").expect("测试字段键应合法")
            )),
            "Name"
        );
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
            RpgMakerTranslationRequestConfiguration::new(Vec::new(), std::time::Duration::ZERO),
            Arc::new(()),
        ))
    }

    fn user_message(task: &TranslationTaskBlock) -> &str {
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
    ) -> StandardTranslationGroup {
        let original = original.into();
        let group_location = RpgMakerLocation::value(
            source.clone(),
            vec![RpgMakerLocationStep::index(object_index)],
        );
        let source_content = TextUnitContent::Value(original.clone());
        let identity = TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            group_location.clone(),
            TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应合法")),
            source_content,
            "{}",
        );
        let translation_state = translation.map(|translation| {
            translation_state_for(&identity, &original, translation, &terms, Vec::new())
        });
        StandardTranslationGroup::new(
            TextGroupKind::DatabaseEntry,
            group_location,
            vec![StandardTranslationAsset::new(
                identity,
                translation.map(|value| TextUnitContent::Value(value.to_owned())),
                translation_state,
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

    fn map_group(
        kind: TextGroupKind,
        event_index: Option<usize>,
        page_index: Option<usize>,
        command_index: Option<usize>,
        original: &str,
    ) -> StandardTranslationGroup {
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
            RpgMakerStandardAssetOwner::Builtin,
            kind,
            group_location.clone(),
            role,
            source_content,
            "{}",
        );
        StandardTranslationGroup::new(
            kind,
            group_location,
            vec![StandardTranslationAsset::new(identity, None, None)],
        )
    }

    fn map_unit_group(
        kind: TextGroupKind,
        command_index: usize,
        units: Vec<(TextUnitRole, TextUnitContent, &str)>,
    ) -> StandardTranslationGroup {
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
                StandardTranslationAsset::new(
                    TranslationUnitIdentity::new(
                        RpgMakerStandardAssetOwner::Builtin,
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
        StandardTranslationGroup::new(kind, group_location, assets)
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
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            group_location.clone(),
            TextUnitRole::DialogueBody,
            TextUnitContent::Lines(vec!["同一句".to_owned()]),
            r#"{"source_speaker":"甲"}"#,
        );
        let other_body = TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            group_location.clone(),
            TextUnitRole::DialogueBody,
            TextUnitContent::Lines(vec!["同一句".to_owned()]),
            r#"{"source_speaker":"乙"}"#,
        );
        let speaker = TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            group_location.clone(),
            TextUnitRole::DialogueSpeaker,
            TextUnitContent::Value("甲".to_owned()),
            "{}",
        );
        let prepared = prepare_corpus(vec![StandardTranslationGroup::new(
            TextGroupKind::EventDialogue,
            group_location,
            vec![
                StandardTranslationAsset::new(body.clone(), None, None),
                StandardTranslationAsset::new(speaker, None, None),
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
            SemanticScopeKey::StandardDatabase(StandardDataFile::Items)
        ));
        assert!(matches!(
            &prepared.scopes[1].key,
            SemanticScopeKey::StandardDatabase(StandardDataFile::Actors)
        ));
        assert!(matches!(
            &prepared.scopes[2].key,
            SemanticScopeKey::StandardDatabase(StandardDataFile::Items)
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
        let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, ()>::new(
            reader,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let old_dependency = TerminologyDependency::new("魔法剣", "魔法剑");
        let corpus = StandardTranslationCorpus::new(vec![group(
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
                StandardTranslationInput::new(Some(terminology_path), None),
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
        assert_eq!(tasks[0].expected_outputs()[0].id(), 1);
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
        let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, ()>::new(
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
                StandardTranslationCorpus::new(vec![group(
                    RpgMakerSource::data(StandardDataFile::Items),
                    1,
                    original,
                    None,
                    Vec::new(),
                )]),
                StandardTranslationInput::new(None, None),
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
                RpgMakerStandardAssetOwner::Builtin,
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
        let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let corpus = StandardTranslationCorpus::new(
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
                    RpgMakerStandardAssetOwner::Rules,
                    TextGroupKind::PluginParameter,
                    group_location.clone(),
                    TextUnitRole::Scalar(
                        ScalarFieldKey::new("<json>.text[0]").expect("字段键应合法"),
                    ),
                    TextUnitContent::Value(value.to_owned()),
                    "{}",
                );
                StandardTranslationGroup::new(
                    TextGroupKind::PluginParameter,
                    group_location,
                    vec![StandardTranslationAsset::new(identity, None, None)],
                )
            })
            .collect(),
        );

        let (_, _, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                corpus,
                StandardTranslationInput::new(None, None),
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
    async fn one_minimal_message_can_mix_all_five_semantic_unit_roles() {
        let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, ()>::new(
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
                StandardTranslationCorpus::new(vec![scrolling, choices, dialogue, scalar]),
                StandardTranslationInput::new(None, None),
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
            vec![1, 2, 3, 4, 5]
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
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            group_location.clone(),
            TextUnitRole::DialogueSpeaker,
            TextUnitContent::Value("アリス".to_owned()),
            "{}",
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
        let speaker_state =
            translation_state_context(global, &speaker, &protected_speaker, &bindings, &[])
                .expect("说话人状态应可建立")
                .finish(&speaker_translation);
        let body = TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            group_location.clone(),
            TextUnitRole::DialogueBody,
            TextUnitContent::Lines(vec!["出かけましょう。".to_owned()]),
            r#"{"source_speaker":"アリス"}"#,
        );
        let corpus = StandardTranslationCorpus::new(vec![StandardTranslationGroup::new(
            TextGroupKind::EventDialogue,
            group_location,
            vec![
                StandardTranslationAsset::new(
                    speaker,
                    Some(speaker_translation),
                    Some(speaker_state),
                ),
                StandardTranslationAsset::new(body, None, None),
            ],
        )]);
        let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, ()>::new(
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
                StandardTranslationInput::new(None, None),
            )
            .await
            .expect("已有说话人译文应作为正文语境")
            .into_parts();

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].expected_outputs().len(), 1);
        assert_eq!(tasks[0].expected_outputs()[0].id(), 1);
        let user = tasks[0].messages()[1].content();
        assert!(user.contains("Speaker:爱丽丝"));
        assert!(!user.contains("Speaker ["));
        assert!(!user.contains("アリス"));
        assert!(user.contains("Body [1] (free line breaking):"));
    }

    #[test]
    fn reused_translation_is_preferred_over_source_for_virtual_context() {
        let identity = TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            RpgMakerLocation::value(RpgMakerSource::map(1), Vec::new()),
            TextUnitRole::DialogueSpeaker,
            TextUnitContent::Value("アリス".to_owned()),
            "{}",
        );
        let language_analysis = translation_resources()
            .source_language()
            .analyze_source(&crate::language::LanguageText::natural("アリス"));
        let unit = PreparedUnit {
            field_name: "speaker".to_owned(),
            identity: identity.clone(),
            protected_text: "アリス".to_owned(),
            translation: None,
            placeholders: Vec::new(),
            language_analysis,
            triggered_terms: Vec::new(),
            state_context: TranslationStateContext::new(Sha256Fingerprint::from_bytes([7; 32])),
            responsibility: PreparedUnitResponsibility::Virtual {
                reason: TranslationVirtualReason::Reused {
                    seed: Box::new(identity),
                    translation: TextUnitContent::Value("爱丽丝".to_owned()),
                },
            },
        };

        assert_eq!(context_text(&unit), "爱丽丝");
    }

    #[test]
    fn virtual_context_without_translation_uses_source_instead_of_placeholder_protocol_text() {
        let identity = TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            RpgMakerLocation::value(RpgMakerSource::map(1), Vec::new()),
            TextUnitRole::DialogueSpeaker,
            TextUnitContent::Value("\\N[1]".to_owned()),
            "{}",
        );
        let language_analysis = translation_resources()
            .source_language()
            .analyze_source(&crate::language::LanguageText::natural("角色"));
        let unit = PreparedUnit {
            field_name: "speaker".to_owned(),
            identity,
            protected_text: "⟦ATT_ACTOR_NAME_WHOLE_0000⟧".to_owned(),
            translation: None,
            placeholders: Vec::new(),
            language_analysis,
            triggered_terms: Vec::new(),
            state_context: TranslationStateContext::new(Sha256Fingerprint::from_bytes([8; 32])),
            responsibility: PreparedUnitResponsibility::Virtual {
                reason: TranslationVirtualReason::FullyProtected,
            },
        };

        assert_eq!(context_text(&unit), "\\N[1]");
    }

    #[tokio::test]
    async fn target_language_uses_the_exact_resolved_system_prompt() {
        let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources_for("ja", "zh-Hant"),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );

        let (_, _, tasks) = planner
            .plan(
                &project_with_languages("ja", "zh-Hant"),
                &profile(10_000),
                StandardTranslationCorpus::new(vec![group(
                    RpgMakerSource::data(StandardDataFile::Items),
                    1,
                    "翻訳対象",
                    None,
                    Vec::new(),
                )]),
                StandardTranslationInput::new(None, None),
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
        let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let corpus = StandardTranslationCorpus::new(vec![
            group(RpgMakerSource::map(1), 0, "一番目", None, Vec::new()),
            group(RpgMakerSource::map(2), 0, "二番目", None, Vec::new()),
        ]);

        let (_, _, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                corpus,
                StandardTranslationInput::new(None, None),
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
        let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, ()>::new(
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
                StandardTranslationCorpus::new(vec![
                    display,
                    event_1_page_0_command_2,
                    event_1_page_1_command_0,
                    event_2_page_0_command_0,
                ]),
                StandardTranslationInput::new(None, None),
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
        let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, ()>::new(
            reader,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let corpus = StandardTranslationCorpus::new(vec![
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
                StandardTranslationInput::new(None, Some(placeholder_path)),
            )
            .await
            .expect("整段保护应取消该单元的翻译要求")
            .into_parts();

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].expected_outputs().len(), 1);
        assert_eq!(tasks[0].expected_outputs()[0].id(), 1);
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
        let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, ()>::new(
            reader,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );

        let (_, _, tasks) = planner
            .plan(
                &project(),
                &profile(10_000),
                StandardTranslationCorpus::new(vec![group(
                    RpgMakerSource::data(StandardDataFile::Items),
                    1,
                    r"<code:秘密>前\C[2]後勇者翻訳",
                    None,
                    Vec::new(),
                )]),
                StandardTranslationInput::new(Some(terminology_path), Some(placeholder_path)),
            )
            .await
            .expect("自然段术语应可建立任务")
            .into_parts();

        let user = tasks[0].messages()[1].content();
        assert!(user.contains("- 勇者 → 英雄"));
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
        let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, ()>::new(
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
                StandardTranslationCorpus::new(vec![body]),
                StandardTranslationInput::new(Some(terminology_path), None),
            )
            .await
            .expect("Lines 术语扫描域应可建立任务")
            .into_parts();

        let user = tasks[0].messages()[1].content();
        assert!(!user.contains("- 跨元素 →"));
    }

    #[tokio::test]
    async fn global_deduplication_sends_only_the_first_unit_and_propagates_atomically() {
        let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, ()>::new(
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
                StandardTranslationCorpus::new(vec![neighbouring, duplicate, first]),
                StandardTranslationInput::new(None, None),
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
        assert_eq!(user.matches("保存しますか？").count(), 1);
        assert!(!user.contains("仅上下文"));
        assert_eq!(tasks[0].expected_outputs().len(), 2);
    }

    #[tokio::test]
    async fn manual_candidate_states_match_the_same_profile_standard_plan_at_every_location() {
        use crate::rpg_maker::translate::candidate::{
            StandardCandidateRequest, StandardCandidateUnitIndex,
        };

        let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let corpus = StandardTranslationCorpus::new(vec![
            group(
                RpgMakerSource::data(StandardDataFile::Items),
                1,
                "保存しますか？",
                None,
                Vec::new(),
            ),
            group(
                RpgMakerSource::data(StandardDataFile::Items),
                2,
                "保存しますか？",
                None,
                Vec::new(),
            ),
        ]);
        let profile = profile(10_000);
        let (_, _, tasks) = planner
            .plan(
                &project(),
                &profile,
                corpus.clone(),
                StandardTranslationInput::new(None, None),
            )
            .await
            .expect("普通 Standard 应建立去重计划")
            .into_parts();
        let expected = &tasks[0].expected_outputs()[0];
        let translation = TextUnitContent::Value("是否保存？".to_owned());
        let expected_states = std::iter::once(expected.state_context().finish(&translation))
            .chain(
                expected
                    .propagation_state_contexts()
                    .iter()
                    .map(|context| context.finish(&translation)),
            )
            .collect::<Vec<_>>();

        let session = planner
            .open_candidate_session(&project(), &profile, corpus)
            .await
            .expect("人工 Standard 应复用同一 Profile 语义");
        let prepared = session
            .prepare_acceptance(vec![StandardCandidateRequest::new(
                StandardCandidateUnitIndex::new(0),
                translation,
                false,
            )])
            .expect("人工候选应可验收");
        let actual_states = prepared.commits()[0]
            .writes()
            .iter()
            .map(|write| write.replacement_translation_state())
            .collect::<Vec<_>>();

        assert_eq!(actual_states, expected_states);
    }

    #[tokio::test]
    async fn valid_existing_translation_reuses_without_creating_an_llm_task() {
        let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, ()>::new(
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
                StandardTranslationCorpus::new(vec![target, seed]),
                StandardTranslationInput::new(None, None),
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
        let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let corpus = StandardTranslationCorpus::new(vec![group(
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
                StandardTranslationInput::new(None, None),
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
        let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, ()>::new(
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
        let corpus = StandardTranslationCorpus::new(vec![StandardTranslationGroup::new(
            base.kind(),
            base.group_location().clone(),
            vec![StandardTranslationAsset::new(
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
                StandardTranslationInput::new(None, Some(placeholder_path)),
            )
            .await
            .expect("不命中规则的插入不应使有效译文失效")
            .into_parts();

        assert_eq!(preparation.retained(), 1);
        assert!(preparation.invalidations().is_empty());
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn placeholder_projection_failure_isolated_to_one_unit_and_invalidates_its_old_translation()
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
        let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, ()>::new(
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
                StandardTranslationCorpus::new(vec![bad, good]),
                StandardTranslationInput::new(None, Some(placeholder_path)),
            )
            .await
            .expect("一个单元的 Placeholder 冲突不应阻断其他单元")
            .into_parts();

        assert_eq!(preparation.planning_failures().len(), 1);
        assert!(matches!(
            preparation.planning_failures()[0].reason(),
            TranslationPlanningFailureReason::PlaceholderProtection { .. }
        ));
        assert_eq!(preparation.invalidations().len(), 1);
        assert_eq!(preparation.invalidated(), 1);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].expected_outputs().len(), 1);
        assert!(tasks[0].messages()[1].content().contains("正常な翻訳"));
        assert!(!tasks[0].messages()[1].content().contains("翻訳<BAD>"));
    }

    #[tokio::test]
    async fn line_crossing_placeholder_failure_isolated_to_its_lines_unit() {
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
        let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, ()>::new(
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
                StandardTranslationCorpus::new(vec![bad, good]),
                StandardTranslationInput::new(None, Some(placeholder_path)),
            )
            .await
            .expect("一个 Lines 单元的 Placeholder 边界失败不应阻断其他单元")
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
    async fn conflicting_existing_translations_fail_before_a_plan_is_returned() {
        let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let corpus = StandardTranslationCorpus::new(vec![
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

        let error = planner
            .plan(
                &project(),
                &profile(10_000),
                corpus,
                StandardTranslationInput::new(None, None),
            )
            .await
            .expect_err("同族有效译文冲突必须显式失败");

        assert!(matches!(
            error,
            RpgMakerStandardTranslationTaskPlanningError::InvalidDeduplication(
                TranslationDeduplicationError::ConflictingReusableTranslations { .. }
            )
        ));
    }

    #[tokio::test]
    async fn user_message_target_splits_only_between_groups_inside_the_same_scope() {
        let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, ()>::new(
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
                StandardTranslationCorpus::new(vec![first.clone()]),
                StandardTranslationInput::new(None, None),
            )
            .await
            .expect("单组应该规划成功")
            .into_parts();
        let exact_single_user_message_characters = user_message(&single[0]).chars().count();

        let (_, _, split) = planner
            .plan(
                &project(),
                &profile(exact_single_user_message_characters),
                StandardTranslationCorpus::new(vec![first, second]),
                StandardTranslationInput::new(None, None),
            )
            .await
            .expect("同一范围应在复合组边界切块")
            .into_parts();

        assert_eq!(split.len(), 2);
        assert_eq!(split[0].expected_outputs()[0].id(), 1);
        assert_eq!(split[1].expected_outputs()[0].id(), 1);
    }

    #[tokio::test]
    async fn system_prompt_is_independent_from_exact_user_message_target() {
        let system_markdown = format!("# System\n{}", "固定规则。".repeat(200));
        let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, ()>::new(
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
                StandardTranslationCorpus::new(vec![seed.clone()]),
                StandardTranslationInput::new(None, None),
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
                StandardTranslationCorpus::new(vec![seed.clone()]),
                StandardTranslationInput::new(None, None),
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
                StandardTranslationCorpus::new(vec![seed]),
                StandardTranslationInput::new(None, None),
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
        let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, ()>::new(
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
                StandardTranslationCorpus::new(vec![first, oversized, third, fourth]),
                StandardTranslationInput::new(None, None),
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
                assert_eq!(outputs[0].id(), 1);
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
    fn incremental_user_message_size_matches_the_exact_rendered_markdown() {
        let first_term = crate::rpg_maker::translate::planning_resource::TerminologyEntry::new(
            "术语.一",
            "翻译(一)",
            Vec::new(),
        );
        let second_term = crate::rpg_maker::translate::planning_resource::TerminologyEntry::new(
            "术语二",
            "翻译二",
            Vec::new(),
        );
        let first_line = terminology_line_character_count(&first_term);
        let second_line = terminology_line_character_count(&second_term);
        let first_group = "## Database Text\n\nName [1] (single line):一\n";
        let second_group = "## Database Text\n\nName [2] (single line):二\n";

        let first_size = candidate_user_message_character_count(
            0,
            false,
            0,
            1,
            first_line,
            first_group.chars().count(),
        );
        let expected_first = format!("Terminology:\n\n- 术语\\.一 → 翻译\\(一\\)\n\n{first_group}");
        assert_eq!(first_size, expected_first.chars().count());

        let second_size = candidate_user_message_character_count(
            first_size,
            true,
            1,
            1,
            second_line,
            second_group.chars().count(),
        );
        let expected_second = format!(
            "Terminology:\n\n- 术语\\.一 → 翻译\\(一\\)\n- 术语二 → 翻译二\n\n{first_group}\n{second_group}"
        );
        assert_eq!(second_size, expected_second.chars().count());

        let inserted_size = candidate_user_message_character_count(
            first_group.chars().count(),
            true,
            0,
            1,
            first_line,
            second_group.chars().count(),
        );
        let expected_inserted =
            format!("Terminology:\n\n- 术语\\.一 → 翻译\\(一\\)\n\n{first_group}\n{second_group}");
        assert_eq!(inserted_size, expected_inserted.chars().count());
    }

    #[test]
    fn sparse_terminology_prompt_visits_only_matches_and_preserves_natural_order() {
        let entries = (0..4_096)
            .map(|index| {
                crate::rpg_maker::translate::planning_resource::TerminologyEntry::new(
                    format!("术语-{index:04}-末"),
                    format!("译文-{index:04}-末"),
                    vec![format!("触发-{index:04}-末")],
                )
            })
            .collect();
        let terminology =
            super::super::planning_resource::compile_terminology(entries).expect("术语索引应建立");
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
        let planner = RpgMakerStandardTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let corpus = StandardTranslationCorpus::new(vec![
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
                StandardTranslationInput::new(None, None),
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
