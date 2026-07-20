//! 标准译文状态的原子对账与提交。

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::path::PathBuf;

use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::fingerprint::Sha256Fingerprint;
use crate::rpg_maker::location_codec::{
    RpgMakerLocationCodec, RpgMakerLocationCodecError, RpgMakerProjectionCodec,
    RpgMakerProjectionCodecError,
};
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::project_database::{
    PLACEHOLDER_RULES_RESOURCE_KIND, TERMINOLOGY_RESOURCE_KIND,
};
use crate::storage::sqlite::{
    ExecuteTransactionError, SqliteCommand, SqliteQuery, SqliteTransactionExecutor,
    SqliteTransactionPlan, SqliteTransactionStep, SqliteValue,
};

use super::standard::{
    StandardTranslationResultStore, TranslationLeafIdentity, TranslationPlanPreparation,
    TranslationSnapshotBaseline, ValidatedTranslationTaskResult,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerStandardTranslationResultStorageConfig {
    leaves_per_encode_job: NonZeroUsize,
}

impl RpgMakerStandardTranslationResultStorageConfig {
    pub(crate) const fn new(leaves_per_encode_job: NonZeroUsize) -> Self {
        Self {
            leaves_per_encode_job,
        }
    }
}

pub(crate) struct RpgMakerStandardTranslationResultStorageService<S, C> {
    sqlite: S,
    cpu: C,
    config: RpgMakerStandardTranslationResultStorageConfig,
}

impl<S, C> RpgMakerStandardTranslationResultStorageService<S, C> {
    pub(crate) fn new(
        sqlite: S,
        cpu: C,
        config: RpgMakerStandardTranslationResultStorageConfig,
    ) -> Self {
        Self {
            sqlite,
            cpu,
            config,
        }
    }
}

impl<S, C> StandardTranslationResultStore for RpgMakerStandardTranslationResultStorageService<S, C>
where
    S: SqliteTransactionExecutor,
    C: CpuTaskExecutor,
{
    type Error = RpgMakerStandardTranslationResultStorageError<S::Error, C::Error>;

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

impl<S, C> RpgMakerStandardTranslationResultStorageService<S, C>
where
    S: SqliteTransactionExecutor,
    C: CpuTaskExecutor,
{
    async fn encode_preparation_plan(
        &self,
        preparation: TranslationPlanPreparation,
    ) -> Result<
        SqliteTransactionPlan,
        RpgMakerStandardTranslationResultStorageError<S::Error, C::Error>,
    > {
        let leaves_per_job = self.config.leaves_per_encode_job.get();
        let (jobs, terminology_json, placeholder_rules_json, snapshot_baseline) = self
            .cpu
            .execute(move || {
                let (work, terminology_json, placeholder_rules_json, snapshot_baseline) =
                    preparation_work(preparation)?;
                Ok::<_, ResultStoragePlanError>((
                    split_jobs(work, leaves_per_job),
                    terminology_json,
                    placeholder_rules_json,
                    snapshot_baseline,
                ))
            })
            .await
            .map_err(RpgMakerStandardTranslationResultStorageError::ScheduleEncoding)?
            .map_err(RpgMakerStandardTranslationResultStorageError::InvalidPlan)?;
        let batches = self
            .cpu
            .execute_ordered_map(jobs, encode_preparation_job)
            .await
            .map_err(RpgMakerStandardTranslationResultStorageError::ScheduleEncoding)?;

        self.cpu
            .execute(move || {
                finish_preparation_plan(
                    batches.into_iter().collect::<Result<Vec<_>, _>>()?,
                    terminology_json,
                    placeholder_rules_json,
                    snapshot_baseline,
                )
            })
            .await
            .map_err(RpgMakerStandardTranslationResultStorageError::ScheduleEncoding)?
            .map_err(RpgMakerStandardTranslationResultStorageError::InvalidPlan)
    }

    async fn encode_commit_plan(
        &self,
        result: ValidatedTranslationTaskResult,
    ) -> Result<
        SqliteTransactionPlan,
        RpgMakerStandardTranslationResultStorageError<S::Error, C::Error>,
    > {
        let leaves_per_job = self.config.leaves_per_encode_job.get();
        let jobs = self
            .cpu
            .execute(move || commit_work(result).map(|work| split_jobs(work, leaves_per_job)))
            .await
            .map_err(RpgMakerStandardTranslationResultStorageError::ScheduleEncoding)?
            .map_err(RpgMakerStandardTranslationResultStorageError::InvalidPlan)?;
        let batches = self
            .cpu
            .execute_ordered_map(jobs, encode_commit_job)
            .await
            .map_err(RpgMakerStandardTranslationResultStorageError::ScheduleEncoding)?;

        self.cpu
            .execute(move || {
                finish_commit_plan(batches.into_iter().collect::<Result<Vec<_>, _>>()?)
            })
            .await
            .map_err(RpgMakerStandardTranslationResultStorageError::ScheduleEncoding)?
            .map_err(RpgMakerStandardTranslationResultStorageError::InvalidPlan)
    }

    async fn execute(
        &self,
        database_path: PathBuf,
        plan: SqliteTransactionPlan,
    ) -> Result<(), RpgMakerStandardTranslationResultStorageError<S::Error, C::Error>> {
        self.sqlite
            .execute_transaction(database_path.clone(), plan)
            .await
            .map_err(|error| map_transaction_error(database_path, error))
    }
}

#[derive(Debug)]
pub(crate) enum RpgMakerStandardTranslationResultStorageError<S, C> {
    ScheduleEncoding(CpuTaskExecutionError<C>),
    InvalidPlan(ResultStoragePlanError),
    DatabaseNotFound { database_path: PathBuf },
    StalePlan { database_path: PathBuf },
    NotCommitted { database_path: PathBuf, source: S },
    OutcomeUnknown { database_path: PathBuf, source: S },
}

impl<S: fmt::Display, C: fmt::Display> fmt::Display
    for RpgMakerStandardTranslationResultStorageError<S, C>
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
    for RpgMakerStandardTranslationResultStorageError<S, C>
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
    Location(RpgMakerLocationCodecError),
    Projection(RpgMakerProjectionCodecError),
    EmptyTaskResult,
    EmptyReuseTargets,
    BlankTranslation,
    InconsistentTranslationState,
    MismatchedReuseOriginal,
    MismatchedReuseContext,
    MismatchedPropagationOriginal,
    MismatchedPropagationContext,
    DuplicateLeaf,
}

impl fmt::Display for ResultStoragePlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Location(source) => source.fmt(formatter),
            Self::Projection(source) => source.fmt(formatter),
            Self::EmptyTaskResult => formatter.write_str("任务结果不包含任何译文"),
            Self::EmptyReuseTargets => formatter.write_str("译文复用计划不包含任何目标"),
            Self::BlankTranslation => formatter.write_str("任务结果包含空白译文"),
            Self::InconsistentTranslationState => {
                formatter.write_str("读取时的译文与译文状态没有同时存在或同时缺失")
            }
            Self::MismatchedReuseOriginal => formatter.write_str("译文复用种子与目标的原文不一致"),
            Self::MismatchedReuseContext => {
                formatter.write_str("译文复用种子与目标的翻译上下文不一致")
            }
            Self::MismatchedPropagationOriginal => {
                formatter.write_str("译文代表与传播目标的原文不一致")
            }
            Self::MismatchedPropagationContext => {
                formatter.write_str("译文代表与传播目标的翻译上下文不一致")
            }
            Self::DuplicateLeaf => formatter.write_str("同一事务重复修改同一文本叶子"),
        }
    }
}

