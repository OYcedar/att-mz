//! 标准译文状态的原子对账与提交。

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::path::PathBuf;

use futures_util::stream::{self, StreamExt, TryStreamExt};

use crate::att_mz::location_codec::{MzLocationCodec, MzLocationCodecError};
use crate::att_mz::project::OpenedProject;
use crate::att_mz::standard_asset::{MzStandardAssetStorageKind, MzStandardAssetTable};
use crate::fingerprint::Sha256Fingerprint;
use crate::project_database::{PLACEHOLDER_RULES_RESOURCE_KIND, TERMINOLOGY_RESOURCE_KIND};
use crate::storage::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::storage::sqlite::{
    ExecuteTransactionError, SqliteCommand, SqliteQuery, SqliteTransactionExecutor,
    SqliteTransactionPlan, SqliteTransactionStep, SqliteValue,
};

use super::standard::{
    StandardTranslationResultStore, TranslationLeafIdentity, TranslationPlanPreparation,
    TranslationSnapshotBaseline, ValidatedTranslationTaskResult,
};

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
}

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
        project: &OpenedProject,
        preparation: TranslationPlanPreparation,
    ) -> Result<(), Self::Error> {
        if !preparation.requires_storage_changes() {
            return Ok(());
        }
        let plan = self.encode_preparation_plan(preparation).await?;
        self.execute(project.database_path().to_path_buf(), plan)
            .await
    }

    async fn commit(
        &self,
        project: &OpenedProject,
        result: ValidatedTranslationTaskResult,
    ) -> Result<(), Self::Error> {
        let plan = self.encode_commit_plan(result).await?;
        self.execute(project.database_path().to_path_buf(), plan)
            .await
    }
}

