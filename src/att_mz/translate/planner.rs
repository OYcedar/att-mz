//! MZ 标准翻译任务规划：自然排序、语义范围、虚原文、术语和占位符。

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

use futures_util::stream::{self, StreamExt};

use crate::att_mz::location_codec::MzLocationCodec;
use crate::att_mz::project::OpenedProject;
use crate::att_mz::standard_asset::MzStandardAssetStorageKind;
use crate::att_mz::text::{MzLocationStep, MzSource, StandardDataFile, TextGroupKind};
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::language::{LanguageAnalysis, LanguagePair};
use crate::llm::{ChatMessage, ChatMessageRole, LlmClientSemanticIdentity};

use super::deduplication::{
    TranslationDeduplicationCandidate, TranslationDeduplicationError,
    TranslationDeduplicationOutcome, deduplicate_translation_candidates,
};
use super::placeholder::{Pcre2PlaceholderService, PlaceholderRuleCompilationError};
use super::planning_resource::{CompiledTerminology, TranslationPlanningResourceReader};
use super::profile::{MzTranslationProfile, ResolvedMzTranslationResources};
use super::semantics::{
    PreparedTranslationAcceptance, PreparedTranslationStatus, ResolvedTranslationSemanticError,
    ResolvedTranslationSemantics,
};
use super::standard::{
    ExpectedTranslationOutput, StandardTranslationCorpus, StandardTranslationGroup,
    StandardTranslationInput, StandardTranslationPlan, StandardTranslationTaskIndex,
    StandardTranslationTaskPlanner, TerminologyDependency, TranslationInvalidation,
    TranslationLeafIdentity, TranslationPlanPreparation, TranslationPlanPreparationCounts,
    TranslationPropagationTarget, TranslationStateContext, TranslationTaskBlock,
    TranslationTaskGroup, TranslationTaskUnit, TranslationVirtualReason,
};

/// 使用三个职责模块与 CPU 根建立确定性 MZ 翻译计划。
pub(crate) struct MzStandardTranslationTaskPlanningService<R, C, L> {
    resources: R,
    translation_resources: Arc<ResolvedMzTranslationResources>,
    placeholders: Pcre2PlaceholderService,
    cpu: C,
    llm_client: PhantomData<fn() -> L>,
}

