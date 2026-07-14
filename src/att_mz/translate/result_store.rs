#![allow(dead_code, reason = "标准翻译结果存储器尚未接入生产组合根")]

//! 标准翻译准备与单任务结果的 SQLite 持久化实现。

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::path::PathBuf;

use futures_util::stream::{self, StreamExt, TryStreamExt};

use crate::att_mz::location_codec::{MzLocationCodec, MzLocationCodecError};
use crate::att_mz::text::TextGroupKind;
use crate::project_database::StoredProjectRecord;
use crate::storage::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::storage::sqlite::{
    ExecuteTransactionError, SqliteBatch, SqliteCheckId, SqliteCommand, SqliteQuery,
    SqliteTransactionExecutor, SqliteTransactionPlan, SqliteTransactionStep, SqliteValue,
};

use super::standard::{
    StandardTranslationResultStore, TerminologyDependency, TranslationInvalidation,
    TranslationLeafIdentity, TranslationPatch, TranslationPlanPreparation, TranslationReuse,
    TranslationReuseSeed, TranslationReuseTarget, ValidatedTranslationTaskResult,
};

const TERMINOLOGY_DEPENDENCY_TABLE: &str = "translation_terminology_dependency";
const DELETE_TERMINOLOGY_DEPENDENCIES: &str =
    "DELETE FROM translation_terminology_dependency WHERE asset_table = ? AND exact_location = ?";
const INSERT_TERMINOLOGY_DEPENDENCY: &str = r#"INSERT INTO translation_terminology_dependency (
    asset_table,
    exact_location,
    term,
    term_translation
) VALUES (?, ?, ?, ?)"#;

/// 标准译文写入编码阶段的全部必填资源上限。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MzStandardTranslationResultStorageConfig {
    encode_concurrency: NonZeroUsize,
    leaves_per_encode_job: NonZeroUsize,
}

impl MzStandardTranslationResultStorageConfig {
    pub(crate) const fn new(
        encode_concurrency: NonZeroUsize,
        leaves_per_encode_job: NonZeroUsize,
    ) -> Self {
        Self {
            encode_concurrency,
            leaves_per_encode_job,
        }
    }

    pub(crate) const fn encode_concurrency(self) -> NonZeroUsize {
        self.encode_concurrency
    }

    pub(crate) const fn leaves_per_encode_job(self) -> NonZeroUsize {
        self.leaves_per_encode_job
    }
}

/// 以乐观并发检查和短事务保存标准译文状态。
pub(crate) struct MzStandardTranslationResultStorageService<S, C> {
    sqlite: S,
    cpu: C,
    config: MzStandardTranslationResultStorageConfig,
}

impl<S, C> MzStandardTranslationResultStorageService<S, C> {
    pub(crate) fn new(sqlite: S, cpu: C, config: MzStandardTranslationResultStorageConfig) -> Self {
        Self {
            sqlite,
            cpu,
            config,
        }
    }
}

impl<S, C> StandardTranslationResultStore for MzStandardTranslationResultStorageService<S, C>
where
    S: SqliteTransactionExecutor,
    C: CpuTaskExecutor,
{
    type Error = MzStandardTranslationResultStorageError<S::Error, C::Error>;

    async fn apply_preparation(
        &self,
        project: &StoredProjectRecord,
        preparation: TranslationPlanPreparation,
    ) -> Result<(), Self::Error> {
        let (invalidations, reuses) = preparation.into_parts();
        if invalidations.is_empty() && reuses.is_empty() {
            return Ok(());
        }

        let invalidation_batches =
            split_owned(invalidations, self.config.leaves_per_encode_job.get());
        let encoded_invalidations = self
            .encode_batches(invalidation_batches, encode_invalidation_batch)
            .await?;
        let (reuse_count, reuse_inputs) = build_reuse_encoding_inputs(reuses)
            .map_err(MzStandardTranslationResultStorageError::InvalidPlan)?;
        let reuse_batches = split_owned(reuse_inputs, self.config.leaves_per_encode_job.get());
        let encoded_reuse_parts = self
            .encode_batches(reuse_batches, encode_reuse_part_batch)
            .await?;
        let encoded = self
            .cpu
            .execute(move || {
                let encoded_reuses = assemble_encoded_reuses(reuse_count, encoded_reuse_parts)?;
                ensure_valid_preparation(encoded_invalidations, encoded_reuses)
            })
            .await
            .map_err(MzStandardTranslationResultStorageError::ScheduleEncoding)?
            .map_err(MzStandardTranslationResultStorageError::InvalidPlan)?;
        let plan = build_preparation_plan(encoded);
        self.execute(project.database_path().to_path_buf(), plan)
            .await
    }

    async fn commit(
        &self,
        project: &StoredProjectRecord,
        result: ValidatedTranslationTaskResult,
    ) -> Result<(), Self::Error> {
        let patches = result.into_updates();
        if patches.is_empty() {
            return Err(MzStandardTranslationResultStorageError::InvalidPlan(
                ResultStoragePlanError::EmptyTaskResult,
            ));
        }

        let patch_inputs = build_patch_encoding_inputs(patches);
        let batches = split_owned(patch_inputs, self.config.leaves_per_encode_job.get());
        let encoded = self
            .encode_batches(batches, encode_patch_leaf_batch)
            .await?;
        let encoded = self
            .cpu
            .execute(move || ensure_unique_patches(encoded))
            .await
            .map_err(MzStandardTranslationResultStorageError::ScheduleEncoding)?
            .map_err(MzStandardTranslationResultStorageError::InvalidPlan)?;
        let plan = build_commit_plan(encoded);
        self.execute(project.database_path().to_path_buf(), plan)
            .await
    }
}

impl<S, C> MzStandardTranslationResultStorageService<S, C>
where
    S: SqliteTransactionExecutor,
    C: CpuTaskExecutor,
{
    async fn encode_batches<I, O, F>(
        &self,
        batches: Vec<Vec<I>>,
        encode: F,
    ) -> Result<Vec<O>, MzStandardTranslationResultStorageError<S::Error, C::Error>>
    where
        I: Send + 'static,
        O: Send + 'static,
        F: Fn(Vec<I>) -> Result<Vec<O>, ResultStoragePlanError> + Copy + Send + 'static,
    {
        let encoded_batches = stream::iter(batches.into_iter().map(|batch| async move {
            self.cpu
                .execute(move || encode(batch))
                .await
                .map_err(MzStandardTranslationResultStorageError::ScheduleEncoding)?
                .map_err(MzStandardTranslationResultStorageError::InvalidPlan)
        }))
        .buffered(self.config.encode_concurrency.get())
        .try_collect::<Vec<_>>()
        .await?;

        Ok(encoded_batches.into_iter().flatten().collect())
    }

    async fn execute(
        &self,
        database_path: PathBuf,
        plan: SqliteTransactionPlan,
    ) -> Result<(), MzStandardTranslationResultStorageError<S::Error, C::Error>> {
        self.sqlite
            .execute_transaction(database_path.clone(), plan)
            .await
            .map_err(|error| map_transaction_error(database_path, error))
    }
}