impl<S, C> MzStandardTranslationResultStorageService<S, C>
where
    S: SqliteTransactionExecutor,
    C: CpuTaskExecutor,
{
    async fn encode_preparation_plan(
        &self,
        preparation: TranslationPlanPreparation,
    ) -> Result<SqliteTransactionPlan, MzStandardTranslationResultStorageError<S::Error, C::Error>>
    {
        let (work, terminology_json, placeholder_rules_json, snapshot_baseline) =
            preparation_work(preparation)
                .map_err(MzStandardTranslationResultStorageError::InvalidPlan)?;
        let jobs = split_jobs(work, self.config.leaves_per_encode_job.get());
        let batches = stream::iter(jobs.into_iter().map(|job| {
            let cpu = &self.cpu;
            async move {
                cpu.execute(move || encode_preparation_job(job))
                    .await
                    .map_err(MzStandardTranslationResultStorageError::ScheduleEncoding)?
                    .map_err(MzStandardTranslationResultStorageError::InvalidPlan)
            }
        }))
        .buffered(self.config.encode_concurrency.get())
        .try_collect::<Vec<_>>()
        .await?;

        finish_preparation_plan(
            batches,
            terminology_json,
            placeholder_rules_json,
            snapshot_baseline,
        )
        .map_err(MzStandardTranslationResultStorageError::InvalidPlan)
    }

    async fn encode_commit_plan(
        &self,
        result: ValidatedTranslationTaskResult,
    ) -> Result<SqliteTransactionPlan, MzStandardTranslationResultStorageError<S::Error, C::Error>>
    {
        let work =
            commit_work(result).map_err(MzStandardTranslationResultStorageError::InvalidPlan)?;
        let jobs = split_jobs(work, self.config.leaves_per_encode_job.get());
        let batches = stream::iter(jobs.into_iter().map(|job| {
            let cpu = &self.cpu;
            async move {
                cpu.execute(move || encode_commit_job(job))
                    .await
                    .map_err(MzStandardTranslationResultStorageError::ScheduleEncoding)?
                    .map_err(MzStandardTranslationResultStorageError::InvalidPlan)
            }
        }))
        .buffered(self.config.encode_concurrency.get())
        .try_collect::<Vec<_>>()
        .await?;

        finish_commit_plan(batches).map_err(MzStandardTranslationResultStorageError::InvalidPlan)
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

#[derive(Debug)]
pub(crate) enum MzStandardTranslationResultStorageError<S, C> {
    ScheduleEncoding(CpuTaskExecutionError<C>),
    InvalidPlan(ResultStoragePlanError),
    DatabaseNotFound { database_path: PathBuf },
    StalePlan { database_path: PathBuf },
    NotCommitted { database_path: PathBuf, source: S },
    OutcomeUnknown { database_path: PathBuf, source: S },
}

impl<S: fmt::Display, C: fmt::Display> fmt::Display
    for MzStandardTranslationResultStorageError<S, C>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ScheduleEncoding(source) => write!(formatter, "译文写入编码任务失败：{source}"),
            Self::InvalidPlan(source) => write!(formatter, "译文写入计划无效：{source}"),
            Self::DatabaseNotFound { database_path } => {
                write!(formatter, "项目数据库不存在：{}", database_path.display())
            }
            Self::StalePlan { database_path } => write!(
                formatter,
                "翻译计划建立后资产已发生变化（{}）",
                database_path.display()
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

impl<S: Error + 'static, C: Error + 'static> Error
    for MzStandardTranslationResultStorageError<S, C>
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

#[derive(Debug)]
pub(crate) enum ResultStoragePlanError {
    Location(MzLocationCodecError),
    EmptyTaskResult,
    EmptyReuseTargets,
    BlankTranslation,
    InconsistentTranslationState,
    MismatchedReuseOriginal,
    MismatchedPropagationOriginal,
    DuplicateLeaf,
}

impl fmt::Display for ResultStoragePlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Location(source) => source.fmt(formatter),
            Self::EmptyTaskResult => formatter.write_str("任务结果不包含任何译文"),
            Self::EmptyReuseTargets => formatter.write_str("译文复用计划不包含任何目标"),
            Self::BlankTranslation => formatter.write_str("任务结果包含空白译文"),
            Self::InconsistentTranslationState => {
                formatter.write_str("读取时的译文与译文状态没有同时存在或同时缺失")
            }
            Self::MismatchedReuseOriginal => formatter.write_str("译文复用种子与目标的原文不一致"),
            Self::MismatchedPropagationOriginal => {
                formatter.write_str("译文代表与传播目标的原文不一致")
            }
            Self::DuplicateLeaf => formatter.write_str("同一事务重复修改同一文本叶子"),
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

#[derive(Clone)]
struct EncodedIdentity {
    owner: &'static str,
    table: MzStandardAssetTable,
    unit_type: Option<&'static str>,
    exact_location: String,
    group_location: String,
    field_name: String,
    original_text: String,
}

enum PreparationLeafWork {
    Invalidation {
        identity: TranslationLeafIdentity,
        expected_translation: String,
        expected_translation_state: Sha256Fingerprint,
    },
    ReuseSeed {
        identity: TranslationLeafIdentity,
        expected_translation: String,
        expected_translation_state: Sha256Fingerprint,
    },
    ReuseTarget {
        seed_original: String,
        translation: String,
        identity: TranslationLeafIdentity,
        expected_translation: Option<String>,
        expected_translation_state: Option<Sha256Fingerprint>,
        replacement_translation_state: Sha256Fingerprint,
    },
}

enum EncodedPreparationLeaf {
    Invalidation {
        identity: EncodedIdentity,
        expected_translation: String,
        expected_translation_state: Sha256Fingerprint,
    },
    ReuseSeed {
        identity: EncodedIdentity,
        expected_translation: String,
        expected_translation_state: Sha256Fingerprint,
    },
    ReuseTarget {
        translation: String,
        identity: EncodedIdentity,
        expected_translation: Option<String>,
        expected_translation_state: Option<Sha256Fingerprint>,
        replacement_translation_state: Sha256Fingerprint,
    },
}

struct CommitLeafWork {
    identity: TranslationLeafIdentity,
    required_original: Option<String>,
    translation: String,
    translation_state: Sha256Fingerprint,
}

struct EncodedCommitLeaf {
    identity: EncodedIdentity,
    translation: String,
    translation_state: Sha256Fingerprint,
}

fn preparation_work(
    preparation: TranslationPlanPreparation,
) -> Result<
    (
        Vec<PreparationLeafWork>,
        String,
        String,
        TranslationSnapshotBaseline,
    ),
    ResultStoragePlanError,
> {
    let (invalidations, reuses, terminology_json, placeholder_rules_json, _, _, snapshot_baseline) =
        preparation.into_parts();
    let mut work = Vec::new();

    for invalidation in invalidations {
        work.push(PreparationLeafWork::Invalidation {
            identity: invalidation.identity().clone(),
            expected_translation: invalidation.expected_translation().to_owned(),
            expected_translation_state: invalidation.expected_translation_state(),
        });
    }

    for reuse in reuses {
        if reuse.targets().is_empty() {
            return Err(ResultStoragePlanError::EmptyReuseTargets);
        }
        let seed_original = reuse.seed().identity().original_text().to_owned();
        let translation = reuse.seed().expected_translation().to_owned();
        work.push(PreparationLeafWork::ReuseSeed {
            identity: reuse.seed().identity().clone(),
            expected_translation: translation.clone(),
            expected_translation_state: reuse.seed().expected_translation_state(),
        });

        for target in reuse.targets() {
            work.push(PreparationLeafWork::ReuseTarget {
                seed_original: seed_original.clone(),
                translation: translation.clone(),
                identity: target.identity().clone(),
                expected_translation: target.expected_translation().map(str::to_owned),
                expected_translation_state: target.expected_translation_state(),
                replacement_translation_state: target.replacement_translation_state(),
            });
        }
    }

    Ok((
        work,
        terminology_json,
        placeholder_rules_json,
        snapshot_baseline,
    ))
}

fn encode_preparation_job(
    job: Vec<PreparationLeafWork>,
) -> Result<Vec<EncodedPreparationLeaf>, ResultStoragePlanError> {
    job.into_iter()
        .map(|work| match work {
            PreparationLeafWork::Invalidation {
                identity,
                expected_translation,
                expected_translation_state,
            } => {
                ensure_nonblank(&expected_translation)?;
                Ok(EncodedPreparationLeaf::Invalidation {
                    identity: encode_identity(&identity)?,
                    expected_translation,
                    expected_translation_state,
                })
            }
            PreparationLeafWork::ReuseSeed {
                identity,
                expected_translation,
                expected_translation_state,
            } => {
                ensure_nonblank(&expected_translation)?;
                Ok(EncodedPreparationLeaf::ReuseSeed {
                    identity: encode_identity(&identity)?,
                    expected_translation,
                    expected_translation_state,
                })
            }
            PreparationLeafWork::ReuseTarget {
                seed_original,
                translation,
                identity,
                expected_translation,
                expected_translation_state,
                replacement_translation_state,
            } => {
                ensure_nonblank(&translation)?;
                if identity.original_text() != seed_original {
                    return Err(ResultStoragePlanError::MismatchedReuseOriginal);
                }
                if expected_translation.is_some() != expected_translation_state.is_some() {
                    return Err(ResultStoragePlanError::InconsistentTranslationState);
                }
                if expected_translation
                    .as_deref()
                    .is_some_and(|value| value.trim().is_empty())
                {
                    return Err(ResultStoragePlanError::BlankTranslation);
                }
                Ok(EncodedPreparationLeaf::ReuseTarget {
                    translation,
                    identity: encode_identity(&identity)?,
                    expected_translation,
                    expected_translation_state,
                    replacement_translation_state,
                })
            }
        })
        .collect()
}

fn finish_preparation_plan(
    batches: Vec<Vec<EncodedPreparationLeaf>>,
    terminology_json: String,
    placeholder_rules_json: String,
    snapshot_baseline: TranslationSnapshotBaseline,
) -> Result<SqliteTransactionPlan, ResultStoragePlanError> {
    let mut steps = vec![require_snapshot_baseline(&snapshot_baseline)];
    let mut seen = BTreeSet::new();
    for leaf in batches.into_iter().flatten() {
        match leaf {
            EncodedPreparationLeaf::Invalidation {
                identity,
                expected_translation,
                expected_translation_state,
            } => {
                ensure_unique(&mut seen, &identity)?;
                steps.push(require_snapshot(
                    &identity,
                    Some((&expected_translation, expected_translation_state)),
                ));
                steps.push(clear_translation(&identity));
            }
            EncodedPreparationLeaf::ReuseSeed {
                identity,
                expected_translation,
                expected_translation_state,
            } => {
                ensure_unique(&mut seen, &identity)?;
                steps.push(require_snapshot(
                    &identity,
                    Some((&expected_translation, expected_translation_state)),
                ));
            }
            EncodedPreparationLeaf::ReuseTarget {
                translation,
                identity,
                expected_translation,
                expected_translation_state,
                replacement_translation_state,
            } => {
                ensure_unique(&mut seen, &identity)?;
                let expected = expected_translation
                    .as_deref()
                    .zip(expected_translation_state);
                steps.push(require_snapshot(&identity, expected));
                steps.push(write_translation(
                    &identity,
                    &translation,
                    replacement_translation_state,
                ));
            }
        }
    }
    steps.extend(resource_updates(terminology_json, placeholder_rules_json));
    Ok(SqliteTransactionPlan::new(steps))
}

fn commit_work(
    result: ValidatedTranslationTaskResult,
) -> Result<Vec<CommitLeafWork>, ResultStoragePlanError> {
    let patches = result.into_updates();
    if patches.is_empty() {
        return Err(ResultStoragePlanError::EmptyTaskResult);
    }
    let mut work = Vec::new();
    for patch in patches {
        let translation = patch.translation().to_owned();
        work.push(CommitLeafWork {
            identity: patch.identity().clone(),
            required_original: None,
            translation: translation.clone(),
            translation_state: patch.translation_state(),
        });
        for target in patch.propagation_targets() {
            work.push(CommitLeafWork {
                identity: target.identity().clone(),
                required_original: Some(patch.identity().original_text().to_owned()),
                translation: translation.clone(),
                translation_state: target.state_context().finish(&translation),
            });
        }
    }
    Ok(work)
}

fn encode_commit_job(
    job: Vec<CommitLeafWork>,
) -> Result<Vec<EncodedCommitLeaf>, ResultStoragePlanError> {
    job.into_iter()
        .map(|work| {
            ensure_nonblank(&work.translation)?;
            if work
                .required_original
                .as_deref()
                .is_some_and(|original| original != work.identity.original_text())
            {
                return Err(ResultStoragePlanError::MismatchedPropagationOriginal);
            }
            Ok(EncodedCommitLeaf {
                identity: encode_identity(&work.identity)?,
                translation: work.translation,
                translation_state: work.translation_state,
            })
        })
        .collect()
}

fn finish_commit_plan(
    batches: Vec<Vec<EncodedCommitLeaf>>,
) -> Result<SqliteTransactionPlan, ResultStoragePlanError> {
    let mut steps = Vec::new();
    let mut seen = BTreeSet::new();
    for leaf in batches.into_iter().flatten() {
        ensure_unique(&mut seen, &leaf.identity)?;
        steps.push(require_snapshot(&leaf.identity, None));
        steps.push(write_translation(
            &leaf.identity,
            &leaf.translation,
            leaf.translation_state,
        ));
    }
    Ok(SqliteTransactionPlan::new(steps))
}

fn split_jobs<T>(items: Vec<T>, leaves_per_job: usize) -> Vec<Vec<T>> {
    debug_assert!(leaves_per_job > 0);
    let mut items = items.into_iter();
    std::iter::from_fn(|| {
        let job = items.by_ref().take(leaves_per_job).collect::<Vec<_>>();
        (!job.is_empty()).then_some(job)
    })
    .collect()
}

fn encode_identity(
    identity: &TranslationLeafIdentity,
) -> Result<EncodedIdentity, ResultStoragePlanError> {
    let storage = MzStandardAssetStorageKind::for_group_kind(identity.kind());
    Ok(EncodedIdentity {
        owner: identity.owner().storage_name(),
        table: storage.table(),
        unit_type: storage.unit_type().map(|unit| unit.storage_name()),
        exact_location: MzLocationCodec::encode(identity.exact_location())
            .map_err(ResultStoragePlanError::Location)?,
        group_location: MzLocationCodec::encode(identity.group_location())
            .map_err(ResultStoragePlanError::Location)?,
        field_name: identity.field_name().to_owned(),
        original_text: identity.original_text().to_owned(),
    })
}

fn ensure_unique(
    seen: &mut BTreeSet<(&'static str, &'static str, String)>,
    identity: &EncodedIdentity,
) -> Result<(), ResultStoragePlanError> {
    if seen.insert((
        identity.owner,
        identity.table.storage_name(),
        identity.exact_location.clone(),
    )) {
        Ok(())
    } else {
        Err(ResultStoragePlanError::DuplicateLeaf)
    }
}

fn ensure_nonblank(translation: &str) -> Result<(), ResultStoragePlanError> {
    if translation.trim().is_empty() {
        Err(ResultStoragePlanError::BlankTranslation)
    } else {
        Ok(())
    }
}

fn require_snapshot_baseline(baseline: &TranslationSnapshotBaseline) -> SqliteTransactionStep {
    let mut parameters = vec![SqliteValue::Blob(
        baseline.source_snapshot_fingerprint().as_bytes().to_vec(),
    )];
    let owner_condition = if baseline.owner_source_fingerprints().is_empty() {
        "(SELECT COUNT(*) FROM standard_asset_owner_state) <> 0".to_owned()
    } else {
        let clauses = baseline
            .owner_source_fingerprints()
            .iter()
            .map(|(owner, fingerprint)| {
                parameters.push(text(owner.storage_name()));
                parameters.push(SqliteValue::Blob(fingerprint.as_bytes().to_vec()));
                "(owner = ? AND source_snapshot_fingerprint = ?)"
            })
            .collect::<Vec<_>>()
            .join(" OR ");
        format!(
            "(SELECT COUNT(*) FROM standard_asset_owner_state) <> {} OR EXISTS (SELECT 1 FROM standard_asset_owner_state WHERE NOT ({clauses}))",
            baseline.owner_source_fingerprints().len()
        )
    };
    parameters.extend([
        text(TERMINOLOGY_RESOURCE_KIND),
        text(baseline.terminology_json()),
        text(PLACEHOLDER_RULES_RESOURCE_KIND),
        text(baseline.placeholder_rules_json()),
    ]);

    SqliteTransactionStep::RequireNoRows(SqliteQuery::new(
        format!(
            "SELECT 1 WHERE (SELECT COUNT(*) FROM metadata) <> 1 OR NOT EXISTS (SELECT 1 FROM metadata WHERE source_snapshot_fingerprint = ?) OR {owner_condition} OR (SELECT COUNT(*) FROM standard_translation_resource) <> 2 OR NOT EXISTS (SELECT 1 FROM standard_translation_resource WHERE resource_kind = ? AND canonical_json = ?) OR NOT EXISTS (SELECT 1 FROM standard_translation_resource WHERE resource_kind = ? AND canonical_json = ?)"
        ),
        parameters,
    ))
}

fn require_snapshot(
    identity: &EncodedIdentity,
    expected_translation: Option<(&str, Sha256Fingerprint)>,
) -> SqliteTransactionStep {
    let mut parameters = vec![
        text(identity.owner),
        text(identity.exact_location.clone()),
        text(identity.group_location.clone()),
        text(identity.field_name.clone()),
        text(identity.original_text.clone()),
    ];
    let unit_predicate = if let Some(unit_type) = identity.unit_type {
        parameters.push(text(unit_type));
        " AND unit_type = ?"
    } else {
        ""
    };
    let state_predicate = if let Some((translation, state)) = expected_translation {
        parameters.push(text(translation));
        parameters.push(blob(state));
        " AND translation = ? AND translation_state = ?"
    } else {
        " AND translation IS NULL AND translation_state IS NULL"
    };
    SqliteTransactionStep::RequireNoRows(SqliteQuery::new(
        format!(
            "SELECT 1 WHERE NOT EXISTS (SELECT 1 FROM {} WHERE owner = ? AND exact_location = ? AND group_location = ? AND field_name = ? AND original_text = ?{unit_predicate}{state_predicate})",
            identity.table.storage_name()
        ),
        parameters,
    ))
}

fn clear_translation(identity: &EncodedIdentity) -> SqliteTransactionStep {
    execute(
        format!(
            "UPDATE {} SET translation = NULL, translation_state = NULL WHERE owner = ? AND exact_location = ?",
            identity.table.storage_name()
        ),
        vec![text(identity.owner), text(identity.exact_location.clone())],
    )
}

fn write_translation(
    identity: &EncodedIdentity,
    translation: &str,
    state: Sha256Fingerprint,
) -> SqliteTransactionStep {
    execute(
        format!(
            "UPDATE {} SET translation = ?, translation_state = ? WHERE owner = ? AND exact_location = ?",
            identity.table.storage_name()
        ),
        vec![
            text(translation),
            blob(state),
            text(identity.owner),
            text(identity.exact_location.clone()),
        ],
    )
}

fn resource_updates(
    terminology_json: String,
    placeholder_rules_json: String,
) -> [SqliteTransactionStep; 2] {
    [
        update_resource(TERMINOLOGY_RESOURCE_KIND, terminology_json),
        update_resource(PLACEHOLDER_RULES_RESOURCE_KIND, placeholder_rules_json),
    ]
}

fn update_resource(kind: &'static str, canonical_json: String) -> SqliteTransactionStep {
    execute(
        "UPDATE standard_translation_resource SET canonical_json = ? WHERE resource_kind = ? AND canonical_json <> ?",
        vec![
            text(canonical_json.clone()),
            text(kind),
            text(canonical_json),
        ],
    )
}

fn execute(statement: impl Into<String>, parameters: Vec<SqliteValue>) -> SqliteTransactionStep {
    SqliteTransactionStep::Execute(SqliteCommand::new(statement, parameters))
}

fn text(value: impl Into<String>) -> SqliteValue {
    SqliteValue::Text(value.into())
}

fn blob(value: Sha256Fingerprint) -> SqliteValue {
    SqliteValue::Blob(value.as_bytes().to_vec())
}

fn map_transaction_error<S, C>(
    database_path: PathBuf,
    error: ExecuteTransactionError<S>,
) -> MzStandardTranslationResultStorageError<S, C> {
    match error {
        ExecuteTransactionError::NotFound => {
            MzStandardTranslationResultStorageError::DatabaseNotFound { database_path }
        }
        ExecuteTransactionError::RequirementFailed => {
            MzStandardTranslationResultStorageError::StalePlan { database_path }
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
    use std::future::{Future, ready};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use rusqlite::types::Value as RusqliteValue;
    use rusqlite::{Connection, params_from_iter};

    use crate::att_mz::ProjectName;
    use crate::att_mz::standard_asset::MzStandardAssetOwner;
    use crate::att_mz::text::{
        MzLocation, MzLocationStep, MzSource, StandardDataFile, TextGroupKind,
    };
    use crate::project_database::SourceSnapshotFingerprint;
    use crate::storage::sqlite::SqliteTransactionStep;

    use super::*;
    use crate::att_mz::translate::standard::{
        StandardTranslationTaskIndex, TranslationInvalidation, TranslationPatch,
        TranslationPlanPreparationCounts, TranslationPropagationTarget,
        TranslationSnapshotBaseline, TranslationStateContext,
    };

    #[derive(Clone, Copy, Debug)]
    struct FakeError;

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fake failure")
        }
    }

    impl Error for FakeError {}

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
            let result = task();
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(result)
        }
    }

    #[derive(Clone, Default)]
    struct RecordingSqlite {
        plans: Arc<Mutex<Vec<SqliteTransactionPlan>>>,
    }

    impl SqliteTransactionExecutor for RecordingSqlite {
        type Error = FakeError;

        async fn execute_transaction(
            &self,
            _path: PathBuf,
            plan: SqliteTransactionPlan,
        ) -> Result<(), ExecuteTransactionError<Self::Error>> {
            self.plans.lock().expect("计划锁不应中毒").push(plan);
            Ok(())
        }
    }

    struct Harness {
        cpu_calls: Arc<AtomicUsize>,
        max_cpu_active: Arc<AtomicUsize>,
        sqlite: RecordingSqlite,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                cpu_calls: Arc::new(AtomicUsize::new(0)),
                max_cpu_active: Arc::new(AtomicUsize::new(0)),
                sqlite: RecordingSqlite::default(),
            }
        }

        fn service(
            &self,
            concurrency: usize,
            leaves_per_job: usize,
        ) -> MzStandardTranslationResultStorageService<RecordingSqlite, RecordingCpu> {
            MzStandardTranslationResultStorageService::new(
                self.sqlite.clone(),
                RecordingCpu {
                    calls: Arc::clone(&self.cpu_calls),
                    active: Arc::new(AtomicUsize::new(0)),
                    max_active: Arc::clone(&self.max_cpu_active),
                },
                MzStandardTranslationResultStorageConfig::new(
                    non_zero(concurrency),
                    non_zero(leaves_per_job),
                ),
            )
        }

        fn only_plan(&self) -> SqliteTransactionPlan {
            self.sqlite
                .plans
                .lock()
                .expect("计划锁不应中毒")
                .first()
                .expect("应提交一个事务计划")
                .clone()
        }
    }

    #[tokio::test]
    async fn preparation_encoding_obeys_job_size_and_bounded_ordered_concurrency() {
        let concurrent = Harness::new();
        concurrent
            .service(2, 2)
            .apply_preparation(&project(), invalidation_preparation(5))
            .await
            .expect("并发分片应成功");

        assert_eq!(concurrent.cpu_calls.load(Ordering::SeqCst), 3);
        assert_eq!(concurrent.max_cpu_active.load(Ordering::SeqCst), 2);
        let concurrent_plan = concurrent.only_plan();

        let serial = Harness::new();
        serial
            .service(1, 1)
            .apply_preparation(&project(), invalidation_preparation(5))
            .await
            .expect("串行分片应成功");

        assert_eq!(serial.cpu_calls.load(Ordering::SeqCst), 5);
        assert_eq!(serial.max_cpu_active.load(Ordering::SeqCst), 1);
        assert_eq!(concurrent_plan, serial.only_plan());
        assert!(matches!(
            concurrent_plan.steps().first(),
            Some(SqliteTransactionStep::RequireNoRows(_))
        ));
    }

    #[tokio::test]
    async fn commit_encoding_uses_the_same_bounded_ordered_leaf_partitioning() {
        let concurrent = Harness::new();
        concurrent
            .service(2, 2)
            .commit(&project(), five_leaf_result())
            .await
            .expect("并发提交编码应成功");
        assert_eq!(concurrent.cpu_calls.load(Ordering::SeqCst), 3);
        assert_eq!(concurrent.max_cpu_active.load(Ordering::SeqCst), 2);

        let serial = Harness::new();
        serial
            .service(1, 1)
            .commit(&project(), five_leaf_result())
            .await
            .expect("串行提交编码应成功");
        assert_eq!(serial.cpu_calls.load(Ordering::SeqCst), 5);
        assert_eq!(serial.max_cpu_active.load(Ordering::SeqCst), 1);
        assert_eq!(concurrent.only_plan(), serial.only_plan());
    }

    #[tokio::test]
    async fn unchanged_resources_and_current_leaves_skip_cpu_and_sqlite() {
        let harness = Harness::new();
        let preparation = TranslationPlanPreparation::with_baseline(
            Vec::new(),
            Vec::new(),
            "[]".to_owned(),
            "[]".to_owned(),
            TranslationPlanPreparationCounts::new(4, 0, 0),
            baseline(),
        );

        harness
            .service(3, 2)
            .apply_preparation(&project(), preparation)
            .await
            .expect("完全收敛状态应直接成功");

        assert_eq!(harness.cpu_calls.load(Ordering::SeqCst), 0);
        assert!(
            harness
                .sqlite
                .plans
                .lock()
                .expect("计划锁不应中毒")
                .is_empty()
        );
    }

    #[derive(Clone)]
    struct EvaluatingSqlite {
        connection: Arc<Mutex<Connection>>,
        attempted_writes: Arc<AtomicUsize>,
    }

    impl SqliteTransactionExecutor for EvaluatingSqlite {
        type Error = FakeError;

        fn execute_transaction(
            &self,
            _path: PathBuf,
            plan: SqliteTransactionPlan,
        ) -> impl Future<Output = Result<(), ExecuteTransactionError<Self::Error>>> + Send {
            let result = {
                let mut connection = self.connection.lock().expect("SQLite 锁不应中毒");
                evaluate_plan(&mut connection, &plan, &self.attempted_writes)
            };
            ready(result)
        }
    }

    #[tokio::test]
    async fn baseline_drift_is_stale_and_prevents_every_planned_write() {
        for mutation in [
            "metadata",
            "owner_fingerprint",
            "owner_set",
            "terminology",
            "placeholder_rules",
        ] {
            let sqlite = evaluating_sqlite();
            mutate_baseline(&sqlite.connection, mutation);
            let service = MzStandardTranslationResultStorageService::new(
                sqlite.clone(),
                RecordingCpu {
                    calls: Arc::new(AtomicUsize::new(0)),
                    active: Arc::new(AtomicUsize::new(0)),
                    max_active: Arc::new(AtomicUsize::new(0)),
                },
                MzStandardTranslationResultStorageConfig::new(non_zero(2), non_zero(2)),
            );

            let error = service
                .apply_preparation(&project(), changed_resource_preparation())
                .await
                .expect_err("基线漂移必须拒绝整个事务");

            assert!(
                matches!(
                    error,
                    MzStandardTranslationResultStorageError::StalePlan { .. }
                ),
                "漂移种类 {mutation}"
            );
            assert_eq!(sqlite.attempted_writes.load(Ordering::SeqCst), 0);
            let connection = sqlite.connection.lock().expect("SQLite 锁不应中毒");
            let translation: Option<String> = connection
                .query_row("SELECT translation FROM entry", [], |row| row.get(0))
                .expect("应可复核原译文");
            assert_eq!(
                translation.as_deref(),
                Some("旧译文"),
                "漂移种类 {mutation}"
            );
        }
    }

    #[tokio::test]
    async fn unchanged_baseline_applies_resource_and_leaf_changes_in_one_transaction() {
        let sqlite = evaluating_sqlite();
        let service = MzStandardTranslationResultStorageService::new(
            sqlite.clone(),
            RecordingCpu {
                calls: Arc::new(AtomicUsize::new(0)),
                active: Arc::new(AtomicUsize::new(0)),
                max_active: Arc::new(AtomicUsize::new(0)),
            },
            MzStandardTranslationResultStorageConfig::new(non_zero(2), non_zero(1)),
        );

        service
            .apply_preparation(&project(), changed_resource_preparation())
            .await
            .expect("未漂移基线应原子提交");

        let connection = sqlite.connection.lock().expect("SQLite 锁不应中毒");
        let (translation, state): (Option<String>, Option<Vec<u8>>) = connection
            .query_row(
                "SELECT translation, translation_state FROM entry",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("应可读取资产状态");
        assert_eq!((translation, state), (None, None));
        let terminology: String = connection
            .query_row(
                "SELECT canonical_json FROM standard_translation_resource WHERE resource_kind = 'terminology'",
                [],
                |row| row.get(0),
            )
            .expect("应可读取术语资源");
        assert_eq!(terminology, r#"[{"term":"新"}]"#);
    }

    fn evaluate_plan(
        connection: &mut Connection,
        plan: &SqliteTransactionPlan,
        attempted_writes: &AtomicUsize,
    ) -> Result<(), ExecuteTransactionError<FakeError>> {
        let transaction = connection.transaction().expect("应可开始测试事务");
        for step in plan.steps() {
            match step {
                SqliteTransactionStep::RequireNoRows(query) => {
                    let exists = {
                        let mut statement = transaction
                            .prepare(query.statement())
                            .expect("检查 SQL 应合法");
                        let parameters = query
                            .parameters()
                            .iter()
                            .map(to_rusqlite_value)
                            .collect::<Vec<_>>();
                        let mut rows = statement
                            .query(params_from_iter(parameters))
                            .expect("检查参数应可绑定");
                        rows.next().expect("检查查询应可执行").is_some()
                    };
                    if exists {
                        transaction.rollback().expect("失败事务应可回滚");
                        return Err(ExecuteTransactionError::RequirementFailed);
                    }
                }
                SqliteTransactionStep::Execute(command) => {
                    attempted_writes.fetch_add(1, Ordering::SeqCst);
                    let parameters = command
                        .parameters()
                        .iter()
                        .map(to_rusqlite_value)
                        .collect::<Vec<_>>();
                    transaction
                        .execute(command.statement(), params_from_iter(parameters))
                        .expect("写入 SQL 应可执行");
                }
                SqliteTransactionStep::ExecuteMany(batch) => {
                    let mut statement = transaction
                        .prepare(batch.statement())
                        .expect("批量 SQL 应合法");
                    for parameters in batch.parameter_sets() {
                        attempted_writes.fetch_add(1, Ordering::SeqCst);
                        statement
                            .execute(params_from_iter(parameters.iter().map(to_rusqlite_value)))
                            .expect("批量参数应可执行");
                    }
                }
            }
        }
        transaction.commit().expect("测试事务应可提交");
        Ok(())
    }

    fn evaluating_sqlite() -> EvaluatingSqlite {
        let connection = Connection::open_in_memory().expect("应可创建内存数据库");
        connection
            .execute_batch(
                "CREATE TABLE metadata (source_snapshot_fingerprint BLOB NOT NULL);
                 CREATE TABLE standard_asset_owner_state (owner TEXT PRIMARY KEY, source_snapshot_fingerprint BLOB NOT NULL);
                 CREATE TABLE standard_translation_resource (resource_kind TEXT PRIMARY KEY, canonical_json TEXT NOT NULL);
                 CREATE TABLE entry (owner TEXT NOT NULL, exact_location TEXT NOT NULL, group_location TEXT NOT NULL, field_name TEXT NOT NULL, original_text TEXT NOT NULL, translation TEXT, translation_state BLOB, PRIMARY KEY (owner, exact_location));",
            )
            .expect("测试 schema 应可建立");
        let identity = identity(0);
        connection
            .execute("INSERT INTO metadata VALUES (?1)", [vec![0xa5; 32]])
            .expect("应可写 metadata");
        connection
            .execute(
                "INSERT INTO standard_asset_owner_state VALUES ('builtin', ?1)",
                [vec![0xa5; 32]],
            )
            .expect("应可写 owner");
        connection
            .execute_batch(
                "INSERT INTO standard_translation_resource VALUES ('terminology', '[]');
                 INSERT INTO standard_translation_resource VALUES ('placeholder_rules', '[]');",
            )
            .expect("应可写资源");
        connection
            .execute(
                "INSERT INTO entry VALUES ('builtin', ?1, ?2, 'name', '原文-0', '旧译文', ?3)",
                rusqlite::params![
                    MzLocationCodec::encode(identity.exact_location()).expect("位置应可编码"),
                    MzLocationCodec::encode(identity.group_location()).expect("位置应可编码"),
                    vec![0x10_u8; 32],
                ],
            )
            .expect("应可写资产");
        EvaluatingSqlite {
            connection: Arc::new(Mutex::new(connection)),
            attempted_writes: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn mutate_baseline(connection: &Mutex<Connection>, mutation: &str) {
        let connection = connection.lock().expect("SQLite 锁不应中毒");
        match mutation {
            "metadata" => connection
                .execute("UPDATE metadata SET source_snapshot_fingerprint = ?1", [vec![0xb4; 32]])
                .expect("应可篡改 metadata"),
            "owner_fingerprint" => connection
                .execute("UPDATE standard_asset_owner_state SET source_snapshot_fingerprint = ?1", [vec![0xb4; 32]])
                .expect("应可篡改 owner 指纹"),
            "owner_set" => connection
                .execute("INSERT INTO standard_asset_owner_state VALUES ('rules', ?1)", [vec![0xa5; 32]])
                .expect("应可篡改 owner 集合"),
            "terminology" => connection
                .execute("UPDATE standard_translation_resource SET canonical_json = '[1]' WHERE resource_kind = 'terminology'", [])
                .expect("应可篡改术语"),
            "placeholder_rules" => connection
                .execute("UPDATE standard_translation_resource SET canonical_json = '[1]' WHERE resource_kind = 'placeholder_rules'", [])
                .expect("应可篡改占位符"),
            _ => panic!("未知篡改种类"),
        };
    }

    fn changed_resource_preparation() -> TranslationPlanPreparation {
        TranslationPlanPreparation::with_baseline(
            vec![TranslationInvalidation::new(
                identity(0),
                "旧译文",
                Sha256Fingerprint::from_bytes([0x10; 32]),
            )],
            Vec::new(),
            r#"[{"term":"新"}]"#.to_owned(),
            "[]".to_owned(),
            TranslationPlanPreparationCounts::new(0, 1, 0),
            baseline(),
        )
    }

    fn invalidation_preparation(count: usize) -> TranslationPlanPreparation {
        TranslationPlanPreparation::with_baseline(
            (0..count)
                .map(|index| {
                    TranslationInvalidation::new(
                        identity(index),
                        format!("旧译文-{index}"),
                        Sha256Fingerprint::from_bytes([index as u8; 32]),
                    )
                })
                .collect(),
            Vec::new(),
            "[]".to_owned(),
            "[]".to_owned(),
            TranslationPlanPreparationCounts::new(0, count, 0),
            baseline(),
        )
    }

    fn five_leaf_result() -> ValidatedTranslationTaskResult {
        let translation = "新译文";
        let state_context =
            |byte| TranslationStateContext::new(Sha256Fingerprint::from_bytes([byte; 32]));
        let propagation_targets = (1..5)
            .map(|index| {
                TranslationPropagationTarget::new(
                    identity_with_original(index, "原文-0"),
                    state_context(index as u8),
                )
            })
            .collect();
        ValidatedTranslationTaskResult::new(
            StandardTranslationTaskIndex::new(0),
            vec![TranslationPatch::new(
                identity(0),
                propagation_targets,
                translation,
                state_context(0).finish(translation),
            )],
        )
    }

    fn baseline() -> TranslationSnapshotBaseline {
        TranslationSnapshotBaseline::new(
            SourceSnapshotFingerprint::from_bytes([0xa5; 32]),
            vec![(
                MzStandardAssetOwner::Builtin,
                SourceSnapshotFingerprint::from_bytes([0xa5; 32]),
            )],
            "[]".to_owned(),
            "[]".to_owned(),
        )
    }

    fn identity(index: usize) -> TranslationLeafIdentity {
        identity_with_original(index, &format!("原文-{index}"))
    }

    fn identity_with_original(index: usize, original: &str) -> TranslationLeafIdentity {
        let source = MzSource::data(StandardDataFile::Items);
        let group = MzLocation::value(source.clone(), vec![MzLocationStep::index(index)]);
        let exact = MzLocation::value(
            source,
            vec![MzLocationStep::index(index), MzLocationStep::key("name")],
        );
        TranslationLeafIdentity::new(
            MzStandardAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            "name",
            group,
            exact,
            original,
        )
    }

    fn project() -> OpenedProject {
        OpenedProject::new(
            "demo".parse::<ProjectName>().expect("项目名应合法"),
            PathBuf::from("C:/projects/demo"),
            PathBuf::from("C:/projects/demo/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::att_mz::project::test_layout_profile(),
        )
    }

    fn to_rusqlite_value(value: &SqliteValue) -> RusqliteValue {
        match value {
            SqliteValue::Null => RusqliteValue::Null,
            SqliteValue::Integer(value) => RusqliteValue::Integer(*value),
            SqliteValue::Real(value) => RusqliteValue::Real(*value),
            SqliteValue::Text(value) => RusqliteValue::Text(value.clone()),
            SqliteValue::Blob(value) => RusqliteValue::Blob(value.clone()),
        }
    }

    fn non_zero(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).expect("测试配置必须非零")
    }
}