impl<R, C, L> MzStandardTranslationTaskPlanningService<R, C, L> {
    pub(crate) fn new(
        resources: R,
        translation_resources: Arc<ResolvedMzTranslationResources>,
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

impl<R, C, L> StandardTranslationTaskPlanner for MzStandardTranslationTaskPlanningService<R, C, L>
where
    R: TranslationPlanningResourceReader,
    C: CpuTaskExecutor,
    L: LlmClientSemanticIdentity + 'static,
{
    type Profile = Arc<MzTranslationProfile<L>>;
    type Error = MzStandardTranslationTaskPlanningError<R::Error, C::Error>;

    async fn plan(
        &self,
        project: &OpenedProject,
        profile: &Self::Profile,
        corpus: StandardTranslationCorpus,
        input: StandardTranslationInput,
    ) -> Result<StandardTranslationPlan, Self::Error> {
        let resolved_pair = self.translation_resources.language_pair();
        if project.source_language() != resolved_pair.source()
            || project.target_language() != resolved_pair.target()
        {
            return Err(
                MzStandardTranslationTaskPlanningError::ResolvedLanguagePairMismatch {
                    project_source: project.source_language().to_string(),
                    project_target: project.target_language().to_string(),
                    resolved_source: resolved_pair.source().to_string(),
                    resolved_target: resolved_pair.target().to_string(),
                },
            );
        }
        let planning = profile.planning();
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
            .map_err(MzStandardTranslationTaskPlanningError::ReadResources)?;
        let (terminology, placeholder_definitions, terminology_json, placeholder_rules_json) =
            resources.into_parts();

        let placeholder_service = self.placeholders.clone();
        let custom_placeholders = self
            .cpu
            .execute(move || placeholder_service.compile_custom(placeholder_definitions))
            .await
            .map_err(MzStandardTranslationTaskPlanningError::CompilePlaceholdersCompute)?
            .map_err(MzStandardTranslationTaskPlanningError::InvalidPlaceholderRules)?;

        let prepared = self
            .cpu
            .execute(move || prepare_corpus(groups))
            .await
            .map_err(MzStandardTranslationTaskPlanningError::PrepareCorpusCompute)?
            .map_err(MzStandardTranslationTaskPlanningError::InvalidCorpus)?;

        let source_language_id = project.source_language().to_owned();
        let target_language_id = project.target_language().to_owned();
        let global_semantics = global_translation_semantics(
            source_language_id.as_str(),
            target_language_id.as_str(),
            source_language.semantic_fingerprint(),
            &system_markdown,
            profile.llm_client().semantic_fingerprint(),
        );
        let task_language_pair = LanguagePair::new(source_language_id, target_language_id);
        let semantics = Arc::new(ResolvedTranslationSemantics::new(
            system_markdown.clone(),
            task_language_pair.clone(),
            Arc::clone(&terminology),
            self.placeholders.clone(),
            custom_placeholders,
            source_language,
            global_semantics,
        ));
        let max_characters = planning.max_message_characters().get();
        let scope_concurrency = planning.scope_concurrency().get();
        let mut preprocessed_scopes = stream::iter(prepared.into_iter().map(|scope| {
            let semantics = Arc::clone(&semantics);
            async move {
                let scope_name = scope.key.clone();
                self.cpu
                    .execute(move || preprocess_scope(scope, semantics))
                    .await
                    .map_err(|source| ScopePreprocessingFailure::Compute {
                        scope: scope_name.clone(),
                        source,
                    })?
                    .map_err(|source| ScopePreprocessingFailure::Invalid {
                        scope: scope_name,
                        source,
                    })
            }
        }))
        .buffered(scope_concurrency);

        let mut scopes = Vec::new();
        while let Some(result) = preprocessed_scopes.next().await {
            scopes.push(result.map_err(|failure| match failure {
                ScopePreprocessingFailure::Compute { scope, source } => {
                    MzStandardTranslationTaskPlanningError::PreprocessScopeCompute { scope, source }
                }
                ScopePreprocessingFailure::Invalid { scope, source } => {
                    MzStandardTranslationTaskPlanningError::InvalidScopePreprocessing {
                        scope,
                        source,
                    }
                }
            })?);
        }

        let (candidates, positions, mut invalidations) = collect_deduplication_inputs(&scopes);
        let deduplicated = self
            .cpu
            .execute(move || deduplicate_translation_candidates(candidates))
            .await
            .map_err(MzStandardTranslationTaskPlanningError::DeduplicateCompute)?
            .map_err(MzStandardTranslationTaskPlanningError::InvalidDeduplication)?;
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
            .count();

        let mut planned_scopes = stream::iter(scopes.into_iter().map(|scope| {
            let terminology = terminology.clone();
            let system_markdown = system_markdown.clone();
            async move {
                let scope_name = scope.key.clone();
                self.cpu
                    .execute(move || {
                        build_scope_tasks(scope, terminology, &system_markdown, max_characters)
                    })
                    .await
                    .map_err(|source| ScopePlanningFailure::Compute {
                        scope: scope_name.clone(),
                        source,
                    })?
                    .map_err(|source| ScopePlanningFailure::Invalid {
                        scope: scope_name,
                        source,
                    })
            }
        }))
        .buffered(scope_concurrency);

        let mut unindexed_tasks = Vec::new();
        while let Some(result) = planned_scopes.next().await {
            unindexed_tasks.extend(result.map_err(|failure| match failure {
                ScopePlanningFailure::Compute { scope, source } => {
                    MzStandardTranslationTaskPlanningError::PlanScopeCompute { scope, source }
                }
                ScopePlanningFailure::Invalid { scope, source } => {
                    MzStandardTranslationTaskPlanningError::InvalidScope { scope, source }
                }
            })?);
        }

        let tasks = unindexed_tasks
            .into_iter()
            .enumerate()
            .map(|(index, task)| {
                task.with_index(
                    StandardTranslationTaskIndex::new(index),
                    task_language_pair.clone(),
                )
            })
            .collect();

        Ok(StandardTranslationPlan::new(
            Arc::clone(&semantics),
            TranslationPlanPreparation::with_baseline(
                invalidations,
                reuses,
                terminology_json,
                placeholder_rules_json,
                TranslationPlanPreparationCounts::new(retained, invalidated, not_applicable),
                snapshot_baseline,
            ),
            tasks,
        ))
    }
}

struct PreparedCorpus {
    scopes: Vec<PreparedScope>,
}

impl IntoIterator for PreparedCorpus {
    type Item = PreparedScope;
    type IntoIter = std::vec::IntoIter<PreparedScope>;

    fn into_iter(self) -> Self::IntoIter {
        self.scopes.into_iter()
    }
}

struct PreparedScope {
    key: SemanticScopeKey,
    groups: Vec<PreparedGroup>,
}

struct PreparedGroup {
    kind: TextGroupKind,
    group_location: crate::att_mz::text::MzLocation,
    assets: Vec<PreparedAsset>,
}

struct PreparedAsset {
    identity: TranslationLeafIdentity,
    translation: Option<String>,
    translation_state: Option<Sha256Fingerprint>,
}

fn prepare_corpus(
    groups: Vec<StandardTranslationGroup>,
) -> Result<PreparedCorpus, CorpusPlanningError> {
    let mut by_scope = BTreeMap::<SemanticScopeKey, Vec<PreparedGroup>>::new();
    for group in groups {
        let scope = SemanticScopeKey::from_group(&group)?;
        let kind = group.kind();
        let group_location = group.group_location().clone();
        let mut assets = Vec::new();
        for asset in group.into_assets() {
            let (identity, translation, translation_state) = asset.into_parts();
            assets.push(PreparedAsset {
                identity,
                translation,
                translation_state,
            });
        }
        assets.sort_by(|left, right| {
            left.identity
                .exact_location()
                .cmp(right.identity.exact_location())
        });
        by_scope.entry(scope).or_default().push(PreparedGroup {
            kind,
            group_location,
            assets,
        });
    }

    let scopes = by_scope
        .into_iter()
        .map(|(key, mut groups)| {
            groups.sort_by(|left, right| left.group_location.cmp(&right.group_location));
            PreparedScope { key, groups }
        })
        .collect();

    Ok(PreparedCorpus { scopes })
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum SemanticScopeKey {
    StandardDatabase(StandardDataFile),
    System,
    Map(u32),
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
            MzSource::Data(StandardDataFile::System) => Ok(Self::System),
            MzSource::Data(StandardDataFile::CommonEvents) => {
                first_array_index(group).map(Self::CommonEvent)
            }
            MzSource::Data(StandardDataFile::Troops) => first_array_index(group).map(Self::Troop),
            MzSource::Data(file) => Ok(Self::StandardDatabase(*file)),
            MzSource::Map(map_id) => Ok(Self::Map(*map_id)),
            MzSource::PluginParameter {
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
            Self::System => formatter.write_str("data/System.json"),
            Self::Map(map_id) => write!(formatter, "Map{map_id:03}"),
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
            MzLocationStep::ArrayIndex(index) => Some(*index),
            _ => None,
        })
        .ok_or_else(|| CorpusPlanningError::MissingSemanticIndex {
            location: group.group_location().to_string(),
        })
}

struct PreprocessedScope {
    key: SemanticScopeKey,
    groups: Vec<PreprocessedGroup>,
}

struct PreprocessedGroup {
    kind: TextGroupKind,
    group_location: crate::att_mz::text::MzLocation,
    units: Vec<PreprocessedUnit>,
}

struct PreprocessedUnit {
    identity: TranslationLeafIdentity,
    protected_text: String,
    placeholders: Vec<super::standard::AppliedPlaceholder>,
    language_analysis: LanguageAnalysis,
    translation: Option<String>,
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
) -> Result<PreprocessedScope, ScopePreprocessingError> {
    let mut groups = Vec::with_capacity(scope.groups.len());
    for group in scope.groups {
        let mut units = Vec::with_capacity(group.assets.len());
        for asset in group.assets {
            let prepared = semantics
                .prepare(group.kind, asset.identity.original_text())
                .map_err(ScopePreprocessingError::Semantics)?;
            let protected_text = prepared.model_text().to_owned();
            let placeholders = prepared.placeholders().to_vec();
            let language_analysis = prepared.language_analysis().clone();
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
            } else if let Some(translation) = asset.translation.as_deref() {
                asset.translation_state == Some(state_context.finish(translation))
                    && matches!(
                        prepared
                            .accept(translation)
                            .map_err(ScopePreprocessingError::Semantics)?,
                        PreparedTranslationAcceptance::Accepted(accepted)
                            if accepted == translation
                    )
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
            group_location: group.group_location,
            units,
        });
    }
    Ok(PreprocessedScope {
        key: scope.key,
        groups,
    })
}

fn global_translation_semantics(
    source_language: &str,
    target_language: &str,
    language_semantics: Sha256Fingerprint,
    system_markdown: &str,
    client_semantics: Sha256Fingerprint,
) -> Sha256Fingerprint {
    let mut hasher = Sha256FramedHasher::new(b"att.mz.translation-global");
    hasher
        .frame(1, source_language.as_bytes())
        .frame(2, target_language.as_bytes())
        .frame(3, language_semantics.as_bytes())
        .frame(4, system_markdown.as_bytes())
        .frame(5, client_semantics.as_bytes());
    hasher.finish()
}

fn translation_state_context(
    global_semantics: Sha256Fingerprint,
    identity: &TranslationLeafIdentity,
    protected_text: &str,
    placeholders: &[super::standard::AppliedPlaceholder],
    terminology: &[TerminologyDependency],
) -> Result<TranslationStateContext, ScopePreprocessingError> {
    let storage = MzStandardAssetStorageKind::for_group_kind(identity.kind());
    let exact_location = MzLocationCodec::encode(identity.exact_location())
        .map_err(ScopePreprocessingError::EncodeStateLocation)?;
    let mut hasher = Sha256FramedHasher::new(b"att.mz.translation-leaf-context");
    hasher
        .frame(1, global_semantics.as_bytes())
        .frame(2, identity.owner().storage_name().as_bytes())
        .frame(3, storage.table().storage_name().as_bytes())
        .frame(
            4,
            storage
                .unit_type()
                .map_or(b"".as_slice(), |unit| unit.storage_name().as_bytes()),
        )
        .frame(5, identity.field_name().as_bytes())
        .frame(6, exact_location.as_bytes())
        .frame(7, identity.original_text().as_bytes())
        .frame(8, protected_text.as_bytes());
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
                            .as_deref()
                            .expect("只有已有译文的叶子才可能术语失效"),
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
        "全局去重必须为每个候选叶子返回一个责任"
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
    group_location: crate::att_mz::text::MzLocation,
    units: Vec<PreparedUnit>,
    triggered_terms: Vec<usize>,
}

#[derive(Clone)]
struct PreparedUnit {
    field_name: String,
    identity: TranslationLeafIdentity,
    protected_text: String,
    placeholders: Vec<super::standard::AppliedPlaceholder>,
    language_analysis: LanguageAnalysis,
    state_context: TranslationStateContext,
    responsibility: PreparedUnitResponsibility,
}

fn build_scope_tasks(
    scope: PreprocessedScope,
    terminology: Arc<CompiledTerminology>,
    system_markdown: &str,
    max_characters: usize,
) -> Result<Vec<UnindexedTask>, ScopePlanningError> {
    let mut prepared_groups = Vec::with_capacity(scope.groups.len());
    for group in scope.groups {
        let units = group
            .units
            .into_iter()
            .map(|unit| PreparedUnit {
                field_name: unit.identity.field_name().to_owned(),
                identity: unit.identity,
                protected_text: unit.protected_text,
                placeholders: unit.placeholders,
                language_analysis: unit.language_analysis,
                state_context: unit.state_context,
                responsibility: unit.responsibility,
            })
            .collect::<Vec<_>>();
        let triggered_terms =
            terminology.triggered_indices(units.iter().map(|unit| unit.identity.original_text()));
        prepared_groups.push(PreparedTaskGroup {
            kind: group.kind,
            group_location: group.group_location,
            units,
            triggered_terms,
        });
    }

    pack_scope(
        prepared_groups,
        terminology.as_ref(),
        system_markdown,
        max_characters,
    )
}

struct RenderedGroup {
    group: TranslationTaskGroup,
    markdown: String,
    expected: Vec<ExpectedBase>,
    active_count: usize,
    triggered_terms: Vec<usize>,
}

struct ExpectedBase {
    id: usize,
    identity: TranslationLeafIdentity,
    propagation_targets: Vec<TranslationPropagationTarget>,
    placeholders: Vec<super::standard::AppliedPlaceholder>,
    language_analysis: LanguageAnalysis,
    state_context: TranslationStateContext,
}

fn render_group(seed: PreparedTaskGroup, first_active_id: usize, ordinal: usize) -> RenderedGroup {
    let mut active_id = first_active_id;
    let mut units = Vec::with_capacity(seed.units.len());
    let mut expected = Vec::new();
    let mut markdown = format!(
        "\n## 语义组 {} · {}\n",
        ordinal + 1,
        human_group_kind(seed.kind)
    );

    for unit in seed.units {
        match unit.responsibility {
            PreparedUnitResponsibility::Active {
                propagation_targets,
            } => {
                markdown.push_str(&format!("\n### [{}] {}\n", active_id, unit.field_name));
                push_blockquote(&mut markdown, &unit.protected_text);
                expected.push(ExpectedBase {
                    id: active_id,
                    identity: unit.identity.clone(),
                    propagation_targets,
                    placeholders: unit.placeholders.clone(),
                    language_analysis: unit.language_analysis,
                    state_context: unit.state_context,
                });
                units.push(TranslationTaskUnit::active(
                    unit.field_name,
                    unit.identity,
                    unit.protected_text,
                    unit.placeholders,
                    active_id,
                ));
                active_id += 1;
            }
            PreparedUnitResponsibility::Virtual { reason } => {
                markdown.push_str(&format!("\n### [仅上下文] {}\n", unit.field_name));
                push_blockquote(&mut markdown, &unit.protected_text);
                units.push(TranslationTaskUnit::virtual_context(
                    unit.field_name,
                    unit.identity,
                    unit.protected_text,
                    unit.placeholders,
                    reason,
                ));
            }
            PreparedUnitResponsibility::AwaitingDeduplication => {
                unreachable!("任务切块前必须完成全局去重")
            }
        }
    }

    RenderedGroup {
        group: TranslationTaskGroup::new(seed.kind, seed.group_location, units),
        markdown,
        expected,
        active_count: active_id - first_active_id,
        triggered_terms: seed.triggered_terms,
    }
}

fn push_blockquote(markdown: &mut String, text: &str) {
    for line in text.split('\n') {
        markdown.push_str("> ");
        markdown.push_str(line);
        markdown.push('\n');
    }
}

fn pack_scope(
    groups: Vec<PreparedTaskGroup>,
    terminology: &CompiledTerminology,
    system_markdown: &str,
    max_characters: usize,
) -> Result<Vec<UnindexedTask>, ScopePlanningError> {
    let mut tasks = Vec::new();
    let mut current_groups = Vec::<RenderedGroup>::new();
    let mut current_active = 0usize;
    let mut current_terms = Vec::<bool>::new();

    for seed in groups {
        let rendered = render_group(seed.clone(), current_active, current_groups.len());
        let candidate_terms = merged_term_flags(
            &current_terms,
            &rendered.triggered_terms,
            terminology.entries().len(),
        );
        let candidate_size = message_character_count(
            system_markdown,
            &current_groups,
            Some(&rendered),
            terminology,
            &candidate_terms,
        );

        if candidate_size <= max_characters {
            current_active += rendered.active_count;
            current_terms = candidate_terms;
            current_groups.push(rendered);
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
        current_active = 0;
        current_terms.clear();

        let rendered = render_group(seed, 0, 0);
        let terms = merged_term_flags(&[], &rendered.triggered_terms, terminology.entries().len());
        let group_size =
            message_character_count(system_markdown, &[], Some(&rendered), terminology, &terms);
        if group_size > max_characters {
            if rendered.active_count > 0 {
                return Err(ScopePlanningError::GroupExceedsCapacity {
                    group_kind: human_group_kind(rendered.group.kind()),
                    actual_characters: group_size,
                    maximum_characters: max_characters,
                });
            }
            continue;
        }
        current_active = rendered.active_count;
        current_terms = terms;
        current_groups.push(rendered);
    }

    if current_active > 0 {
        tasks.push(finalize_task(
            current_groups,
            terminology,
            &current_terms,
            system_markdown,
        ));
    }
    Ok(tasks)
}

fn merged_term_flags(current: &[bool], additional: &[usize], total: usize) -> Vec<bool> {
    let mut result = if current.is_empty() {
        vec![false; total]
    } else {
        current.to_vec()
    };
    for &index in additional {
        result[index] = true;
    }
    result
}

fn message_character_count(
    system_markdown: &str,
    current_groups: &[RenderedGroup],
    additional_group: Option<&RenderedGroup>,
    terminology: &CompiledTerminology,
    term_flags: &[bool],
) -> usize {
    system_markdown.chars().count()
        + render_user_markdown(current_groups, additional_group, terminology, term_flags)
            .chars()
            .count()
}

fn render_user_markdown(
    groups: &[RenderedGroup],
    additional_group: Option<&RenderedGroup>,
    terminology: &CompiledTerminology,
    term_flags: &[bool],
) -> String {
    let mut markdown = String::from("# 翻译任务\n\n");
    let terms = terminology
        .entries()
        .iter()
        .enumerate()
        .filter(|(index, _)| term_flags.get(*index).copied().unwrap_or(false))
        .map(|(_, entry)| entry)
        .collect::<Vec<_>>();
    if !terms.is_empty() {
        markdown.push_str("## 本任务术语\n\n");
        for entry in terms {
            markdown.push_str("- **");
            markdown.push_str(entry.term());
            markdown.push_str("** → **");
            markdown.push_str(entry.translation());
            markdown.push_str("**\n");
        }
    }
    markdown.push_str("\n# 文本\n");
    for group in groups {
        markdown.push_str(&group.markdown);
    }
    if let Some(group) = additional_group {
        markdown.push_str(&group.markdown);
    }
    markdown
}

fn finalize_task(
    groups: Vec<RenderedGroup>,
    terminology: &CompiledTerminology,
    term_flags: &[bool],
    system_markdown: &str,
) -> UnindexedTask {
    let injected = terminology
        .entries()
        .iter()
        .enumerate()
        .filter(|(index, _)| term_flags.get(*index).copied().unwrap_or(false))
        .map(|(_, entry)| entry.dependency())
        .collect::<Vec<_>>();
    let user_markdown = render_user_markdown(&groups, None, terminology, term_flags);
    let mut task_groups = Vec::with_capacity(groups.len());
    let mut expected_outputs = Vec::new();
    for group in groups {
        task_groups.push(group.group);
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
                expected.placeholders,
                expected.language_analysis,
                expected.state_context,
                propagation_state_contexts,
            )
        }));
    }
    UnindexedTask {
        groups: task_groups,
        injected_terminology: injected,
        messages: vec![
            ChatMessage::new(ChatMessageRole::System, system_markdown),
            ChatMessage::new(ChatMessageRole::User, user_markdown),
        ],
        expected_outputs,
    }
}