/// 标准译文持久化职责的明确失败语义。
#[derive(Debug)]
pub(crate) enum MzStandardTranslationResultStorageError<S, C> {
    ScheduleEncoding(CpuTaskExecutionError<C>),
    InvalidPlan(ResultStoragePlanError),
    DatabaseNotFound {
        database_path: PathBuf,
    },
    StalePlan {
        database_path: PathBuf,
        check_id: SqliteCheckId,
    },
    NotCommitted {
        database_path: PathBuf,
        source: S,
    },
    OutcomeUnknown {
        database_path: PathBuf,
        source: S,
    },
}

impl<S, C> fmt::Display for MzStandardTranslationResultStorageError<S, C>
where
    S: fmt::Display,
    C: fmt::Display,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ScheduleEncoding(source) => write!(formatter, "译文写入编码任务失败：{source}"),
            Self::InvalidPlan(source) => write!(formatter, "译文写入计划无效：{source}"),
            Self::DatabaseNotFound { database_path } => {
                write!(formatter, "项目数据库不存在：{}", database_path.display())
            }
            Self::StalePlan {
                database_path,
                check_id,
            } => write!(
                formatter,
                "翻译计划建立后资产已发生变化（{}，检查 {}）",
                database_path.display(),
                check_id.as_str()
            ),
            Self::NotCommitted {
                database_path,
                source,
            } => write!(
                formatter,
                "译文事务未提交到 {}：{source}",
                database_path.display()
            ),
            Self::OutcomeUnknown {
                database_path,
                source,
            } => write!(
                formatter,
                "无法确认译文事务是否已提交到 {}：{source}",
                database_path.display()
            ),
        }
    }
}

impl<S, C> Error for MzStandardTranslationResultStorageError<S, C>
where
    S: Error + 'static,
    C: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ScheduleEncoding(source) => Some(source),
            Self::InvalidPlan(source) => Some(source),
            Self::NotCommitted { source, .. } | Self::OutcomeUnknown { source, .. } => Some(source),
            Self::DatabaseNotFound { .. } | Self::StalePlan { .. } => None,
        }
    }
}

/// 构造安全事务计划前发现的内部结果不变量破坏。
#[derive(Debug)]
pub(crate) enum ResultStoragePlanError {
    Location(MzLocationCodecError),
    EmptyTaskResult,
    EmptyReuseTargets,
    IncompleteReuseEncoding,
    BlankTranslation,
    BlankTerminologyDependency,
    UntranslatedLeafHasTerminologyDependencies,
    MismatchedReuseOriginal,
    MismatchedPropagationOriginal,
    DuplicateLeaf,
    DuplicateTerminologyDependency { term: String },
    ContradictoryTerminologyDependency { term: String },
}

impl fmt::Display for ResultStoragePlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Location(source) => source.fmt(formatter),
            Self::EmptyTaskResult => formatter.write_str("任务结果不包含任何译文"),
            Self::EmptyReuseTargets => formatter.write_str("译文复用计划不包含任何目标"),
            Self::IncompleteReuseEncoding => formatter.write_str("译文复用编码结果不完整"),
            Self::BlankTranslation => formatter.write_str("任务结果包含空白译文"),
            Self::BlankTerminologyDependency => {
                formatter.write_str("任务结果的术语依赖为空或带首尾空白")
            }
            Self::UntranslatedLeafHasTerminologyDependencies => {
                formatter.write_str("未翻译的复用目标不应持有术语依赖")
            }
            Self::MismatchedReuseOriginal => formatter.write_str("译文复用种子与目标的原文不一致"),
            Self::MismatchedPropagationOriginal => {
                formatter.write_str("译文代表与传播目标的原文不一致")
            }
            Self::DuplicateLeaf => formatter.write_str("同一事务重复修改同一文本叶子"),
            Self::DuplicateTerminologyDependency { term } => {
                write!(formatter, "重复记录术语依赖：{term}")
            }
            Self::ContradictoryTerminologyDependency { term } => {
                write!(formatter, "同一术语记录了矛盾译词：{term}")
            }
        }
    }
}

impl Error for ResultStoragePlanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Location(source) => Some(source),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
enum AssetTable {
    Entry,
    SystemText,
    MapText,
    TextBody,
    PluginParam,
}

impl AssetTable {
    const fn for_kind(kind: TextGroupKind) -> Self {
        match kind {
            TextGroupKind::DatabaseEntry => Self::Entry,
            TextGroupKind::System => Self::SystemText,
            TextGroupKind::Map => Self::MapText,
            TextGroupKind::EventDialogue
            | TextGroupKind::EventChoices
            | TextGroupKind::EventScrollingText
            | TextGroupKind::EventCommand => Self::TextBody,
            TextGroupKind::PluginParameter => Self::PluginParam,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Entry => "entry",
            Self::SystemText => "system_text",
            Self::MapText => "map_text",
            Self::TextBody => "text_body",
            Self::PluginParam => "plugin_param",
        }
    }
}

struct EncodedIdentity {
    table: AssetTable,
    unit_type: Option<&'static str>,
    exact_location: String,
    group_location: String,
    original_text: String,
}

struct EncodedInvalidation {
    identity: EncodedIdentity,
    expected_translation: String,
    expected_dependencies: Vec<TerminologyDependency>,
}

enum ReuseEncodingInput {
    Seed {
        reuse_index: usize,
        seed: TranslationReuseSeed,
    },
    Target {
        reuse_index: usize,
        target: TranslationReuseTarget,
    },
}

enum EncodedReusePart {
    Seed {
        reuse_index: usize,
        seed: EncodedReuseSeed,
    },
    Target {
        reuse_index: usize,
        target: EncodedReuseTarget,
    },
}

struct EncodedReuseSeed {
    identity: EncodedIdentity,
    expected_translation: String,
    expected_dependencies: Vec<TerminologyDependency>,
}

struct EncodedReuseTarget {
    identity: EncodedIdentity,
    expected_translation: Option<String>,
    expected_dependencies: Vec<TerminologyDependency>,
}

struct EncodedReuse {
    seed: EncodedReuseSeed,
    targets: Vec<EncodedReuseTarget>,
}

struct EncodedPreparation {
    invalidations: Vec<EncodedInvalidation>,
    reuses: Vec<EncodedReuse>,
}

struct PatchEncodingInput {
    identity: TranslationLeafIdentity,
    leader_original: String,
    translation: String,
    dependencies: Vec<TerminologyDependency>,
}

