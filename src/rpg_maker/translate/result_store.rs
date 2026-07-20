//! 标准译文状态的原子对账与提交。

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;

use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::fingerprint::Sha256Fingerprint;
use crate::rpg_maker::location_codec::{
    RpgMakerLocationCodec, RpgMakerLocationCodecError, RpgMakerProjectionCodec,
    RpgMakerProjectionCodecError,
};
use crate::rpg_maker::model::TextUnitContent;
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::project_database::{
    PLACEHOLDER_RULES_RESOURCE_KIND, TERMINOLOGY_RESOURCE_KIND,
};
use crate::storage::sqlite::{
    ExecuteTransactionError, SqliteBatch, SqliteCommand, SqliteQuery, SqliteTransactionExecutor,
    SqliteTransactionPlan, SqliteTransactionStep, SqliteValue,
};

use super::standard::{
    StandardTranslationResultStore, TranslationPlanPreparation, TranslationSnapshotBaseline,
    TranslationTaskOutcome, TranslationUnitIdentity, ValidatedTranslationTaskResult,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerStandardTranslationResultStorageConfig {
    units_per_encode_job: NonZeroUsize,
}

impl RpgMakerStandardTranslationResultStorageConfig {
    pub(crate) const fn new(units_per_encode_job: NonZeroUsize) -> Self {
        Self {
            units_per_encode_job,
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
    type PreparedCommit = RpgMakerPreparedTranslationCommit;
    type Error = RpgMakerStandardTranslationResultStorageError<S::Error, C::Error>;

    async fn apply_preparation(
        &self,
        project: &OpenedProject,
        preparation: TranslationPlanPreparation,
    ) -> Result<(), Self::Error> {
        let plan = self.encode_preparation_plan(preparation).await?;
        self.execute(project.database_path().to_path_buf(), plan)
            .await
    }

    async fn prepare_commit(
        &self,
        outcome: Arc<TranslationTaskOutcome>,
    ) -> Result<Self::PreparedCommit, Self::Error> {
        self.encode_commit_plan(outcome).await
    }

    async fn commit_prepared(
        &self,
        project: &OpenedProject,
        prepared: Self::PreparedCommit,
    ) -> Result<(), Self::Error> {
        let RpgMakerPreparedTranslationCommit { plan } = prepared;
        self.execute(project.database_path().to_path_buf(), plan)
            .await
    }
}

/// 已完成全部纯计算编码与校验、只等待独立事务提交的任务结果。
pub(crate) struct RpgMakerPreparedTranslationCommit {
    plan: SqliteTransactionPlan,
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
        let units_per_job = self.config.units_per_encode_job.get();
        let (jobs, terminology_json, placeholder_rules_json, snapshot_baseline) = self
            .cpu
            .execute(move || {
                let (work, terminology_json, placeholder_rules_json, snapshot_baseline) =
                    preparation_work(preparation)?;
                Ok::<_, ResultStoragePlanError>((
                    split_jobs(work, units_per_job),
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
        outcome: Arc<TranslationTaskOutcome>,
    ) -> Result<
        RpgMakerPreparedTranslationCommit,
        RpgMakerStandardTranslationResultStorageError<S::Error, C::Error>,
    > {
        let units_per_job = self.config.units_per_encode_job.get();
        let jobs = self
            .cpu
            .execute(move || {
                let result = outcome
                    .validated_result()
                    .expect("Store 只应准备至少包含一项合格译文的任务结果");
                commit_work(result).map(|work| split_jobs(work, units_per_job))
            })
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
                Ok::<_, ResultStoragePlanError>(RpgMakerPreparedTranslationCommit {
                    plan: finish_commit_plan(batches.into_iter().collect::<Result<Vec<_>, _>>()?)?,
                })
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
    Content(serde_json::Error),
    EmptyTaskResult,
    EmptyReuseTargets,
    BlankTranslation,
    InconsistentTranslationState,
    MismatchedReuseSourceContent,
    MismatchedReuseSourceContext,
    MismatchedPropagationSourceContent,
    MismatchedPropagationSourceContext,
    DuplicateUnit,
}

impl fmt::Display for ResultStoragePlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Location(source) => source.fmt(formatter),
            Self::Projection(source) => source.fmt(formatter),
            Self::Content(source) => write!(formatter, "文本单元内容无法编码为 JSON：{source}"),
            Self::EmptyTaskResult => formatter.write_str("任务结果不包含任何译文"),
            Self::EmptyReuseTargets => formatter.write_str("译文复用计划不包含任何目标"),
            Self::BlankTranslation => formatter.write_str("任务结果包含空白译文"),
            Self::InconsistentTranslationState => {
                formatter.write_str("读取时的译文与译文状态没有同时存在或同时缺失")
            }
            Self::MismatchedReuseSourceContent => {
                formatter.write_str("译文复用种子与目标的完整源内容不一致")
            }
            Self::MismatchedReuseSourceContext => {
                formatter.write_str("译文复用种子与目标的源上下文不一致")
            }
            Self::MismatchedPropagationSourceContent => {
                formatter.write_str("译文代表与传播目标的完整源内容不一致")
            }
            Self::MismatchedPropagationSourceContext => {
                formatter.write_str("译文代表与传播目标的源上下文不一致")
            }
            Self::DuplicateUnit => formatter.write_str("同一事务重复修改同一文本单元"),
        }
    }
}

impl Error for ResultStoragePlanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Location(source) => Some(source),
            Self::Projection(source) => Some(source),
            Self::Content(source) => Some(source),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct EncodedIdentity {
    owner: &'static str,
    group_location: String,
    unit_role: String,
    source_content_json: String,
    source_context_json: String,
}

enum PreparationUnitWork {
    Invalidation {
        identity: TranslationUnitIdentity,
        expected_translation: TextUnitContent,
        expected_translation_state: Sha256Fingerprint,
    },
    ReuseSeed {
        identity: TranslationUnitIdentity,
        expected_translation: TextUnitContent,
        expected_translation_state: Sha256Fingerprint,
    },
    ReuseTarget {
        seed_source_content: TextUnitContent,
        seed_source_context_json: String,
        translation: TextUnitContent,
        identity: TranslationUnitIdentity,
        expected_translation: Option<TextUnitContent>,
        expected_translation_state: Option<Sha256Fingerprint>,
        replacement_translation_state: Sha256Fingerprint,
    },
}

enum EncodedPreparationUnit {
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

struct CommitUnitWork {
    identity: TranslationUnitIdentity,
    required_source_content: Option<TextUnitContent>,
    required_source_context_json: Option<String>,
    translation: TextUnitContent,
    translation_state: Sha256Fingerprint,
}

struct EncodedCommitUnit {
    identity: EncodedIdentity,
    translation: String,
    translation_state: Sha256Fingerprint,
}

fn preparation_work(
    preparation: TranslationPlanPreparation,
) -> Result<
    (
        Vec<PreparationUnitWork>,
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
        work.push(PreparationUnitWork::Invalidation {
            identity: invalidation.identity().clone(),
            expected_translation: invalidation.expected_translation().clone(),
            expected_translation_state: invalidation.expected_translation_state(),
        });
    }

    for reuse in reuses {
        if reuse.targets().is_empty() {
            return Err(ResultStoragePlanError::EmptyReuseTargets);
        }
        let seed_source_content = reuse.seed().identity().source_content().clone();
        let seed_source_context_json = reuse.seed().identity().source_context_json().to_owned();
        let translation = reuse.seed().expected_translation().clone();
        work.push(PreparationUnitWork::ReuseSeed {
            identity: reuse.seed().identity().clone(),
            expected_translation: translation.clone(),
            expected_translation_state: reuse.seed().expected_translation_state(),
        });

        for target in reuse.targets() {
            work.push(PreparationUnitWork::ReuseTarget {
                seed_source_content: seed_source_content.clone(),
                seed_source_context_json: seed_source_context_json.clone(),
                translation: translation.clone(),
                identity: target.identity().clone(),
                expected_translation: target.expected_translation().cloned(),
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
    job: Vec<PreparationUnitWork>,
) -> Result<Vec<EncodedPreparationUnit>, ResultStoragePlanError> {
    job.into_iter()
        .map(|work| match work {
            PreparationUnitWork::Invalidation {
                identity,
                expected_translation,
                expected_translation_state,
            } => {
                ensure_nonblank(&expected_translation)?;
                Ok(EncodedPreparationUnit::Invalidation {
                    identity: encode_identity(&identity)?,
                    expected_translation: encode_content(&expected_translation)?,
                    expected_translation_state,
                })
            }
            PreparationUnitWork::ReuseSeed {
                identity,
                expected_translation,
                expected_translation_state,
            } => {
                ensure_nonblank(&expected_translation)?;
                Ok(EncodedPreparationUnit::ReuseSeed {
                    identity: encode_identity(&identity)?,
                    expected_translation: encode_content(&expected_translation)?,
                    expected_translation_state,
                })
            }
            PreparationUnitWork::ReuseTarget {
                seed_source_content,
                seed_source_context_json,
                translation,
                identity,
                expected_translation,
                expected_translation_state,
                replacement_translation_state,
            } => {
                ensure_nonblank(&translation)?;
                if identity.source_content() != &seed_source_content {
                    return Err(ResultStoragePlanError::MismatchedReuseSourceContent);
                }
                if identity.source_context_json() != seed_source_context_json {
                    return Err(ResultStoragePlanError::MismatchedReuseSourceContext);
                }
                if expected_translation.is_some() != expected_translation_state.is_some() {
                    return Err(ResultStoragePlanError::InconsistentTranslationState);
                }
                if expected_translation
                    .as_ref()
                    .is_some_and(TextUnitContent::is_blank)
                {
                    return Err(ResultStoragePlanError::BlankTranslation);
                }
                Ok(EncodedPreparationUnit::ReuseTarget {
                    translation: encode_content(&translation)?,
                    identity: encode_identity(&identity)?,
                    expected_translation: expected_translation
                        .as_ref()
                        .map(encode_content)
                        .transpose()?,
                    expected_translation_state,
                    replacement_translation_state,
                })
            }
        })
        .collect()
}

fn finish_preparation_plan(
    batches: Vec<Vec<EncodedPreparationUnit>>,
    terminology_json: String,
    placeholder_rules_json: String,
    snapshot_baseline: TranslationSnapshotBaseline,
) -> Result<SqliteTransactionPlan, ResultStoragePlanError> {
    let mut steps = vec![require_snapshot_baseline(&snapshot_baseline)];
    let mut seen = BTreeSet::new();
    let mut snapshot_parameter_sets = Vec::new();
    let mut clear_parameter_sets = Vec::new();
    let mut reuse_parameter_sets = Vec::new();
    for unit in batches.into_iter().flatten() {
        match unit {
            EncodedPreparationUnit::Invalidation {
                identity,
                expected_translation,
                expected_translation_state,
            } => {
                ensure_unique(&mut seen, &identity)?;
                snapshot_parameter_sets.push(snapshot_parameters(
                    &identity,
                    Some((&expected_translation, expected_translation_state)),
                ));
                clear_parameter_sets.push(clear_translation_parameters(
                    identity,
                    expected_translation,
                    expected_translation_state,
                ));
            }
            EncodedPreparationUnit::ReuseSeed {
                identity,
                expected_translation,
                expected_translation_state,
            } => {
                ensure_unique(&mut seen, &identity)?;
                snapshot_parameter_sets.push(snapshot_parameters(
                    &identity,
                    Some((&expected_translation, expected_translation_state)),
                ));
            }
            EncodedPreparationUnit::ReuseTarget {
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
                snapshot_parameter_sets.push(snapshot_parameters(&identity, expected));
                reuse_parameter_sets.push(write_translation_from_snapshot_parameters(
                    identity,
                    translation,
                    replacement_translation_state,
                    expected_translation,
                    expected_translation_state,
                ));
            }
        }
    }
    if !snapshot_parameter_sets.is_empty() {
        steps.push(SqliteTransactionStep::RequireNoRowsMany(SqliteBatch::new(
            REQUIRE_SNAPSHOT,
            snapshot_parameter_sets,
        )));
    }
    if !clear_parameter_sets.is_empty() {
        steps.push(SqliteTransactionStep::ExecuteManyExactlyOne(
            SqliteBatch::new(CLEAR_TRANSLATION_FROM_SNAPSHOT, clear_parameter_sets),
        ));
    }
    if !reuse_parameter_sets.is_empty() {
        steps.push(SqliteTransactionStep::ExecuteManyExactlyOne(
            SqliteBatch::new(WRITE_TRANSLATION_FROM_SNAPSHOT, reuse_parameter_sets),
        ));
    }
    steps.extend(resource_updates(terminology_json, placeholder_rules_json));
    Ok(SqliteTransactionPlan::new(steps))
}

fn commit_work(
    result: ValidatedTranslationTaskResult,
) -> Result<Vec<CommitUnitWork>, ResultStoragePlanError> {
    let patches = result.into_updates();
    if patches.is_empty() {
        return Err(ResultStoragePlanError::EmptyTaskResult);
    }
    let mut work = Vec::new();
    for patch in patches {
        let translation = patch.translation().clone();
        work.push(CommitUnitWork {
            identity: patch.identity().clone(),
            required_source_content: None,
            required_source_context_json: None,
            translation: translation.clone(),
            translation_state: patch.translation_state(),
        });
        for target in patch.propagation_targets() {
            work.push(CommitUnitWork {
                identity: target.identity().clone(),
                required_source_content: Some(patch.identity().source_content().clone()),
                required_source_context_json: Some(
                    patch.identity().source_context_json().to_owned(),
                ),
                translation: translation.clone(),
                translation_state: target.state_context().finish(&translation),
            });
        }
    }
    Ok(work)
}

fn encode_commit_job(
    job: Vec<CommitUnitWork>,
) -> Result<Vec<EncodedCommitUnit>, ResultStoragePlanError> {
    job.into_iter()
        .map(|work| {
            ensure_nonblank(&work.translation)?;
            if work
                .required_source_content
                .as_ref()
                .is_some_and(|source_content| source_content != work.identity.source_content())
            {
                return Err(ResultStoragePlanError::MismatchedPropagationSourceContent);
            }
            if work
                .required_source_context_json
                .as_deref()
                .is_some_and(|context| context != work.identity.source_context_json())
            {
                return Err(ResultStoragePlanError::MismatchedPropagationSourceContext);
            }
            Ok(EncodedCommitUnit {
                identity: encode_identity(&work.identity)?,
                translation: encode_content(&work.translation)?,
                translation_state: work.translation_state,
            })
        })
        .collect()
}

fn finish_commit_plan(
    batches: Vec<Vec<EncodedCommitUnit>>,
) -> Result<SqliteTransactionPlan, ResultStoragePlanError> {
    let mut seen = BTreeSet::new();
    let mut parameter_sets = Vec::new();
    for unit in batches.into_iter().flatten() {
        ensure_unique(&mut seen, &unit.identity)?;
        parameter_sets.push(commit_translation_parameters(unit));
    }
    Ok(SqliteTransactionPlan::new(vec![
        SqliteTransactionStep::ExecuteManyExactlyOne(SqliteBatch::new(
            COMMIT_TRANSLATION,
            parameter_sets,
        )),
    ]))
}

fn split_jobs<T>(items: Vec<T>, units_per_job: usize) -> Vec<Vec<T>> {
    debug_assert!(units_per_job > 0);
    let mut items = items.into_iter();
    std::iter::from_fn(|| {
        let job = items.by_ref().take(units_per_job).collect::<Vec<_>>();
        (!job.is_empty()).then_some(job)
    })
    .collect()
}

fn encode_identity(
    identity: &TranslationUnitIdentity,
) -> Result<EncodedIdentity, ResultStoragePlanError> {
    Ok(EncodedIdentity {
        owner: identity.owner().storage_name(),
        group_location: RpgMakerLocationCodec::encode(identity.group_location())
            .map_err(ResultStoragePlanError::Location)?,
        unit_role: RpgMakerProjectionCodec::encode_role(identity.role())
            .map_err(ResultStoragePlanError::Projection)?,
        source_content_json: encode_content(identity.source_content())?,
        source_context_json: identity.source_context_json().to_owned(),
    })
}

fn encode_content(content: &TextUnitContent) -> Result<String, ResultStoragePlanError> {
    serde_json::to_string(content).map_err(ResultStoragePlanError::Content)
}

fn ensure_unique(
    seen: &mut BTreeSet<(&'static str, String, String)>,
    identity: &EncodedIdentity,
) -> Result<(), ResultStoragePlanError> {
    if seen.insert((
        identity.owner,
        identity.group_location.clone(),
        identity.unit_role.clone(),
    )) {
        Ok(())
    } else {
        Err(ResultStoragePlanError::DuplicateUnit)
    }
}

fn ensure_nonblank(translation: &TextUnitContent) -> Result<(), ResultStoragePlanError> {
    if translation.is_blank() {
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

const REQUIRE_SNAPSHOT: &str = "SELECT 1 WHERE NOT EXISTS (SELECT 1 FROM standard_text_unit WHERE owner = ?1 AND group_location = ?2 AND unit_role = ?3 AND source_content_json = ?4 AND source_context_json = ?5 AND ((?6 IS NULL AND ?7 IS NULL AND translation_content_json IS NULL AND translation_state IS NULL) OR (translation_content_json = ?6 AND translation_state = ?7)))";

const CLEAR_TRANSLATION_FROM_SNAPSHOT: &str = "UPDATE standard_text_unit SET translation_content_json = NULL, translation_state = NULL WHERE owner = ?1 AND group_location = ?2 AND unit_role = ?3 AND source_content_json = ?4 AND source_context_json = ?5 AND translation_content_json = ?6 AND translation_state = ?7";

const WRITE_TRANSLATION_FROM_SNAPSHOT: &str = "UPDATE standard_text_unit SET translation_content_json = ?1, translation_state = ?2 WHERE owner = ?3 AND group_location = ?4 AND unit_role = ?5 AND source_content_json = ?6 AND source_context_json = ?7 AND ((?8 IS NULL AND ?9 IS NULL AND translation_content_json IS NULL AND translation_state IS NULL) OR (translation_content_json = ?8 AND translation_state = ?9))";

const COMMIT_TRANSLATION: &str = "UPDATE standard_text_unit SET translation_content_json = ?1, translation_state = ?2 WHERE owner = ?3 AND group_location = ?4 AND unit_role = ?5 AND source_content_json = ?6 AND source_context_json = ?7 AND translation_content_json IS NULL AND translation_state IS NULL";

fn snapshot_parameters(
    identity: &EncodedIdentity,
    expected_translation: Option<(&str, Sha256Fingerprint)>,
) -> Vec<SqliteValue> {
    let mut parameters = vec![
        text(identity.owner),
        text(identity.group_location.clone()),
        text(identity.unit_role.clone()),
        text(identity.source_content_json.clone()),
        text(identity.source_context_json.clone()),
    ];
    if let Some((translation, state)) = expected_translation {
        parameters.push(text(translation));
        parameters.push(blob(state));
    } else {
        parameters.extend([SqliteValue::Null, SqliteValue::Null]);
    }
    parameters
}

fn clear_translation_parameters(
    identity: EncodedIdentity,
    expected_translation: String,
    expected_translation_state: Sha256Fingerprint,
) -> Vec<SqliteValue> {
    vec![
        text(identity.owner),
        text(identity.group_location),
        text(identity.unit_role),
        text(identity.source_content_json),
        text(identity.source_context_json),
        text(expected_translation),
        blob(expected_translation_state),
    ]
}

fn write_translation_from_snapshot_parameters(
    identity: EncodedIdentity,
    translation: String,
    replacement_translation_state: Sha256Fingerprint,
    expected_translation: Option<String>,
    expected_translation_state: Option<Sha256Fingerprint>,
) -> Vec<SqliteValue> {
    vec![
        text(translation),
        blob(replacement_translation_state),
        text(identity.owner),
        text(identity.group_location),
        text(identity.unit_role),
        text(identity.source_content_json),
        text(identity.source_context_json),
        expected_translation.map_or(SqliteValue::Null, text),
        expected_translation_state.map_or(SqliteValue::Null, blob),
    ]
}

fn commit_translation_parameters(unit: EncodedCommitUnit) -> Vec<SqliteValue> {
    vec![
        text(unit.translation),
        blob(unit.translation_state),
        text(unit.identity.owner),
        text(unit.identity.group_location),
        text(unit.identity.unit_role),
        text(unit.identity.source_content_json),
        text(unit.identity.source_context_json),
    ]
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
    use std::time::Duration;

    use crate::rpg_maker::ProjectName;
    use crate::rpg_maker::model::{ScalarFieldKey, TextUnitContent, TextUnitRole};
    use crate::rpg_maker::project_database::{AssetSnapshotFingerprint, SourceSnapshotFingerprint};
    use crate::rpg_maker::standard_asset::RpgMakerStandardAssetOwner;
    use crate::rpg_maker::text::{
        RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, StandardDataFile, TextGroupKind,
    };
    use crate::runtime::sqlite::{
        RusqliteStorage, RusqliteStorageConfiguration, SqliteJournalMode, SqliteSynchronous,
    };
    use crate::storage::sqlite::SqliteTransactionStep;
    use rusqlite::{Connection, params};

    use super::*;
    use crate::rpg_maker::translate::executor::FinalLlmResponseMetadata;
    use crate::rpg_maker::translate::standard::{
        AcceptedTranslationDecision, NonEmptyTaskItems, StandardTranslationTaskIndex,
        TranslationInvalidation, TranslationOwnerSnapshot, TranslationPatch,
        TranslationPropagationTarget, TranslationReuse, TranslationReuseSeed,
        TranslationReuseTarget, TranslationSnapshotBaseline, TranslationStateContext,
        TranslationTaskOutcomeContext,
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
    async fn prepared_commit_uses_one_conditional_batch_update_for_all_unit_checks() {
        let sqlite = RecordingSqlite::default();
        let plans = Arc::clone(&sqlite.plans);
        let service = RpgMakerStandardTranslationResultStorageService::new(
            sqlite,
            InlineCpu,
            RpgMakerStandardTranslationResultStorageConfig::new(NonZeroUsize::MIN),
        );
        let identity = scalar_identity(1, "name", "原文", "{}");
        let context = state_context(0x11);
        let translation = value("译文");
        let outcome = Arc::new(TranslationTaskOutcome::Complete {
            context: TranslationTaskOutcomeContext::new(
                StandardTranslationTaskIndex::new(0),
                NonZeroUsize::MIN,
                Vec::new(),
            ),
            final_response: FinalLlmResponseMetadata::new(None, None, "stop", None),
            accepted: NonEmptyTaskItems::new(
                AcceptedTranslationDecision::new(
                    0,
                    TranslationPatch::new(
                        identity.clone(),
                        Vec::new(),
                        translation.clone(),
                        context.finish(&translation),
                    ),
                ),
                Vec::new(),
            ),
        });

        let prepared = service
            .prepare_commit(Arc::clone(&outcome))
            .await
            .expect("提交准备应成功");
        assert_eq!(Arc::strong_count(&outcome), 1);
        assert!(plans.lock().expect("事务锁").is_empty());
        service
            .commit_prepared(&project(), prepared)
            .await
            .expect("提交应成功");

        let plans = plans.lock().expect("事务锁");
        let plan = &plans[0].1;
        assert_eq!(plan.steps().len(), 1);
        let SqliteTransactionStep::ExecuteManyExactlyOne(update) = &plan.steps()[0] else {
            panic!("提交应以一条条件批量修改核对并写入逻辑单元");
        };
        assert_eq!(update.statement(), COMMIT_TRANSLATION);
        assert!(update.statement().contains("source_content_json = ?6"));
        assert!(update.statement().contains("source_context_json = ?7"));
        assert!(
            update
                .statement()
                .contains("translation_content_json IS NULL")
        );
        assert!(update.statement().contains("translation_state IS NULL"));
        assert!(!update.statement().contains("exact_location"));
        let parameters = &update.parameter_sets()[0];
        assert_eq!(
            parameters[3],
            SqliteValue::Text(
                RpgMakerLocationCodec::encode(identity.group_location()).expect("组位置应可编码")
            )
        );
        assert_eq!(
            parameters[4],
            SqliteValue::Text(
                RpgMakerProjectionCodec::encode_role(identity.role()).expect("角色应可编码")
            )
        );
        assert_eq!(parameters[5], SqliteValue::Text(r#""原文""#.to_owned()));
        assert_eq!(parameters[6], SqliteValue::Text("{}".to_owned()));
    }

    #[tokio::test]
    async fn preparation_batches_guards_clears_and_reuse_in_natural_order() {
        let sqlite = RecordingSqlite::default();
        let plans = Arc::clone(&sqlite.plans);
        let service = RpgMakerStandardTranslationResultStorageService::new(
            sqlite,
            InlineCpu,
            RpgMakerStandardTranslationResultStorageConfig::new(NonZeroUsize::MIN),
        );
        let invalidation_one = scalar_identity(1, "name", "原文一", "{}");
        let invalidation_two = scalar_identity(2, "name", "原文二", "{}");
        let reuse_seed = scalar_identity(3, "name", "共享原文", "{}");
        let reuse_target = scalar_identity(4, "name", "共享原文", "{}");
        let preparation = TranslationPlanPreparation::new(
            vec![
                TranslationInvalidation::new(
                    invalidation_one.clone(),
                    value("旧译文一"),
                    Sha256Fingerprint::from_bytes([0x11; 32]),
                ),
                TranslationInvalidation::new(
                    invalidation_two.clone(),
                    value("旧译文二"),
                    Sha256Fingerprint::from_bytes([0x22; 32]),
                ),
            ],
            vec![TranslationReuse::new(
                TranslationReuseSeed::new(
                    reuse_seed.clone(),
                    value("复用译文"),
                    Sha256Fingerprint::from_bytes([0x33; 32]),
                ),
                vec![TranslationReuseTarget::new(
                    reuse_target.clone(),
                    None,
                    None,
                    Sha256Fingerprint::from_bytes([0x44; 32]),
                )],
            )],
            "[]".to_owned(),
            "[]".to_owned(),
            0,
            2,
            0,
        );

        service
            .apply_preparation(&project(), preparation)
            .await
            .expect("准备事务应成功");

        let plans = plans.lock().expect("事务锁");
        let steps = plans[0].1.steps();
        assert!(matches!(steps[0], SqliteTransactionStep::RequireNoRows(_)));
        let SqliteTransactionStep::RequireNoRowsMany(guards) = &steps[1] else {
            panic!("逐单元快照条件应批量查询");
        };
        assert_eq!(guards.statement(), REQUIRE_SNAPSHOT);
        assert_eq!(guards.parameter_sets().len(), 4);
        let expected_groups = [
            &invalidation_one,
            &invalidation_two,
            &reuse_seed,
            &reuse_target,
        ]
        .map(|identity| {
            SqliteValue::Text(
                RpgMakerLocationCodec::encode(identity.group_location()).expect("位置应可编码"),
            )
        });
        assert_eq!(
            guards
                .parameter_sets()
                .iter()
                .map(|parameters| parameters[1].clone())
                .collect::<Vec<_>>(),
            expected_groups
        );

        let SqliteTransactionStep::ExecuteManyExactlyOne(clears) = &steps[2] else {
            panic!("失效清理应批量条件修改");
        };
        assert_eq!(clears.statement(), CLEAR_TRANSLATION_FROM_SNAPSHOT);
        assert_eq!(clears.parameter_sets().len(), 2);
        assert_eq!(
            clears.parameter_sets()[0][5],
            SqliteValue::Text(r#""旧译文一""#.to_owned())
        );

        let SqliteTransactionStep::ExecuteManyExactlyOne(reuses) = &steps[3] else {
            panic!("复用写入应批量条件修改");
        };
        assert_eq!(reuses.statement(), WRITE_TRANSLATION_FROM_SNAPSHOT);
        assert_eq!(reuses.parameter_sets().len(), 1);
        assert_eq!(
            reuses.parameter_sets()[0][0],
            SqliteValue::Text(r#""复用译文""#.to_owned())
        );
        assert_eq!(reuses.parameter_sets()[0][7], SqliteValue::Null);
        assert_eq!(reuses.parameter_sets()[0][8], SqliteValue::Null);
    }

    #[tokio::test]
    async fn preparation_without_writes_still_checks_the_complete_snapshot_baseline() {
        let sqlite = RecordingSqlite::default();
        let plans = Arc::clone(&sqlite.plans);
        let service = RpgMakerStandardTranslationResultStorageService::new(
            sqlite,
            InlineCpu,
            RpgMakerStandardTranslationResultStorageConfig::new(NonZeroUsize::MIN),
        );
        let preparation = TranslationPlanPreparation::new(
            Vec::new(),
            Vec::new(),
            "[]".to_owned(),
            "[]".to_owned(),
            1,
            0,
            0,
        );

        service
            .apply_preparation(&project(), preparation)
            .await
            .expect("无状态写入的计划仍必须在请求模型前核对读取基线");

        let plans = plans.lock().expect("事务锁");
        assert_eq!(plans.len(), 1);
        assert!(!plans[0].1.steps().is_empty());
        assert!(matches!(
            plans[0].1.steps()[0],
            SqliteTransactionStep::RequireNoRows(_)
        ));
    }

    #[test]
    fn duplicate_logical_unit_is_rejected_before_a_commit_plan_is_created() {
        let identity = scalar_identity(1, "name", "原文", "{}");
        let context = state_context(0x55);
        let translation = value("译文");
        let patch = TranslationPatch::new(
            identity,
            Vec::new(),
            translation.clone(),
            context.finish(&translation),
        );
        let work = commit_work(ValidatedTranslationTaskResult::new(
            StandardTranslationTaskIndex::new(0),
            vec![patch.clone(), patch],
        ))
        .expect("重复应由最终计划阶段按编码身份识别");
        let encoded = encode_commit_job(work).expect("重复单元本身应可编码");

        let error = finish_commit_plan(vec![encoded]).expect_err("重复逻辑单元不得进入事务");
        assert!(matches!(error, ResultStoragePlanError::DuplicateUnit));
    }

    #[tokio::test]
    async fn real_sqlite_rejects_stale_source_context_and_translation_state() {
        let directory = tempfile::tempdir().expect("临时目录应可创建");
        let storage = runtime_storage();
        let service = RpgMakerStandardTranslationResultStorageService::new(
            storage.clone(),
            InlineCpu,
            RpgMakerStandardTranslationResultStorageConfig::new(NonZeroUsize::MIN),
        );
        let cases = [
            ("stale-source.db", "旧原文", "{}", None, None),
            ("stale-context.db", "原文", r#"{"old":true}"#, None, None),
            (
                "stale-translation.db",
                "原文",
                "{}",
                Some("旧译文"),
                Some(vec![0x61; 32]),
            ),
            ("orphan-state.db", "原文", "{}", None, Some(vec![0x62; 32])),
        ];

        for (name, actual_source, actual_context, translation, translation_state) in cases {
            let database_path = directory.path().join(name).join("project.db");
            let identity = scalar_identity(1, "name", "原文", "{}");
            create_unit_database(
                &database_path,
                &[StoredUnit::new(
                    &identity,
                    actual_source,
                    actual_context,
                    translation,
                    translation_state.as_deref(),
                )],
            );
            let prepared = service
                .prepare_commit(complete_outcome(vec![translation_patch(
                    identity, "译文", 0x71,
                )]))
                .await
                .expect("提交计划编码应成功");
            let error = service
                .commit_prepared(&project_at(database_path.clone()), prepared)
                .await
                .expect_err("过时快照必须拒绝");
            assert!(matches!(
                error,
                RpgMakerStandardTranslationResultStorageError::StalePlan { .. }
            ));
            assert_eq!(
                stored_translation(&database_path, 1),
                (translation.map(value), translation_state,),
                "被拒绝的条件 UPDATE 不得改变行"
            );
        }

        drop(service);
        storage.shutdown().await.expect("SQLite 根应正常关闭");
    }

    #[tokio::test]
    async fn real_sqlite_rolls_back_earlier_batch_updates_when_a_later_unit_is_stale() {
        let directory = tempfile::tempdir().expect("临时目录应可创建");
        let database_path = directory.path().join("rollback").join("project.db");
        let first = scalar_identity(1, "name", "原文一", "{}");
        let second = scalar_identity(2, "name", "原文二", "{}");
        create_unit_database(
            &database_path,
            &[
                StoredUnit::new(&first, "原文一", "{}", None, None),
                StoredUnit::new(&second, "原文二", r#"{"old":true}"#, None, None),
            ],
        );
        let storage = runtime_storage();
        let service = RpgMakerStandardTranslationResultStorageService::new(
            storage.clone(),
            InlineCpu,
            RpgMakerStandardTranslationResultStorageConfig::new(NonZeroUsize::MIN),
        );
        let prepared = service
            .prepare_commit(complete_outcome(vec![
                translation_patch(first, "译文一", 0x81),
                translation_patch(second, "译文二", 0x82),
            ]))
            .await
            .expect("提交计划编码应成功");

        let error = service
            .commit_prepared(&project_at(database_path.clone()), prepared)
            .await
            .expect_err("后序单元过时必须回滚整笔任务");
        assert!(matches!(
            error,
            RpgMakerStandardTranslationResultStorageError::StalePlan { .. }
        ));
        assert_eq!(stored_translation(&database_path, 1), (None, None));
        assert_eq!(stored_translation(&database_path, 2), (None, None));

        drop(service);
        storage.shutdown().await.expect("SQLite 根应正常关闭");
    }

    #[tokio::test]
    async fn real_sqlite_rejects_duplicate_physical_rows_and_rolls_back_them_both() {
        let directory = tempfile::tempdir().expect("临时目录应可创建");
        let database_path = directory.path().join("duplicate-rows").join("project.db");
        let identity = scalar_identity(1, "name", "原文", "{}");
        create_unit_database(
            &database_path,
            &[
                StoredUnit::new(&identity, "原文", "{}", None, None),
                StoredUnit::new(&identity, "原文", "{}", None, None),
            ],
        );
        let storage = runtime_storage();
        let service = RpgMakerStandardTranslationResultStorageService::new(
            storage.clone(),
            InlineCpu,
            RpgMakerStandardTranslationResultStorageConfig::new(NonZeroUsize::MIN),
        );
        let prepared = service
            .prepare_commit(complete_outcome(vec![translation_patch(
                identity, "译文", 0x91,
            )]))
            .await
            .expect("提交计划编码应成功");

        let error = service
            .commit_prepared(&project_at(database_path.clone()), prepared)
            .await
            .expect_err("修改多行必须视为快照失效");
        assert!(matches!(
            error,
            RpgMakerStandardTranslationResultStorageError::StalePlan { .. }
        ));
        let connection = Connection::open(&database_path).expect("数据库应可重开");
        let translated: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM standard_text_unit WHERE translation_content_json IS NOT NULL OR translation_state IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .expect("应可核对回滚结果");
        assert_eq!(translated, 0);

        drop(connection);
        drop(service);
        storage.shutdown().await.expect("SQLite 根应正常关闭");
    }

    #[test]
    fn body_propagation_rejects_a_different_source_speaker_context() {
        let leader = dialogue_body_identity(1, "同一句", r#"{"source_speaker":"甲"}"#);
        let target = dialogue_body_identity(2, "同一句", r#"{"source_speaker":"乙"}"#);
        let context = state_context(0x22);
        let translation = lines(&["相同译文"]);
        let result = ValidatedTranslationTaskResult::new(
            StandardTranslationTaskIndex::new(0),
            vec![TranslationPatch::new(
                leader,
                vec![TranslationPropagationTarget::new(target, context)],
                translation.clone(),
                context.finish(&translation),
            )],
        );

        let work = commit_work(result).expect("结果形状应合法");
        let Err(error) = encode_commit_job(work) else {
            panic!("不同 Speaker 上下文不得传播");
        };

        assert!(matches!(
            error,
            ResultStoragePlanError::MismatchedPropagationSourceContext
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

    struct StoredUnit<'a> {
        identity: &'a TranslationUnitIdentity,
        source_content_json: String,
        source_context_json: &'a str,
        translation_content_json: Option<String>,
        translation_state: Option<&'a [u8]>,
    }

    impl<'a> StoredUnit<'a> {
        fn new(
            identity: &'a TranslationUnitIdentity,
            source_content: &str,
            source_context_json: &'a str,
            translation: Option<&'a str>,
            translation_state: Option<&'a [u8]>,
        ) -> Self {
            Self {
                identity,
                source_content_json: encode_content(&value(source_content))
                    .expect("测试源内容应可编码"),
                source_context_json,
                translation_content_json: translation
                    .map(value)
                    .as_ref()
                    .map(encode_content)
                    .transpose()
                    .expect("测试译文应可编码"),
                translation_state,
            }
        }
    }

    fn create_unit_database(path: &std::path::Path, units: &[StoredUnit<'_>]) {
        std::fs::create_dir_all(path.parent().expect("测试数据库必须有父目录"))
            .expect("测试数据库目录应可创建");
        let connection = Connection::open(path).expect("测试数据库应可创建");
        connection
            .execute_batch(
                "CREATE TABLE standard_text_unit (
                    owner TEXT NOT NULL,
                    group_location TEXT NOT NULL,
                    unit_role TEXT NOT NULL,
                    source_content_json TEXT NOT NULL,
                    source_context_json TEXT NOT NULL,
                    translation_content_json TEXT,
                    translation_state BLOB
                );",
            )
            .expect("测试表应可创建");
        for unit in units {
            connection
                .execute(
                    "INSERT INTO standard_text_unit (
                        owner, group_location, unit_role, source_content_json,
                        source_context_json, translation_content_json, translation_state
                    ) VALUES (?, ?, ?, ?, ?, ?, ?)",
                    params![
                        unit.identity.owner().storage_name(),
                        RpgMakerLocationCodec::encode(unit.identity.group_location())
                            .expect("组位置应可编码"),
                        RpgMakerProjectionCodec::encode_role(unit.identity.role())
                            .expect("单元角色应可编码"),
                        &unit.source_content_json,
                        unit.source_context_json,
                        &unit.translation_content_json,
                        unit.translation_state,
                    ],
                )
                .expect("测试单元应可写入");
        }
    }

    fn stored_translation(
        path: &std::path::Path,
        rowid: usize,
    ) -> (Option<TextUnitContent>, Option<Vec<u8>>) {
        Connection::open(path)
            .expect("数据库应可重开")
            .query_row(
                "SELECT translation_content_json, translation_state FROM standard_text_unit WHERE rowid = ?",
                [i64::try_from(rowid).expect("测试 rowid 应可表示为 i64")],
                |row| {
                    let translation_json: Option<String> = row.get(0)?;
                    Ok((
                        translation_json.map(|json| {
                            serde_json::from_str(&json).expect("测试译文 JSON 应可解码")
                        }),
                        row.get(1)?,
                    ))
                },
            )
            .expect("测试单元应仍存在")
    }

    fn translation_patch(
        identity: TranslationUnitIdentity,
        translation: &str,
        state_byte: u8,
    ) -> TranslationPatch {
        let translation = value(translation);
        TranslationPatch::new(
            identity,
            Vec::new(),
            translation.clone(),
            state_context(state_byte).finish(&translation),
        )
    }

    fn complete_outcome(patches: Vec<TranslationPatch>) -> Arc<TranslationTaskOutcome> {
        let mut decisions = patches
            .into_iter()
            .enumerate()
            .map(|(id, patch)| AcceptedTranslationDecision::new(id, patch));
        let first = decisions.next().expect("测试任务至少包含一项译文");
        Arc::new(TranslationTaskOutcome::Complete {
            context: TranslationTaskOutcomeContext::new(
                StandardTranslationTaskIndex::new(0),
                NonZeroUsize::MIN,
                Vec::new(),
            ),
            final_response: FinalLlmResponseMetadata::new(None, None, "stop", None),
            accepted: NonEmptyTaskItems::new(first, decisions.collect()),
        })
    }

    fn runtime_storage() -> RusqliteStorage {
        let nonzero = |value| NonZeroUsize::new(value).expect("测试资源预算必须非零");
        let config = RusqliteStorageConfiguration::new(
            nonzero(2),
            nonzero(8),
            nonzero(4),
            nonzero(1024 * 1024),
            nonzero(1024 * 1024),
            nonzero(1024 * 1024),
            nonzero(100),
            nonzero(1024 * 1024),
            Duration::from_secs(2),
            SqliteJournalMode::Delete,
            SqliteSynchronous::Full,
        )
        .expect("测试 SQLite 配置应合法");
        RusqliteStorage::start(config).expect("测试 SQLite 根应可启动")
    }

    fn project_at(database_path: PathBuf) -> OpenedProject {
        OpenedProject::new(
            "demo".parse::<ProjectName>().expect("项目名应合法"),
            database_path
                .parent()
                .expect("测试数据库必须有父目录")
                .to_path_buf(),
            database_path,
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::rpg_maker::project::test_layout_profile(),
        )
    }

    fn scalar_identity(
        index: usize,
        field: &str,
        source: &str,
        context: &str,
    ) -> TranslationUnitIdentity {
        TranslationUnitIdentity::new(
            RpgMakerStandardAssetOwner::Builtin,
            TextGroupKind::DatabaseEntry,
            data_group(index),
            TextUnitRole::Scalar(ScalarFieldKey::new(field).expect("字段键应合法")),
            value(source),
            context,
        )
    }

    fn dialogue_body_identity(
        index: usize,
        source: &str,
        context: &str,
    ) -> TranslationUnitIdentity {
        TranslationUnitIdentity::new(
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
            TextUnitRole::DialogueBody,
            lines(&[source]),
            context,
        )
    }

    fn value(text: &str) -> TextUnitContent {
        TextUnitContent::Value(text.to_owned())
    }

    fn lines(lines: &[&str]) -> TextUnitContent {
        TextUnitContent::Lines(lines.iter().map(|line| (*line).to_owned()).collect())
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