struct UnindexedTask {
    groups: Vec<TranslationTaskGroup>,
    injected_terminology: Vec<TerminologyDependency>,
    messages: Vec<ChatMessage>,
    expected_outputs: Vec<ExpectedTranslationOutput>,
}

impl UnindexedTask {
    fn with_index(
        self,
        index: StandardTranslationTaskIndex,
        language_pair: LanguagePair,
    ) -> TranslationTaskBlock {
        TranslationTaskBlock::new(
            index,
            language_pair,
            self.groups,
            self.injected_terminology,
            self.messages,
            self.expected_outputs,
        )
    }
}

const fn human_group_kind(kind: TextGroupKind) -> &'static str {
    match kind {
        TextGroupKind::DatabaseEntry => "数据库对象",
        TextGroupKind::System => "系统文本",
        TextGroupKind::Map => "地图文本",
        TextGroupKind::EventDialogue => "事件对话",
        TextGroupKind::EventChoices => "事件选项",
        TextGroupKind::EventScrollingText => "滚动文本",
        TextGroupKind::EventCommand => "事件命令",
        TextGroupKind::PluginParameter => "插件参数",
    }
}

enum ScopePlanningFailure<C> {
    Compute {
        scope: SemanticScopeKey,
        source: CpuTaskExecutionError<C>,
    },
    Invalid {
        scope: SemanticScopeKey,
        source: ScopePlanningError,
    },
}