struct EncodedPatch {
    identity: EncodedIdentity,
    translation: String,
    dependencies: Vec<TerminologyDependency>,
}

fn split_owned<T>(values: Vec<T>, values_per_job: usize) -> Vec<Vec<T>> {
    let mut values = values.into_iter();
    let mut batches = Vec::new();
    loop {
        let batch = values.by_ref().take(values_per_job).collect::<Vec<_>>();
        if batch.is_empty() {
            return batches;
        }
        batches.push(batch);
    }
}

fn encode_invalidation_batch(
    invalidations: Vec<TranslationInvalidation>,
) -> Result<Vec<EncodedInvalidation>, ResultStoragePlanError> {
    invalidations
        .into_iter()
        .map(|invalidation| {
            if invalidation.expected_translation().trim().is_empty() {
                return Err(ResultStoragePlanError::BlankTranslation);
            }
            let dependencies =
                normalize_dependencies(invalidation.expected_terminology_dependencies().to_vec())?;
            Ok(EncodedInvalidation {
                identity: encode_identity(invalidation.identity())?,
                expected_translation: invalidation.expected_translation().to_owned(),
                expected_dependencies: dependencies,
            })
        })
        .collect()
}

fn build_reuse_encoding_inputs(
    reuses: Vec<TranslationReuse>,
) -> Result<(usize, Vec<ReuseEncodingInput>), ResultStoragePlanError> {
    let reuse_count = reuses.len();
    let mut inputs = Vec::new();
    for (reuse_index, reuse_plan) in reuses.into_iter().enumerate() {
        if reuse_plan.targets().is_empty() {
            return Err(ResultStoragePlanError::EmptyReuseTargets);
        }
        inputs.push(ReuseEncodingInput::Seed {
            reuse_index,
            seed: reuse_plan.seed().clone(),
        });
        inputs.extend(reuse_plan.targets().iter().cloned().map(|target| {
            ReuseEncodingInput::Target {
                reuse_index,
                target,
            }
        }));
    }
    Ok((reuse_count, inputs))
}

fn encode_reuse_part_batch(
    inputs: Vec<ReuseEncodingInput>,
) -> Result<Vec<EncodedReusePart>, ResultStoragePlanError> {
    inputs
        .into_iter()
        .map(|input| match input {
            ReuseEncodingInput::Seed { reuse_index, seed } => {
                if seed.expected_translation().trim().is_empty() {
                    return Err(ResultStoragePlanError::BlankTranslation);
                }
                let expected_dependencies =
                    normalize_dependencies(seed.expected_terminology_dependencies().to_vec())?;
                Ok(EncodedReusePart::Seed {
                    reuse_index,
                    seed: EncodedReuseSeed {
                        identity: encode_identity(seed.identity())?,
                        expected_translation: seed.expected_translation().to_owned(),
                        expected_dependencies,
                    },
                })
            }
            ReuseEncodingInput::Target {
                reuse_index,
                target,
            } => {
                if target
                    .expected_translation()
                    .is_some_and(|translation| translation.trim().is_empty())
                {
                    return Err(ResultStoragePlanError::BlankTranslation);
                }
                if target.expected_translation().is_none()
                    && !target.expected_terminology_dependencies().is_empty()
                {
                    return Err(ResultStoragePlanError::UntranslatedLeafHasTerminologyDependencies);
                }
                Ok(EncodedReusePart::Target {
                    reuse_index,
                    target: EncodedReuseTarget {
                        identity: encode_identity(target.identity())?,
                        expected_translation: target.expected_translation().map(str::to_owned),
                        expected_dependencies: normalize_dependencies(
                            target.expected_terminology_dependencies().to_vec(),
                        )?,
                    },
                })
            }
        })
        .collect()
}

fn assemble_encoded_reuses(
    reuse_count: usize,
    parts: Vec<EncodedReusePart>,
) -> Result<Vec<EncodedReuse>, ResultStoragePlanError> {
    let mut seeds = (0..reuse_count).map(|_| None).collect::<Vec<_>>();
    let mut targets = (0..reuse_count).map(|_| Vec::new()).collect::<Vec<_>>();
    for part in parts {
        match part {
            EncodedReusePart::Seed { reuse_index, seed } => {
                let Some(slot) = seeds.get_mut(reuse_index) else {
                    return Err(ResultStoragePlanError::IncompleteReuseEncoding);
                };
                if slot.replace(seed).is_some() {
                    return Err(ResultStoragePlanError::IncompleteReuseEncoding);
                }
            }
            EncodedReusePart::Target {
                reuse_index,
                target,
            } => {
                let Some(group) = targets.get_mut(reuse_index) else {
                    return Err(ResultStoragePlanError::IncompleteReuseEncoding);
                };
                group.push(target);
            }
        }
    }

    seeds
        .into_iter()
        .zip(targets)
        .map(|(seed, targets)| {
            let seed = seed.ok_or(ResultStoragePlanError::IncompleteReuseEncoding)?;
            if targets.is_empty() {
                return Err(ResultStoragePlanError::EmptyReuseTargets);
            }
            if targets
                .iter()
                .any(|target| target.identity.original_text != seed.identity.original_text)
            {
                return Err(ResultStoragePlanError::MismatchedReuseOriginal);
            }
            Ok(EncodedReuse { seed, targets })
        })
        .collect()
}

fn build_patch_encoding_inputs(patches: Vec<TranslationPatch>) -> Vec<PatchEncodingInput> {
    let mut inputs = Vec::new();
    for patch in patches {
        let leader_original = patch.identity().original_text().to_owned();
        inputs.push(PatchEncodingInput {
            identity: patch.identity().clone(),
            leader_original: leader_original.clone(),
            translation: patch.translation().to_owned(),
            dependencies: patch.terminology_dependencies().to_vec(),
        });
        inputs.extend(patch.propagation_targets().iter().cloned().map(|identity| {
            PatchEncodingInput {
                identity,
                leader_original: leader_original.clone(),
                translation: patch.translation().to_owned(),
                dependencies: patch.terminology_dependencies().to_vec(),
            }
        }));
    }
    inputs
}

fn encode_patch_leaf_batch(
    inputs: Vec<PatchEncodingInput>,
) -> Result<Vec<EncodedPatch>, ResultStoragePlanError> {
    inputs
        .into_iter()
        .map(|input| {
            if input.translation.trim().is_empty() {
                return Err(ResultStoragePlanError::BlankTranslation);
            }
            if input.identity.original_text() != input.leader_original {
                return Err(ResultStoragePlanError::MismatchedPropagationOriginal);
            }
            Ok(EncodedPatch {
                identity: encode_identity(&input.identity)?,
                translation: input.translation,
                dependencies: normalize_dependencies(input.dependencies)?,
            })
        })
        .collect()
}