impl Error for ResultStoragePlanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Location(source) => Some(source),
            Self::Projection(source) => Some(source),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct EncodedIdentity {
    owner: &'static str,
    group_location: String,
    field_role: String,
    original_text: String,
    translation_context_json: String,
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
        seed_translation_context_json: String,
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
    required_translation_context_json: Option<String>,
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
        let seed_translation_context_json = reuse
            .seed()
            .identity()
            .translation_context_json()
            .to_owned();
        let translation = reuse.seed().expected_translation().to_owned();
        work.push(PreparationLeafWork::ReuseSeed {
            identity: reuse.seed().identity().clone(),
            expected_translation: translation.clone(),
            expected_translation_state: reuse.seed().expected_translation_state(),
        });

        for target in reuse.targets() {
            work.push(PreparationLeafWork::ReuseTarget {
                seed_original: seed_original.clone(),
                seed_translation_context_json: seed_translation_context_json.clone(),
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
                seed_translation_context_json,
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
                if identity.translation_context_json() != seed_translation_context_json {
                    return Err(ResultStoragePlanError::MismatchedReuseContext);
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
            required_translation_context_json: None,
            translation: translation.clone(),
            translation_state: patch.translation_state(),
        });
        for target in patch.propagation_targets() {
            work.push(CommitLeafWork {
                identity: target.identity().clone(),
                required_original: Some(patch.identity().original_text().to_owned()),
                required_translation_context_json: Some(
                    patch.identity().translation_context_json().to_owned(),
                ),
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
            if work
                .required_translation_context_json
                .as_deref()
                .is_some_and(|context| context != work.identity.translation_context_json())
            {
                return Err(ResultStoragePlanError::MismatchedPropagationContext);
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
    Ok(EncodedIdentity {
        owner: identity.owner().storage_name(),
        group_location: RpgMakerLocationCodec::encode(identity.group_location())
            .map_err(ResultStoragePlanError::Location)?,
        field_role: RpgMakerProjectionCodec::encode_role(identity.role())
            .map_err(ResultStoragePlanError::Projection)?,
        original_text: identity.original_text().to_owned(),
        translation_context_json: identity.translation_context_json().to_owned(),
    })
}

fn ensure_unique(
    seen: &mut BTreeSet<(&'static str, String, String)>,
    identity: &EncodedIdentity,
) -> Result<(), ResultStoragePlanError> {
    if seen.insert((
        identity.owner,
        identity.group_location.clone(),
        identity.field_role.clone(),
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
    let owner_condition = if baseline.owner_snapshots().is_empty() {
        "(SELECT COUNT(*) FROM standard_asset_owner_state) <> 0".to_owned()
    } else {
        let clauses = baseline
            .owner_snapshots()
            .iter()
            .map(|snapshot| {
                parameters.push(text(snapshot.owner().storage_name()));
                parameters.push(SqliteValue::Blob(
                    snapshot.source_snapshot_fingerprint().as_bytes().to_vec(),
                ));
                parameters.push(SqliteValue::Blob(
                    snapshot.asset_snapshot_fingerprint().as_bytes().to_vec(),
                ));
                "(owner = ? AND source_snapshot_fingerprint = ? AND asset_snapshot_fingerprint = ?)"
            })
            .collect::<Vec<_>>()
            .join(" OR ");
        format!(
            "(SELECT COUNT(*) FROM standard_asset_owner_state) <> {} OR EXISTS (SELECT 1 FROM standard_asset_owner_state WHERE NOT ({clauses}))",
            baseline.owner_snapshots().len()
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
        text(identity.group_location.clone()),
        text(identity.field_role.clone()),
        text(identity.original_text.clone()),
        text(identity.translation_context_json.clone()),
    ];
    let state_predicate = if let Some((translation, state)) = expected_translation {
        parameters.push(text(translation));
        parameters.push(blob(state));
        " AND translation = ? AND translation_state = ?"
    } else {
        " AND translation IS NULL AND translation_state IS NULL"
    };
    SqliteTransactionStep::RequireNoRows(SqliteQuery::new(
        format!(
            "SELECT 1 WHERE NOT EXISTS (SELECT 1 FROM standard_text_leaf WHERE owner = ? AND group_location = ? AND field_role = ? AND original_text = ? AND translation_context_json = ?{state_predicate})"
        ),
        parameters,
    ))
}

fn clear_translation(identity: &EncodedIdentity) -> SqliteTransactionStep {
    execute(
        "UPDATE standard_text_leaf SET translation = NULL, translation_state = NULL WHERE owner = ? AND group_location = ? AND field_role = ?",
        vec![
            text(identity.owner),
            text(identity.group_location.clone()),
            text(identity.field_role.clone()),
        ],
    )
}

fn write_translation(
    identity: &EncodedIdentity,
    translation: &str,
    state: Sha256Fingerprint,
) -> SqliteTransactionStep {
    execute(
        "UPDATE standard_text_leaf SET translation = ?, translation_state = ? WHERE owner = ? AND group_location = ? AND field_role = ?",
        vec![
            text(translation),
            blob(state),
            text(identity.owner),
            text(identity.group_location.clone()),
            text(identity.field_role.clone()),
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
) -> RpgMakerStandardTranslationResultStorageError<S, C> {
    match error {
        ExecuteTransactionError::NotFound => {
            RpgMakerStandardTranslationResultStorageError::DatabaseNotFound { database_path }
        }
        ExecuteTransactionError::RequirementFailed => {
            RpgMakerStandardTranslationResultStorageError::StalePlan { database_path }
        }
        ExecuteTransactionError::NotCommitted(source) => {
            RpgMakerStandardTranslationResultStorageError::NotCommitted {
                database_path,
                source,
            }
        }
        ExecuteTransactionError::OutcomeUnknown(source) => {
            RpgMakerStandardTranslationResultStorageError::OutcomeUnknown {
                database_path,
                source,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::{Future, ready};
    use std::sync::{Arc, Mutex};

    use crate::rpg_maker::ProjectName;
    use crate::rpg_maker::model::{ScalarFieldKey, TextFieldRole};
    use crate::rpg_maker::project_database::{AssetSnapshotFingerprint, SourceSnapshotFingerprint};
    use crate::rpg_maker::standard_asset::RpgMakerStandardAssetOwner;
    use crate::rpg_maker::text::{
        RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, StandardDataFile, TextGroupKind,
    };
    use crate::storage::sqlite::SqliteTransactionStep;

    use super::*;
    use crate::rpg_maker::translate::standard::{
        StandardTranslationTaskIndex, TranslationOwnerSnapshot, TranslationPatch,
        TranslationPropagationTarget, TranslationSnapshotBaseline, TranslationStateContext,
    };

    #[derive(Clone, Copy, Debug)]
    struct FakeError;

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fake")
        }
    }

    impl Error for FakeError {}

    #[derive(Clone, Copy)]
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

    #[derive(Clone, Default)]
    struct RecordingSqlite {
        plans: Arc<Mutex<Vec<(PathBuf, SqliteTransactionPlan)>>>,
    }

    impl SqliteTransactionExecutor for RecordingSqlite {
        type Error = FakeError;

        fn execute_transaction(
            &self,
            path: PathBuf,
            plan: SqliteTransactionPlan,
        ) -> impl Future<Output = Result<(), ExecuteTransactionError<Self::Error>>> + Send {
            self.plans.lock().expect("事务锁").push((path, plan));
            ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn commit_targets_one_logical_leaf_and_checks_translation_context() {
        let sqlite = RecordingSqlite::default();
        let plans = Arc::clone(&sqlite.plans);
        let service = RpgMakerStandardTranslationResultStorageService::new(
            sqlite,
            InlineCpu,
            RpgMakerStandardTranslationResultStorageConfig::new(NonZeroUsize::MIN),
        );
        let identity = scalar_identity(1, "name", "原文", "{}");
        let context = state_context(0x11);
        let result = ValidatedTranslationTaskResult::new(
            StandardTranslationTaskIndex::new(0),
            vec![TranslationPatch::new(
                identity.clone(),
                Vec::new(),
                "译文",
                context.finish("译文"),
            )],
        );

        service
            .commit(&project(), result)
            .await
            .expect("提交应成功");

        let plans = plans.lock().expect("事务锁");
        let plan = &plans[0].1;
        assert_eq!(plan.steps().len(), 2);
        let SqliteTransactionStep::RequireNoRows(require) = &plan.steps()[0] else {
            panic!("第一步应核对逻辑叶");
        };
        assert!(require.statement().contains("standard_text_leaf"));
        assert!(require.statement().contains("translation_context_json"));
        assert!(!require.statement().contains("exact_location"));
        assert_eq!(
            require.parameters()[1],
            SqliteValue::Text(
                RpgMakerLocationCodec::encode(identity.group_location()).expect("组位置应可编码")
            )
        );
        assert_eq!(
            require.parameters()[2],
            SqliteValue::Text(
                RpgMakerProjectionCodec::encode_role(identity.role()).expect("角色应可编码")
            )
        );
        assert_eq!(require.parameters()[4], SqliteValue::Text("{}".to_owned()));

        let SqliteTransactionStep::Execute(update) = &plan.steps()[1] else {
            panic!("第二步应写入逻辑叶");
        };
        assert_eq!(
            update.statement(),
            "UPDATE standard_text_leaf SET translation = ?, translation_state = ? WHERE owner = ? AND group_location = ? AND field_role = ?"
        );
    }

    #[test]
    fn body_propagation_rejects_a_different_source_speaker_context() {
        let leader = dialogue_body_identity(1, "同一句", r#"{"source_speaker":"甲"}"#);
        let target = dialogue_body_identity(2, "同一句", r#"{"source_speaker":"乙"}"#);
        let context = state_context(0x22);
        let result = ValidatedTranslationTaskResult::new(
            StandardTranslationTaskIndex::new(0),
            vec![TranslationPatch::new(
                leader,
                vec![TranslationPropagationTarget::new(target, context)],
                "相同译文",
                context.finish("相同译文"),
            )],
        );

        let work = commit_work(result).expect("结果形状应合法");
        let Err(error) = encode_commit_job(work) else {
            panic!("不同 Speaker 上下文不得传播");
        };

        assert!(matches!(
            error,
            ResultStoragePlanError::MismatchedPropagationContext
        ));
    }

    #[test]
    fn baseline_guard_includes_both_owner_fingerprints() {
        let baseline = TranslationSnapshotBaseline::new(
            SourceSnapshotFingerprint::from_bytes([0xa5; 32]),
            vec![TranslationOwnerSnapshot::new(
                RpgMakerStandardAssetOwner::Builtin,
                SourceSnapshotFingerprint::from_bytes([0xa5; 32]),
                AssetSnapshotFingerprint::from_bytes([0xb4; 32]),
            )],
            "[]".to_owned(),
            "[]".to_owned(),
        );

        let SqliteTransactionStep::RequireNoRows(query) = require_snapshot_baseline(&baseline)
        else {
            panic!("baseline 应生成核对查询");
        };

        assert!(query.statement().contains("asset_snapshot_fingerprint"));
        assert!(
            query
                .parameters()
                .contains(&SqliteValue::Blob(vec![0xb4; 32]))
        );
    }

    fn scalar_identity(
        index: usize,
        field: &str,
        original: &str,
        context: &str,
    ) -> TranslationLeafIdentity {
        TranslationLeafIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            data_group(index),
            TextFieldRole::Scalar(ScalarFieldKey::new(field).expect("字段键应合法")),
            original,
            context,
        )
    }

    fn dialogue_body_identity(
        index: usize,
        original: &str,
        context: &str,
    ) -> TranslationLeafIdentity {
        TranslationLeafIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::EventDialogue,
            RpgMakerLocation::value(
                RpgMakerSource::map(1),
                vec![
                    RpgMakerLocationStep::key("events"),
                    RpgMakerLocationStep::index(index),
                    RpgMakerLocationStep::key("list"),
                    RpgMakerLocationStep::index(0),
                ],
            ),
            TextFieldRole::DialogueBody { index: 0 },
            original,
            context,
        )
    }

    fn data_group(index: usize) -> RpgMakerLocation {
        RpgMakerLocation::value(
            RpgMakerSource::data(StandardDataFile::Items),
            vec![RpgMakerLocationStep::index(index)],
        )
    }

    fn state_context(byte: u8) -> TranslationStateContext {
        TranslationStateContext::new(Sha256Fingerprint::from_bytes([byte; 32]))
    }

    fn project() -> OpenedProject {
        OpenedProject::new(
            "demo".parse::<ProjectName>().expect("项目名应合法"),
            PathBuf::from("C:/projects/demo"),
            PathBuf::from("C:/projects/demo/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::rpg_maker::project::test_layout_profile(),
        )
    }
}