enum ScopePreprocessingFailure<C> {
    Compute {
        scope: SemanticScopeKey,
        source: CpuTaskExecutionError<C>,
    },
    Invalid {
        scope: SemanticScopeKey,
        source: ScopePreprocessingError,
    },
}

#[derive(Debug)]
pub(crate) enum MzStandardTranslationTaskPlanningError<R, C> {
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
    PreprocessScopeCompute {
        scope: SemanticScopeKey,
        source: CpuTaskExecutionError<C>,
    },
    InvalidScopePreprocessing {
        scope: SemanticScopeKey,
        source: ScopePreprocessingError,
    },
    DeduplicateCompute(CpuTaskExecutionError<C>),
    InvalidDeduplication(TranslationDeduplicationError),
    PlanScopeCompute {
        scope: SemanticScopeKey,
        source: CpuTaskExecutionError<C>,
    },
    InvalidScope {
        scope: SemanticScopeKey,
        source: ScopePlanningError,
    },
}

impl<R: fmt::Display, C: fmt::Display> fmt::Display
    for MzStandardTranslationTaskPlanningError<R, C>
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
            Self::PreprocessScopeCompute { scope, source } => {
                write!(formatter, "无法调度语义范围 {scope} 的译前处理：{source}")
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
            Self::PlanScopeCompute { scope, source } => {
                write!(formatter, "无法调度语义范围 {scope} 的任务规划：{source}")
            }
            Self::InvalidScope { scope, source } => {
                write!(formatter, "语义范围 {scope} 无法建立任务：{source}")
            }
        }
    }
}