fn encode_identity(
    identity: &TranslationLeafIdentity,
) -> Result<EncodedIdentity, ResultStoragePlanError> {
    Ok(EncodedIdentity {
        table: AssetTable::for_kind(identity.kind()),
        unit_type: unit_type_for_kind(identity.kind()),
        exact_location: MzLocationCodec::encode(identity.exact_location())
            .map_err(ResultStoragePlanError::Location)?,
        group_location: MzLocationCodec::encode(identity.group_location())
            .map_err(ResultStoragePlanError::Location)?,
        original_text: identity.original_text().to_owned(),
    })
}

const fn unit_type_for_kind(kind: TextGroupKind) -> Option<&'static str> {
    match kind {
        TextGroupKind::EventDialogue => Some("dialogue"),
        TextGroupKind::EventChoices => Some("choices"),
        TextGroupKind::EventScrollingText => Some("scrolling_text"),
        TextGroupKind::EventCommand => Some("event_command"),
        TextGroupKind::DatabaseEntry
        | TextGroupKind::System
        | TextGroupKind::Map
        | TextGroupKind::PluginParameter => None,
    }
}

fn normalize_dependencies(
    dependencies: Vec<TerminologyDependency>,
) -> Result<Vec<TerminologyDependency>, ResultStoragePlanError> {
    let mut normalized = BTreeMap::<String, String>::new();
    for dependency in dependencies {
        if dependency.term().trim().is_empty()
            || dependency.term().trim() != dependency.term()
            || dependency.translation().trim().is_empty()
            || dependency.translation().trim() != dependency.translation()
        {
            return Err(ResultStoragePlanError::BlankTerminologyDependency);
        }
        match normalized.entry(dependency.term().to_owned()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(dependency.translation().to_owned());
            }
            std::collections::btree_map::Entry::Occupied(entry)
                if entry.get() == dependency.translation() =>
            {
                return Err(ResultStoragePlanError::DuplicateTerminologyDependency {
                    term: dependency.term().to_owned(),
                });
            }
            std::collections::btree_map::Entry::Occupied(_) => {
                return Err(ResultStoragePlanError::ContradictoryTerminologyDependency {
                    term: dependency.term().to_owned(),
                });
            }
        }
    }
    Ok(normalized
        .into_iter()
        .map(|(term, translation)| TerminologyDependency::new(term, translation))
        .collect())
}

fn ensure_valid_preparation(
    invalidations: Vec<EncodedInvalidation>,
    reuses: Vec<EncodedReuse>,
) -> Result<EncodedPreparation, ResultStoragePlanError> {
    ensure_unique_leaf_keys(
        invalidations
            .iter()
            .map(|value| &value.identity)
            .chain(reuses.iter().map(|reuse_plan| &reuse_plan.seed.identity))
            .chain(
                reuses
                    .iter()
                    .flat_map(|reuse_plan| reuse_plan.targets.iter())
                    .map(|target| &target.identity),
            )
            .map(identity_key),
    )?;
    Ok(EncodedPreparation {
        invalidations,
        reuses,
    })
}

fn ensure_unique_patches(
    patches: Vec<EncodedPatch>,
) -> Result<Vec<EncodedPatch>, ResultStoragePlanError> {
    ensure_unique_leaf_keys(patches.iter().map(|value| {
        (
            &value.identity.table,
            value.identity.exact_location.as_str(),
        )
    }))?;
    Ok(patches)
}

fn ensure_unique_leaf_keys<'a>(
    keys: impl Iterator<Item = (&'a AssetTable, &'a str)>,
) -> Result<(), ResultStoragePlanError> {
    let mut seen = std::collections::BTreeSet::new();
    for (table, exact_location) in keys {
        if !seen.insert((table.name(), exact_location)) {
            return Err(ResultStoragePlanError::DuplicateLeaf);
        }
    }
    Ok(())
}

fn identity_key(identity: &EncodedIdentity) -> (&AssetTable, &str) {
    (&identity.table, identity.exact_location.as_str())
}

fn build_preparation_plan(preparation: EncodedPreparation) -> SqliteTransactionPlan {
    let target_count = preparation
        .reuses
        .iter()
        .map(|reuse_plan| reuse_plan.targets.len())
        .sum::<usize>();
    let mut steps = Vec::with_capacity(
        preparation.invalidations.len() * 3 + preparation.reuses.len() + target_count * 4 + 1,
    );

    for (index, invalidation) in preparation.invalidations.iter().enumerate() {
        steps.push(SqliteTransactionStep::RequireNoRows {
            check_id: stale_check_id("preparation_invalidation", index),
            query: snapshot_stale_query(
                &invalidation.identity,
                Some(invalidation.expected_translation.as_str()),
                &invalidation.expected_dependencies,
            ),
        });
    }
    for (index, reuse_plan) in preparation.reuses.iter().enumerate() {
        steps.push(SqliteTransactionStep::RequireNoRows {
            check_id: stale_check_id("preparation_reuse_seed", index),
            query: snapshot_stale_query(
                &reuse_plan.seed.identity,
                Some(reuse_plan.seed.expected_translation.as_str()),
                &reuse_plan.seed.expected_dependencies,
            ),
        });
    }
    for (index, target) in preparation
        .reuses
        .iter()
        .flat_map(|reuse_plan| reuse_plan.targets.iter())
        .enumerate()
    {
        steps.push(SqliteTransactionStep::RequireNoRows {
            check_id: stale_check_id("preparation_reuse_target", index),
            query: snapshot_stale_query(
                &target.identity,
                target.expected_translation.as_deref(),
                &target.expected_dependencies,
            ),
        });
    }

    for invalidation in preparation.invalidations {
        steps.push(execute(
            DELETE_TERMINOLOGY_DEPENDENCIES,
            vec![
                text(invalidation.identity.table.name()),
                text(invalidation.identity.exact_location.clone()),
            ],
        ));
        steps.push(execute(
            &format!(
                "UPDATE {} SET translation = NULL WHERE exact_location = ?",
                invalidation.identity.table.name()
            ),
            vec![text(invalidation.identity.exact_location)],
        ));
    }

    let mut dependency_parameter_sets = Vec::new();
    for reuse_plan in preparation.reuses {
        for target in reuse_plan.targets {
            steps.push(execute(
                DELETE_TERMINOLOGY_DEPENDENCIES,
                vec![
                    text(target.identity.table.name()),
                    text(target.identity.exact_location.clone()),
                ],
            ));
            steps.push(execute(
                &format!(
                    "UPDATE {} SET translation = ? WHERE exact_location = ?",
                    target.identity.table.name()
                ),
                vec![
                    text(reuse_plan.seed.expected_translation.clone()),
                    text(target.identity.exact_location.clone()),
                ],
            ));
            for dependency in &reuse_plan.seed.expected_dependencies {
                dependency_parameter_sets.push(vec![
                    text(target.identity.table.name()),
                    text(target.identity.exact_location.clone()),
                    text(dependency.term()),
                    text(dependency.translation()),
                ]);
            }
        }
    }
    if !dependency_parameter_sets.is_empty() {
        steps.push(SqliteTransactionStep::ExecuteMany(SqliteBatch::new(
            INSERT_TERMINOLOGY_DEPENDENCY,
            dependency_parameter_sets,
        )));
    }
    SqliteTransactionPlan::new(steps)
}