impl<R: Error + 'static, C: Error + 'static> Error
    for MzStandardTranslationTaskPlanningError<R, C>
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ReadResources(source) => Some(source),
            Self::CompilePlaceholdersCompute(source) => Some(source),
            Self::InvalidPlaceholderRules(source) => Some(source),
            Self::PrepareCorpusCompute(source) => Some(source),
            Self::InvalidCorpus(source) => Some(source),
            Self::PreprocessScopeCompute { source, .. } => Some(source),
            Self::InvalidScopePreprocessing { source, .. } => Some(source),
            Self::DeduplicateCompute(source) => Some(source),
            Self::InvalidDeduplication(source) => Some(source),
            Self::PlanScopeCompute { source, .. } => Some(source),
            Self::InvalidScope { source, .. } => Some(source),
            Self::ResolvedLanguagePairMismatch { .. } => None,
        }
    }
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
    Semantics(ResolvedTranslationSemanticError),
    EncodeStateLocation(crate::att_mz::location_codec::MzLocationCodecError),
}

impl fmt::Display for ScopePreprocessingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Semantics(source) => write!(formatter, "无法建立受信翻译语义：{source}"),
            Self::EncodeStateLocation(source) => {
                write!(formatter, "无法编码译文状态位置：{source}")
            }
        }
    }
}

impl Error for ScopePreprocessingError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Semantics(source) => Some(source),
            Self::EncodeStateLocation(source) => Some(source),
        }
    }
}

#[derive(Debug)]
pub(crate) enum ScopePlanningError {
    GroupExceedsCapacity {
        group_kind: &'static str,
        actual_characters: usize,
        maximum_characters: usize,
    },
}

impl fmt::Display for ScopePlanningError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GroupExceedsCapacity {
                group_kind,
                actual_characters,
                maximum_characters,
            } => write!(
                formatter,
                "不可拆的{group_kind}需要 {actual_characters} 个 Unicode 字符，超过配置上限 {maximum_characters}"
            ),
        }
    }
}

impl Error for ScopePlanningError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::GroupExceedsCapacity { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::num::NonZeroUsize;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::att_mz::text::{MzLocation, MzLocationStep};
    use crate::storage::file_system::{FileReader, ReadFile, ReadFileError};

    use super::*;
    use crate::att_mz::standard_asset::MzStandardAssetOwner;
    use crate::att_mz::translate::planning_resource::{
        JsonTranslationPlanningResourceReadingService, TranslationPlanningResources,
    };
    use crate::att_mz::translate::profile::{
        MzSystemPrompt, MzTranslationPlanningConfiguration, MzTranslationProfile,
        MzTranslationRequestConfiguration, ResolvedMzTranslationResources,
    };
    use crate::att_mz::translate::standard::StandardTranslationAsset;
    use crate::att_mz::translate::standard::TranslationTaskUnitMode;
    use crate::language::{
        JapaneseLanguageModule, JapaneseResidualPolicy, LanguageId, LanguageModule, LanguagePair,
    };

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

    #[derive(Clone)]
    struct YieldingCpu {
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
    }

    impl CpuTaskExecutor for YieldingCpu {
        type Error = FakeError;

        async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            tokio::task::yield_now().await;
            let output = task();
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(output)
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

    impl LlmClientSemanticIdentity for () {
        fn semantic_fingerprint(&self) -> Sha256Fingerprint {
            Sha256Fingerprint::from_bytes([0x33; 32])
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
    ) -> Arc<ResolvedMzTranslationResources> {
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
        let prompt = MzSystemPrompt::new(pair, "# System\n完整且由外部提供。".to_owned())
            .expect("测试 Prompt 应合法");
        Arc::new(ResolvedMzTranslationResources::new(prompt, module))
    }

    fn translation_resources() -> Arc<ResolvedMzTranslationResources> {
        translation_resources_for("ja", "zh-Hans")
    }

    fn profile(max_message_characters: usize) -> Arc<MzTranslationProfile<()>> {
        profile_with_scope_concurrency(max_message_characters, 2)
    }

    fn profile_with_scope_concurrency(
        max_message_characters: usize,
        scope_concurrency: usize,
    ) -> Arc<MzTranslationProfile<()>> {
        let planning = MzTranslationPlanningConfiguration::new(
            NonZeroUsize::new(scope_concurrency).expect("测试范围并发数必须非零"),
            NonZeroUsize::new(max_message_characters).expect("测试容量必须非零"),
        );
        Arc::new(MzTranslationProfile::new(
            "test",
            NonZeroUsize::new(2).expect("常量非零"),
            planning,
            MzTranslationRequestConfiguration::new(Vec::new(), std::time::Duration::ZERO),
            Arc::new(()),
        ))
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
            crate::att_mz::project::test_layout_profile(),
        )
    }

    fn group(
        source: MzSource,
        object_index: usize,
        original: impl Into<String>,
        translation: Option<&str>,
        terms: Vec<TerminologyDependency>,
    ) -> StandardTranslationGroup {
        let original = original.into();
        let group_location =
            MzLocation::value(source.clone(), vec![MzLocationStep::index(object_index)]);
        let exact_location = MzLocation::value(
            source,
            vec![
                MzLocationStep::index(object_index),
                MzLocationStep::key("name"),
            ],
        );
        let identity = TranslationLeafIdentity::new(
            MzStandardAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            "name",
            group_location.clone(),
            exact_location,
            original.clone(),
        );
        let translation_state = translation.map(|translation| {
            let placeholders = Pcre2PlaceholderService::new().expect("内建占位符应可编译");
            let custom = placeholders
                .compile_custom(Vec::new())
                .expect("空自定义占位符应可编译");
            let (protected_text, bindings) = placeholders
                .protect(TextGroupKind::DatabaseEntry, &original, &custom)
                .expect("测试原文应可保护")
                .into_parts();
            let source_language = translation_resources().source_language();
            let global = global_translation_semantics(
                "ja",
                "zh-Hans",
                source_language.semantic_fingerprint(),
                "# System\n完整且由外部提供。",
                ().semantic_fingerprint(),
            );
            translation_state_context(global, &identity, &protected_text, &bindings, &terms)
                .expect("测试译文状态应可建立")
                .finish(translation)
        });
        StandardTranslationGroup::new(
            TextGroupKind::DatabaseEntry,
            group_location,
            vec![StandardTranslationAsset::new(
                identity,
                translation.map(str::to_owned),
                translation_state,
            )],
        )
    }