fn build_commit_plan(patches: Vec<EncodedPatch>) -> SqliteTransactionPlan {
    let mut steps = Vec::with_capacity(patches.len() * 2 + 1);
    for (index, patch) in patches.iter().enumerate() {
        steps.push(SqliteTransactionStep::RequireNoRows {
            check_id: stale_check_id("commit", index),
            query: commit_stale_query(patch),
        });
    }

    let mut dependency_parameter_sets = Vec::new();
    for patch in patches {
        steps.push(execute(
            &format!(
                "UPDATE {} SET translation = ? WHERE exact_location = ?",
                patch.identity.table.name()
            ),
            vec![
                text(patch.translation),
                text(patch.identity.exact_location.clone()),
            ],
        ));
        for dependency in patch.dependencies {
            dependency_parameter_sets.push(vec![
                text(patch.identity.table.name()),
                text(patch.identity.exact_location.clone()),
                text(dependency.term()),
                text(dependency.translation()),
            ]);
        }
    }
    if !dependency_parameter_sets.is_empty() {
        steps.push(SqliteTransactionStep::ExecuteMany(SqliteBatch::new(
            INSERT_TERMINOLOGY_DEPENDENCY,
            dependency_parameter_sets,
        )));
    }
    SqliteTransactionPlan::new(steps)
}

fn snapshot_stale_query(
    identity: &EncodedIdentity,
    expected_translation: Option<&str>,
    expected_dependencies: &[TerminologyDependency],
) -> SqliteQuery {
    let (expected_cte, mut parameters) = expected_dependencies_cte(expected_dependencies);
    parameters.extend([
        text(identity.exact_location.clone()),
        text(identity.group_location.clone()),
        text(identity.original_text.clone()),
    ]);
    let unit_type_predicate = if let Some(unit_type) = identity.unit_type {
        parameters.push(text(unit_type));
        "\n      AND unit_type = ?"
    } else {
        ""
    };
    let translation_predicate = if let Some(expected_translation) = expected_translation {
        parameters.push(text(expected_translation));
        "\n      AND translation = ?"
    } else {
        "\n      AND translation IS NULL"
    };
    parameters.extend([
        text(identity.table.name()),
        text(identity.exact_location.clone()),
    ]);
    parameters.extend([
        text(identity.table.name()),
        text(identity.exact_location.clone()),
    ]);
    SqliteQuery::new(
        format!(
            r#"WITH expected(term, term_translation) AS ({expected_cte})
SELECT 1
WHERE NOT EXISTS (
    SELECT 1
    FROM {asset_table}
    WHERE exact_location = ?
      AND group_location = ?
      AND original_text = ?
      {unit_type_predicate}
      {translation_predicate}
)
OR EXISTS (
    SELECT term, term_translation
    FROM {dependency_table}
    WHERE asset_table = ? AND exact_location = ?
    EXCEPT
    SELECT term, term_translation FROM expected
)
OR EXISTS (
    SELECT term, term_translation FROM expected
    EXCEPT
    SELECT term, term_translation
    FROM {dependency_table}
    WHERE asset_table = ? AND exact_location = ?
)"#,
            asset_table = identity.table.name(),
            dependency_table = TERMINOLOGY_DEPENDENCY_TABLE,
        ),
        parameters,
    )
}

fn commit_stale_query(patch: &EncodedPatch) -> SqliteQuery {
    let identity = &patch.identity;
    let mut parameters = vec![
        text(identity.exact_location.clone()),
        text(identity.group_location.clone()),
        text(identity.original_text.clone()),
    ];
    let unit_type_predicate = if let Some(unit_type) = identity.unit_type {
        parameters.push(text(unit_type));
        "\n      AND unit_type = ?"
    } else {
        ""
    };
    parameters.extend([
        text(identity.table.name()),
        text(identity.exact_location.clone()),
    ]);
    SqliteQuery::new(
        format!(
            r#"SELECT 1
WHERE NOT EXISTS (
    SELECT 1
    FROM {asset_table}
    WHERE exact_location = ?
      AND group_location = ?
      AND original_text = ?
      {unit_type_predicate}
      AND translation IS NULL
)
OR EXISTS (
    SELECT 1
    FROM {dependency_table}
    WHERE asset_table = ? AND exact_location = ?
)"#,
            asset_table = identity.table.name(),
            dependency_table = TERMINOLOGY_DEPENDENCY_TABLE,
        ),
        parameters,
    )
}

fn expected_dependencies_cte(dependencies: &[TerminologyDependency]) -> (String, Vec<SqliteValue>) {
    if dependencies.is_empty() {
        return ("SELECT NULL, NULL WHERE 0".to_owned(), Vec::new());
    }

    let placeholders = std::iter::repeat_n("(?, ?)", dependencies.len())
        .collect::<Vec<_>>()
        .join(", ");
    let parameters = dependencies
        .iter()
        .flat_map(|dependency| [text(dependency.term()), text(dependency.translation())])
        .collect();
    (format!("VALUES {placeholders}"), parameters)
}

fn stale_check_id(stage: &str, index: usize) -> SqliteCheckId {
    SqliteCheckId::new(format!("mz_translation_{stage}_stale_{index}"))
}

fn execute(statement: &str, parameters: Vec<SqliteValue>) -> SqliteTransactionStep {
    SqliteTransactionStep::Execute(SqliteCommand::new(statement, parameters))
}

fn text(value: impl Into<String>) -> SqliteValue {
    SqliteValue::Text(value.into())
}