    fn map_group(
        kind: TextGroupKind,
        event_index: Option<usize>,
        page_index: Option<usize>,
        command_index: Option<usize>,
        original: &str,
    ) -> StandardTranslationGroup {
        let source = MzSource::map(1);
        let group_steps = match (event_index, page_index, command_index) {
            (None, None, None) => Vec::new(),
            (Some(event), Some(page), Some(command)) => vec![
                MzLocationStep::key("events"),
                MzLocationStep::index(event),
                MzLocationStep::key("pages"),
                MzLocationStep::index(page),
                MzLocationStep::key("list"),
                MzLocationStep::index(command),
            ],
            _ => panic!("Map 事件测试位置必须同时给出 event/page/command"),
        };
        let exact_steps = if group_steps.is_empty() {
            vec![MzLocationStep::key("displayName")]
        } else {
            let mut steps = group_steps.clone();
            steps.extend([MzLocationStep::key("parameters"), MzLocationStep::index(0)]);
            steps
        };
        let group_location = MzLocation::value(source.clone(), group_steps);
        let identity = TranslationLeafIdentity::new(
            MzStandardAssetOwner::Builtin,
            kind,
            if kind == TextGroupKind::Map {
                "displayName"
            } else {
                "body[0]"
            },
            group_location.clone(),
            MzLocation::value(source, exact_steps),
            original,
        );
        StandardTranslationGroup::new(
            kind,
            group_location,
            vec![StandardTranslationAsset::new(identity, None, None)],
        )
    }