fn map_transaction_error<S, C>(
    database_path: PathBuf,
    error: ExecuteTransactionError<S>,
) -> MzStandardTranslationResultStorageError<S, C> {
    match error {
        ExecuteTransactionError::NotFound => {
            MzStandardTranslationResultStorageError::DatabaseNotFound { database_path }
        }
        ExecuteTransactionError::RequirementFailed { check_id } => {
            MzStandardTranslationResultStorageError::StalePlan {
                database_path,
                check_id,
            }
        }
        ExecuteTransactionError::NotCommitted(source) => {
            MzStandardTranslationResultStorageError::NotCommitted {
                database_path,
                source,
            }
        }
        ExecuteTransactionError::OutcomeUnknown(source) => {
            MzStandardTranslationResultStorageError::OutcomeUnknown {
                database_path,
                source,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use crate::att_mz::ProjectName;
    use crate::att_mz::text::{MzLocation, MzLocationStep, MzSource, StandardDataFile};

    use super::super::standard::{
        StandardTranslationTaskIndex, TranslationReuseSeed, TranslationReuseTarget,
    };
    use super::*;

    type TransactionResponse = Result<(), ExecuteTransactionError<FakeError>>;
    type SharedTransactionResponse = Arc<Mutex<Option<TransactionResponse>>>;

    #[derive(Clone)]
    struct RecordingSqlite {
        calls: Arc<Mutex<Vec<(PathBuf, SqliteTransactionPlan)>>>,
        response: SharedTransactionResponse,
    }

    impl SqliteTransactionExecutor for RecordingSqlite {
        type Error = FakeError;

        fn execute_transaction(
            &self,
            path: PathBuf,
            plan: SqliteTransactionPlan,
        ) -> impl Future<Output = Result<(), ExecuteTransactionError<Self::Error>>> + Send {
            self.calls
                .lock()
                .expect("事务调用锁不应中毒")
                .push((path, plan));
            let response = self
                .response
                .lock()
                .expect("事务响应锁不应中毒")
                .take()
                .unwrap_or(Ok(()));
            async move { response }
        }
    }

    #[derive(Clone)]
    struct RecordingCpu {
        calls: Arc<AtomicUsize>,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
    }

    impl CpuTaskExecutor for RecordingCpu {
        type Error = FakeError;

        async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            tokio::task::yield_now().await;
            let output = task();
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(output)
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeError {}

    #[test]
    fn config_preserves_explicit_non_zero_limits() {
        let config = MzStandardTranslationResultStorageConfig::new(non_zero(2), non_zero(16));

        assert_eq!(config.encode_concurrency().get(), 2);
        assert_eq!(config.leaves_per_encode_job().get(), 16);
    }

    #[tokio::test]
    async fn preparation_checks_expected_translation_and_exact_dependency_set_before_clearing() {
        let harness = Harness::new(None);
        let service = harness.service(2, 1);
        let preparation = TranslationPlanPreparation::new(
            vec![TranslationInvalidation::new(
                identity("name", "宝剑"),
                "Sword",
                vec![TerminologyDependency::new("宝剑", "Sword")],
            )],
            Vec::new(),
        );

        service
            .apply_preparation(&project(), preparation)
            .await
            .expect("准备事务应该成功");

        let calls = harness.calls.lock().expect("事务调用锁不应中毒");
        assert_eq!(calls.len(), 1);
        let steps = calls[0].1.steps();
        assert!(matches!(
            &steps[0],
            SqliteTransactionStep::RequireNoRows { query, .. }
                if query.statement().contains("EXCEPT")
                    && query.statement().contains("translation = ?")
                    && query.parameters().contains(&text("Sword"))
                    && query.parameters().contains(&text("宝剑"))
        ));
        assert!(steps.iter().any(|step| matches!(
            step,
            SqliteTransactionStep::Execute(command)
                if command.statement() == DELETE_TERMINOLOGY_DEPENDENCIES
        )));
        assert!(steps.iter().any(|step| matches!(
            step,
            SqliteTransactionStep::Execute(command)
                if command.statement().contains("SET translation = NULL")
        )));
    }

    #[tokio::test]
    async fn preparation_checks_all_reuse_snapshots_before_copying_translation_and_dependencies() {
        let harness = Harness::new(None);
        let service = harness.service(2, 1);
        let seed_dependencies = vec![TerminologyDependency::new("宝剑", "Sword")];
        let preparation = TranslationPlanPreparation::new(
            Vec::new(),
            vec![TranslationReuse::new(
                TranslationReuseSeed::new(
                    identity_at(10, "name", "宝剑"),
                    "Sword",
                    seed_dependencies.clone(),
                ),
                vec![
                    TranslationReuseTarget::new(identity_at(11, "name", "宝剑"), None, Vec::new()),
                    TranslationReuseTarget::new(
                        identity_at(12, "name", "宝剑"),
                        Some("Old sword".to_owned()),
                        vec![TerminologyDependency::new("宝剑", "Old sword")],
                    ),
                ],
            )],
        );

        service
            .apply_preparation(&project(), preparation)
            .await
            .expect("复用准备事务应该成功");

        assert_eq!(
            harness.cpu_calls.load(Ordering::SeqCst),
            4,
            "一个种子和两个目标应按三个物理叶子分批，随后执行一次组装校验"
        );
        assert_eq!(harness.max_cpu_active.load(Ordering::SeqCst), 2);
        let calls = harness.calls.lock().expect("事务调用锁不应中毒");
        let steps = calls[0].1.steps();
        assert_eq!(
            steps
                .iter()
                .take_while(|step| matches!(step, SqliteTransactionStep::RequireNoRows { .. }))
                .count(),
            3,
            "种子和所有目标必须在首次写入前完成 CAS"
        );
        let queries = steps[..3]
            .iter()
            .map(|step| match step {
                SqliteTransactionStep::RequireNoRows { query, .. } => query,
                _ => unreachable!("前三步必须是快照检查"),
            })
            .collect::<Vec<_>>();
        assert!(queries[0].statement().contains("translation = ?"));
        assert!(queries[1].statement().contains("translation IS NULL"));
        assert!(queries[2].statement().contains("translation = ?"));
        assert!(
            queries
                .iter()
                .all(|query| query.statement().contains("EXCEPT"))
        );

        let translation_updates = steps
            .iter()
            .filter_map(|step| match step {
                SqliteTransactionStep::Execute(command)
                    if command.statement().contains("SET translation = ?") =>
                {
                    Some(command)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(translation_updates.len(), 2);
        assert!(
            translation_updates
                .iter()
                .all(|command| command.parameters()[0] == text("Sword"))
        );
        assert!(steps.iter().any(|step| matches!(
            step,
            SqliteTransactionStep::ExecuteMany(batch)
                if batch.statement() == INSERT_TERMINOLOGY_DEPENDENCY
                    && batch.parameter_sets().len() == 2
                    && batch.parameter_sets().iter().all(|parameters| {
                        parameters[2] == text("宝剑") && parameters[3] == text("Sword")
                    })
        )));
    }

    #[tokio::test]
    async fn preparation_rejects_invalidation_and_reuse_target_overlap_before_sqlite() {
        let harness = Harness::new(None);
        let target = identity_at(11, "name", "宝剑");
        let preparation = TranslationPlanPreparation::new(
            vec![TranslationInvalidation::new(
                target.clone(),
                "Old sword",
                Vec::new(),
            )],
            vec![TranslationReuse::new(
                TranslationReuseSeed::new(identity_at(10, "name", "宝剑"), "Sword", Vec::new()),
                vec![TranslationReuseTarget::new(
                    target,
                    Some("Old sword".to_owned()),
                    Vec::new(),
                )],
            )],
        );

        let error = harness
            .service(2, 1)
            .apply_preparation(&project(), preparation)
            .await
            .expect_err("同一叶子不得同时失效和复用");

        assert!(matches!(
            error,
            MzStandardTranslationResultStorageError::InvalidPlan(
                ResultStoragePlanError::DuplicateLeaf
            )
        ));
        assert!(harness.calls.lock().expect("事务调用锁不应中毒").is_empty());
    }

    #[tokio::test]
    async fn commit_checks_untranslated_leaf_then_writes_translation_and_dependencies_atomically() {
        let harness = Harness::new(None);
        let service = harness.service(1, 10);
        let result = ValidatedTranslationTaskResult::new(
            StandardTranslationTaskIndex::new(0),
            vec![TranslationPatch::new(
                identity("name", "宝剑"),
                Vec::new(),
                "Sword",
                vec![TerminologyDependency::new("宝剑", "Sword")],
            )],
        );

        service
            .commit(&project(), result)
            .await
            .expect("任务提交应该成功");

        let calls = harness.calls.lock().expect("事务调用锁不应中毒");
        let steps = calls[0].1.steps();
        assert!(matches!(
            &steps[0],
            SqliteTransactionStep::RequireNoRows { query, .. }
                if query.statement().contains("translation IS NULL")
                    && query.statement().contains(TERMINOLOGY_DEPENDENCY_TABLE)
        ));
        assert!(steps.iter().any(|step| matches!(
            step,
            SqliteTransactionStep::Execute(command)
                if command.statement().contains("SET translation = ?")
                    && command.parameters()[0] == text("Sword")
        )));
        assert!(steps.iter().any(|step| matches!(
            step,
            SqliteTransactionStep::ExecuteMany(batch)
                if batch.statement() == INSERT_TERMINOLOGY_DEPENDENCY
                    && batch.parameter_sets()[0][2] == text("宝剑")
        )));
    }

    #[tokio::test]
    async fn commit_expands_one_validated_translation_to_all_propagation_targets_atomically() {
        let harness = Harness::new(None);
        let result = ValidatedTranslationTaskResult::new(
            StandardTranslationTaskIndex::new(0),
            vec![TranslationPatch::new(
                identity_at(10, "name", "宝剑"),
                vec![
                    identity_at(11, "name", "宝剑"),
                    identity_at(12, "name", "宝剑"),
                ],
                "Sword",
                vec![TerminologyDependency::new("宝剑", "Sword")],
            )],
        );

        harness
            .service(2, 1)
            .commit(&project(), result)
            .await
            .expect("代表译文应该原子传播");

        assert_eq!(
            harness.cpu_calls.load(Ordering::SeqCst),
            4,
            "一个代表和两个 alias 应按三个物理叶子分批，随后执行一次唯一性校验"
        );
        assert_eq!(harness.max_cpu_active.load(Ordering::SeqCst), 2);
        let calls = harness.calls.lock().expect("事务调用锁不应中毒");
        let steps = calls[0].1.steps();
        assert_eq!(
            steps
                .iter()
                .take_while(|step| matches!(step, SqliteTransactionStep::RequireNoRows { .. }))
                .count(),
            3
        );
        assert_eq!(
            steps
                .iter()
                .filter(|step| matches!(
                    step,
                    SqliteTransactionStep::Execute(command)
                        if command.statement().contains("SET translation = ?")
                            && command.parameters()[0] == text("Sword")
                ))
                .count(),
            3
        );
        assert!(steps.iter().any(|step| matches!(
            step,
            SqliteTransactionStep::ExecuteMany(batch)
                if batch.statement() == INSERT_TERMINOLOGY_DEPENDENCY
                    && batch.parameter_sets().len() == 3
        )));
    }

    #[tokio::test]
    async fn every_text_body_kind_checks_its_semantic_unit_type() {
        for (kind, unit_type) in [
            (TextGroupKind::EventDialogue, "dialogue"),
            (TextGroupKind::EventChoices, "choices"),
            (TextGroupKind::EventScrollingText, "scrolling_text"),
            (TextGroupKind::EventCommand, "event_command"),
        ] {
            let harness = Harness::new(None);
            let result = ValidatedTranslationTaskResult::new(
                StandardTranslationTaskIndex::new(0),
                vec![TranslationPatch::new(
                    text_body_identity(kind, "こんにちは"),
                    Vec::new(),
                    "你好",
                    Vec::new(),
                )],
            );

            harness
                .service(1, 1)
                .commit(&project(), result)
                .await
                .expect("text_body 应以具体单元语义提交");

            let calls = harness.calls.lock().expect("事务调用锁不应中毒");
            let SqliteTransactionStep::RequireNoRows { query, .. } = &calls[0].1.steps()[0] else {
                panic!("第一步必须检查计划是否过期");
            };
            assert!(query.statement().contains("unit_type = ?"));
            assert!(query.parameters().contains(&text(unit_type)));
        }
    }

    #[tokio::test]
    async fn configured_parallel_encoding_preserves_the_same_deterministic_plan() {
        let serial = Harness::new(None);
        let parallel = Harness::new(None);
        let preparation = || {
            TranslationPlanPreparation::new(
                vec![
                    TranslationInvalidation::new(identity("name", "宝剑"), "Sword", Vec::new()),
                    TranslationInvalidation::new(
                        identity("description", "锋利的宝剑"),
                        "A sharp sword",
                        Vec::new(),
                    ),
                ],
                Vec::new(),
            )
        };

        serial
            .service(1, 1)
            .apply_preparation(&project(), preparation())
            .await
            .expect("串行编码应该成功");
        parallel
            .service(2, 1)
            .apply_preparation(&project(), preparation())
            .await
            .expect("并行编码应该成功");

        assert_eq!(parallel.max_cpu_active.load(Ordering::SeqCst), 2);
        let serial_calls = serial.calls.lock().expect("串行事务调用锁不应中毒");
        let parallel_calls = parallel.calls.lock().expect("并行事务调用锁不应中毒");
        assert_eq!(serial_calls[0].1, parallel_calls[0].1);
    }

    #[tokio::test]
    async fn duplicate_leaf_is_rejected_before_sqlite_side_effects() {
        let harness = Harness::new(None);
        let duplicate_identity = identity("name", "宝剑");
        let result = ValidatedTranslationTaskResult::new(
            StandardTranslationTaskIndex::new(0),
            vec![
                TranslationPatch::new(duplicate_identity.clone(), Vec::new(), "Sword", Vec::new()),
                TranslationPatch::new(duplicate_identity, Vec::new(), "Blade", Vec::new()),
            ],
        );

        let error = harness
            .service(2, 1)
            .commit(&project(), result)
            .await
            .expect_err("同一任务不得重复修改同一叶子");

        assert!(matches!(
            error,
            MzStandardTranslationResultStorageError::InvalidPlan(
                ResultStoragePlanError::DuplicateLeaf
            )
        ));
        assert!(harness.calls.lock().expect("事务调用锁不应中毒").is_empty());
    }

    #[tokio::test]
    async fn propagation_target_cannot_repeat_or_belong_to_multiple_leaders() {
        let repeated_target = identity_at(12, "name", "宝剑");
        for result in [
            ValidatedTranslationTaskResult::new(
                StandardTranslationTaskIndex::new(0),
                vec![TranslationPatch::new(
                    identity_at(10, "name", "宝剑"),
                    vec![repeated_target.clone(), repeated_target.clone()],
                    "Sword",
                    Vec::new(),
                )],
            ),
            ValidatedTranslationTaskResult::new(
                StandardTranslationTaskIndex::new(0),
                vec![
                    TranslationPatch::new(
                        identity_at(10, "name", "宝剑"),
                        vec![repeated_target.clone()],
                        "Sword",
                        Vec::new(),
                    ),
                    TranslationPatch::new(
                        identity_at(11, "name", "宝剑"),
                        vec![repeated_target.clone()],
                        "Sword",
                        Vec::new(),
                    ),
                ],
            ),
        ] {
            let harness = Harness::new(None);
            let error = harness
                .service(2, 1)
                .commit(&project(), result)
                .await
                .expect_err("传播目标必须全局唯一");
            assert!(matches!(
                error,
                MzStandardTranslationResultStorageError::InvalidPlan(
                    ResultStoragePlanError::DuplicateLeaf
                )
            ));
            assert!(harness.calls.lock().expect("事务调用锁不应中毒").is_empty());
        }
    }

    #[tokio::test]
    async fn empty_reuse_is_rejected_before_sqlite_side_effects() {
        let harness = Harness::new(None);
        let preparation = TranslationPlanPreparation::new(
            Vec::new(),
            vec![TranslationReuse::new(
                TranslationReuseSeed::new(identity_at(10, "name", "宝剑"), "Sword", Vec::new()),
                Vec::new(),
            )],
        );

        let error = harness
            .service(1, 1)
            .apply_preparation(&project(), preparation)
            .await
            .expect_err("没有目标的复用计划应该失败");

        assert!(matches!(
            error,
            MzStandardTranslationResultStorageError::InvalidPlan(
                ResultStoragePlanError::EmptyReuseTargets
            )
        ));
        assert!(harness.calls.lock().expect("事务调用锁不应中毒").is_empty());
    }

    #[tokio::test]
    async fn stale_not_committed_and_unknown_outcomes_remain_distinct() {
        let cases = [
            (
                Err(ExecuteTransactionError::RequirementFailed {
                    check_id: SqliteCheckId::new("stale"),
                }),
                "stale",
            ),
            (
                Err(ExecuteTransactionError::NotCommitted(FakeError("write"))),
                "not_committed",
            ),
            (
                Err(ExecuteTransactionError::OutcomeUnknown(FakeError("commit"))),
                "unknown",
            ),
        ];
        for (response, expected) in cases {
            let harness = Harness::new(Some(response));
            let error = harness
                .service(1, 1)
                .apply_preparation(
                    &project(),
                    TranslationPlanPreparation::new(
                        vec![TranslationInvalidation::new(
                            identity("name", "宝剑"),
                            "Sword",
                            Vec::new(),
                        )],
                        Vec::new(),
                    ),
                )
                .await
                .expect_err("事务终态应该传播");
            match (expected, error) {
                ("stale", MzStandardTranslationResultStorageError::StalePlan { .. })
                | ("not_committed", MzStandardTranslationResultStorageError::NotCommitted { .. })
                | ("unknown", MzStandardTranslationResultStorageError::OutcomeUnknown { .. }) => {}
                (expected, actual) => panic!("期望 {expected}，实际为 {actual}"),
            }
        }
    }

    struct Harness {
        calls: Arc<Mutex<Vec<(PathBuf, SqliteTransactionPlan)>>>,
        response: SharedTransactionResponse,
        cpu_calls: Arc<AtomicUsize>,
        max_cpu_active: Arc<AtomicUsize>,
    }

    impl Harness {
        fn new(response: Option<TransactionResponse>) -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                response: Arc::new(Mutex::new(response)),
                cpu_calls: Arc::new(AtomicUsize::new(0)),
                max_cpu_active: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn service(
            &self,
            encode_concurrency: usize,
            leaves_per_job: usize,
        ) -> MzStandardTranslationResultStorageService<RecordingSqlite, RecordingCpu> {
            MzStandardTranslationResultStorageService::new(
                RecordingSqlite {
                    calls: Arc::clone(&self.calls),
                    response: Arc::clone(&self.response),
                },
                RecordingCpu {
                    calls: Arc::clone(&self.cpu_calls),
                    active: Arc::new(AtomicUsize::new(0)),
                    max_active: Arc::clone(&self.max_cpu_active),
                },
                MzStandardTranslationResultStorageConfig::new(
                    non_zero(encode_concurrency),
                    non_zero(leaves_per_job),
                ),
            )
        }
    }

    fn identity(field: &str, original: &str) -> TranslationLeafIdentity {
        identity_at(10, field, original)
    }

    fn identity_at(item_index: usize, field: &str, original: &str) -> TranslationLeafIdentity {
        let source = MzSource::data(StandardDataFile::Items);
        let group_location =
            MzLocation::value(source.clone(), vec![MzLocationStep::index(item_index)]);
        let exact_location = MzLocation::value(
            source,
            vec![
                MzLocationStep::index(item_index),
                MzLocationStep::key(field),
            ],
        );
        TranslationLeafIdentity::new(
            TextGroupKind::DatabaseEntry,
            group_location,
            exact_location,
            original,
        )
    }

    fn text_body_identity(kind: TextGroupKind, original: &str) -> TranslationLeafIdentity {
        let source = MzSource::map(1);
        let group_steps = vec![
            MzLocationStep::key("events"),
            MzLocationStep::index(2),
            MzLocationStep::key("pages"),
            MzLocationStep::index(0),
            MzLocationStep::key("list"),
            MzLocationStep::index(5),
        ];
        let mut exact_steps = group_steps.clone();
        exact_steps.extend([MzLocationStep::key("parameters"), MzLocationStep::index(0)]);
        TranslationLeafIdentity::new(
            kind,
            MzLocation::value(source.clone(), group_steps),
            MzLocation::value(source, exact_steps),
            original,
        )
    }

    fn project() -> StoredProjectRecord {
        StoredProjectRecord::new(
            "demo".parse::<ProjectName>().expect("项目名称应该有效"),
            PathBuf::from("C:/games/demo"),
            PathBuf::from("C:/projects/demo.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
        )
    }

    fn non_zero(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).expect("测试配置必须非零")
    }
}