    #[tokio::test]
    async fn changed_terminology_invalidates_exact_translation_and_builds_dense_task() {
        let terminology_path = PathBuf::from("C:/input/terms.json");
        let mut files = BTreeMap::new();
        files.insert(
            terminology_path.clone(),
            r#"[{"term":"魔法剣","translation":"魔法之剑","triggers":["魔法剣"]}]"#
                .as_bytes()
                .to_vec(),
        );
        let reader = JsonTranslationPlanningResourceReadingService::new(
            MemoryFileReader {
                files: Arc::new(files),
            },
            ImmediateCpu,
        );
        let planner = MzStandardTranslationTaskPlanningService::<_, _, ()>::new(
            reader,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let old_dependency = TerminologyDependency::new("魔法剣", "魔法剑");
        let corpus = StandardTranslationCorpus::new(vec![group(
            MzSource::data(StandardDataFile::Items),
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
            "魔法剑"
        );
        assert_eq!(preparation.invalidated(), 1);
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].expected_outputs()[0].id(), 0);
        assert_eq!(
            tasks[0].injected_terminology(),
            &[TerminologyDependency::new("魔法剣", "魔法之剑")]
        );
        assert_eq!(
            tasks[0].expected_outputs()[0].applied_placeholders().len(),
            1
        );
        assert!(matches!(
            tasks[0].groups()[0].units()[0].mode(),
            TranslationTaskUnitMode::Active { id: 0 }
        ));
        assert_eq!(
            tasks[0].messages()[0].content(),
            "# System\n完整且由外部提供。"
        );
        let user = tasks[0].messages()[1].content();
        assert!(user.contains("**魔法剣** → **魔法之剑**"));
        assert!(user.contains("### [0] name"));
        assert!(!user.contains("data/Items.json"));
        assert!(!user.contains("exact_location"));
    }

    #[tokio::test]
    async fn unknown_backslash_sequence_reaches_the_model_request_as_natural_text() {
        let planner = MzStandardTranslationTaskPlanningService::<_, _, ()>::new(
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
                    MzSource::data(StandardDataFile::Items),
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

    #[tokio::test]
    async fn target_language_uses_the_exact_resolved_system_prompt() {
        let planner = MzStandardTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources_for("ja", "zh-Hant"),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );

        let (_, _, tasks) = planner
            .plan(
                &project_with_languages("ja", "zh-Hant"),
                &profile_with_scope_concurrency(10_000, 2),
                StandardTranslationCorpus::new(vec![group(
                    MzSource::data(StandardDataFile::Items),
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
    async fn whole_maps_are_independent_semantic_scopes_even_with_large_capacity() {
        let planner = MzStandardTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let corpus = StandardTranslationCorpus::new(vec![
            group(MzSource::map(1), 0, "一番目", None, Vec::new()),
            group(MzSource::map(2), 0, "二番目", None, Vec::new()),
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
    async fn semantic_scope_compute_obeys_the_external_concurrency_limit() {
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let cpu = YieldingCpu {
            active: Arc::clone(&active),
            max_active: Arc::clone(&max_active),
        };
        let planner = MzStandardTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            cpu,
        );
        let corpus = StandardTranslationCorpus::new(
            (1..=4)
                .map(|map_id| {
                    group(
                        MzSource::map(map_id),
                        0,
                        format!("第{map_id}番目"),
                        None,
                        Vec::new(),
                    )
                })
                .collect(),
        );

        let (_, _, tasks) = planner
            .plan(
                &project(),
                &profile_with_scope_concurrency(10_000, 2),
                corpus,
                StandardTranslationInput::new(None, None),
            )
            .await
            .expect("四个 Map 范围应该在显式上限内并行规划")
            .into_parts();

        assert_eq!(tasks.len(), 4);
        assert_eq!(max_active.load(Ordering::SeqCst), 2);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn shuffled_map_groups_follow_display_event_page_list_and_command_order() {
        let planner = MzStandardTranslationTaskPlanningService::<_, _, ()>::new(
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
                    event_2_page_0_command_0,
                    event_1_page_1_command_0,
                    display,
                    event_1_page_0_command_2,
                ]),
                StandardTranslationInput::new(None, None),
            )
            .await
            .expect("乱序语料应该按 Map 真实结构稳定排序")
            .into_parts();

        assert_eq!(tasks.len(), 1);
        assert_eq!(
            tasks[0]
                .groups()
                .iter()
                .map(|group| group.group_location().to_string())
                .collect::<Vec<_>>(),
            [
                "data/Map001.json",
                "data/Map001.json.events[1].pages[0].list[2]",
                "data/Map001.json.events[1].pages[1].list[0]",
                "data/Map001.json.events[2].pages[0].list[0]",
            ]
        );
    }

    #[tokio::test]
    async fn fully_protected_source_text_becomes_virtual_context() {
        let placeholder_path = PathBuf::from("C:/input/placeholders.json");
        let reader = JsonTranslationPlanningResourceReadingService::new(
            MemoryFileReader {
                files: Arc::new(BTreeMap::from([(
                    placeholder_path.clone(),
                    r#"[{"scopes":["database_entry"],"pattern":"保護対象","label":"PROTECTED_TEXT"}]"#
                        .as_bytes()
                        .to_vec(),
                )])),
            },
            ImmediateCpu,
        );
        let planner = MzStandardTranslationTaskPlanningService::<_, _, ()>::new(
            reader,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let corpus = StandardTranslationCorpus::new(vec![
            group(
                MzSource::data(StandardDataFile::Items),
                1,
                "保護対象",
                None,
                Vec::new(),
            ),
            group(
                MzSource::data(StandardDataFile::Items),
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
        assert_eq!(tasks[0].groups().len(), 2);
        assert_eq!(
            tasks[0].groups()[0].units()[0].mode(),
            &TranslationTaskUnitMode::Virtual {
                reason: TranslationVirtualReason::FullyProtected
            }
        );
        assert_eq!(
            tasks[0].groups()[1].units()[0].mode(),
            &TranslationTaskUnitMode::Active { id: 0 }
        );
        assert_eq!(tasks[0].expected_outputs().len(), 1);
    }

    #[tokio::test]
    async fn global_deduplication_keeps_later_context_and_one_llm_owner() {
        let planner = MzStandardTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let first = group(
            MzSource::data(StandardDataFile::Items),
            1,
            "保存しますか？",
            None,
            Vec::new(),
        );
        let leader = first.assets()[0].identity().clone();
        let duplicate = map_group(TextGroupKind::Map, None, None, None, "保存しますか？");
        let target = duplicate.assets()[0].identity().clone();
        let neighbouring = map_group(
            TextGroupKind::EventDialogue,
            Some(1),
            Some(0),
            Some(0),
            "別の翻訳対象です。",
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
        assert_eq!(tasks.len(), 2);
        assert_eq!(
            tasks[0].expected_outputs()[0].propagation_targets(),
            &[target]
        );
        let duplicate_unit = &tasks[1].groups()[0].units()[0];
        assert!(matches!(
            duplicate_unit.mode(),
            TranslationTaskUnitMode::Virtual {
                reason: TranslationVirtualReason::Duplicate { leader: actual }
            } if actual.as_ref() == &leader
        ));
        assert!(tasks[1].messages()[1].content().contains("保存しますか？"));
        assert!(tasks[1].messages()[1].content().contains("[仅上下文]"));
        assert_eq!(tasks[1].expected_outputs().len(), 1);
    }

    #[tokio::test]
    async fn valid_existing_translation_reuses_without_creating_an_llm_task() {
        let planner = MzStandardTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let seed = group(
            MzSource::data(StandardDataFile::Items),
            1,
            "保存",
            Some("Save"),
            Vec::new(),
        );
        let target = group(
            MzSource::data(StandardDataFile::Skills),
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
            "Save"
        );
        assert_eq!(preparation.reuses()[0].targets().len(), 1);
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn conflicting_existing_translations_fail_before_a_plan_is_returned() {
        let planner = MzStandardTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let corpus = StandardTranslationCorpus::new(vec![
            group(
                MzSource::data(StandardDataFile::Items),
                1,
                "保存",
                Some("Save"),
                Vec::new(),
            ),
            group(
                MzSource::data(StandardDataFile::Skills),
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
            MzStandardTranslationTaskPlanningError::InvalidDeduplication(
                TranslationDeduplicationError::ConflictingReusableTranslations { .. }
            )
        ));
    }

    #[tokio::test]
    async fn capacity_splits_only_between_groups_inside_the_same_scope() {
        let planner = MzStandardTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let first = group(
            MzSource::data(StandardDataFile::Items),
            1,
            "あ".repeat(120),
            None,
            Vec::new(),
        );
        let second = group(
            MzSource::data(StandardDataFile::Items),
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
        let exact_single_size = single[0]
            .messages()
            .iter()
            .map(|message| message.content().chars().count())
            .sum::<usize>();

        let (_, _, split) = planner
            .plan(
                &project(),
                &profile(exact_single_size),
                StandardTranslationCorpus::new(vec![first, second]),
                StandardTranslationInput::new(None, None),
            )
            .await
            .expect("同一范围应在复合组边界切块")
            .into_parts();

        assert_eq!(split.len(), 2);
        assert_eq!(split[0].expected_outputs()[0].id(), 0);
        assert_eq!(split[1].expected_outputs()[0].id(), 0);
    }

    #[tokio::test]
    async fn translated_or_non_source_assets_are_context_only_and_do_not_create_empty_tasks() {
        let planner = MzStandardTranslationTaskPlanningService::<_, _, ()>::new(
            EmptyResources,
            translation_resources(),
            Pcre2PlaceholderService::new().expect("内置占位符应该可编译"),
            ImmediateCpu,
        );
        let corpus = StandardTranslationCorpus::new(vec![
            group(
                MzSource::data(StandardDataFile::Items),
                1,
                "翻訳済み",
                Some("已翻译"),
                Vec::new(),
            ),
            group(
                MzSource::data(StandardDataFile::Items),
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
