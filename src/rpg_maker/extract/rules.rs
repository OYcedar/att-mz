//! 人类可直接书写的 Rules TOML，以及从定义到可逆标准文本快照的完整编排。

mod definition;
mod matcher;

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::str::Utf8Error;

use serde_json::Value;

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, FailureReport, RecoveryFact, ReportedFailure,
    SafeDiagnostic, SafeDiagnosticSource,
};
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::json::StackSafeJsonValue;
use crate::rpg_maker::model::{ProjectionModelError, TextUnitContent};
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::text::{DataFileName, StandardDataFile};

use self::definition::{FileRuleSource, RuleSource, RulesDefinition, RulesDefinitionError};
use self::matcher::{
    MatchedRuleTarget, RulesMatchError, RulesMatchInput, RulesPlugin, RulesSourceMatchWorkUnit,
    build_source_match_plan, finish_source_matches,
};
use super::document::{
    DocumentReadProgress, PluginConfiguration, RpgMakerDocumentId, RpgMakerDocumentSelection,
    RpgMakerProjectDocumentReader, RpgMakerProjectDocumentReadingDiagnostic,
    RpgMakerProjectDocuments,
};
use super::model::{ExtractedTextGroup, ExtractedTextUnit, RulesSnapshot, SnapshotModelError};
use super::store::RulesSnapshotStore;
use super::{ExtractProgress, ExtractProgressPhase};

/// 使用调用方提供的当前 Rules TOML 完整替换 Rules 提取快照。
pub(crate) trait RulesExtraction: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn replace(
        &self,
        project: &OpenedProject,
        program: RulesProgram,
        progress: ExtractProgress,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 已读取、验证并规范编码的 Extract Rules 程序。
///
/// 显式文件和项目状态复用都在进入业务服务前建立该值；复用路径因此不会重新读取
/// 原 TOML 文件。`diagnostic_path` 只用于人类诊断，不参与规则语义。
#[derive(Clone, Debug)]
pub(crate) struct RulesProgram {
    diagnostic_path: PathBuf,
    definition: RulesDefinition,
}

impl RulesProgram {
    pub(crate) fn from_toml(
        diagnostic_path: PathBuf,
        bytes: Vec<u8>,
    ) -> Result<Self, RulesProgramError> {
        let text = String::from_utf8(bytes)
            .map_err(|source| RulesProgramError::InvalidUtf8(source.utf8_error()))?;
        let definition =
            RulesDefinition::parse(&text).map_err(RulesProgramError::InvalidDefinition)?;
        Ok(Self {
            diagnostic_path,
            definition,
        })
    }

    pub(crate) fn from_canonical_json(
        diagnostic_path: PathBuf,
        canonical_json: &str,
    ) -> Result<Self, RulesProgramError> {
        let definition = RulesDefinition::parse_canonical_json(canonical_json)
            .map_err(RulesProgramError::InvalidDefinition)?;
        Ok(Self {
            diagnostic_path,
            definition,
        })
    }

    pub(crate) fn diagnostic_path(&self) -> &std::path::Path {
        &self.diagnostic_path
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.definition.is_empty()
    }

    pub(crate) fn canonical_json(&self) -> &str {
        self.definition.canonical_json()
    }
}

/// Rules 程序在用户输入/项目状态边界无法建立。
#[derive(Debug)]
pub(crate) enum RulesProgramError {
    InvalidUtf8(Utf8Error),
    InvalidDefinition(RulesDefinitionError),
}

impl RulesProgramError {
    pub(crate) fn safe_diagnostic(&self, rules_path: &std::path::Path) -> SafeDiagnostic {
        match self {
            Self::InvalidUtf8(source) => SafeDiagnostic::new(
                DiagnosticCode::ExtractRules,
                DiagnosticStage::CommandPreparation,
                DiagnosticSubject::path(rules_path),
                DiagnosticReason::InvalidUtf8 {
                    valid_up_to: u64::try_from(source.valid_up_to()).unwrap_or(u64::MAX),
                    error_len: source
                        .error_len()
                        .map(|value| u64::try_from(value).unwrap_or(u64::MAX)),
                },
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixInput,
            ),
            Self::InvalidDefinition(source) => SafeDiagnostic::new(
                DiagnosticCode::ExtractRules,
                DiagnosticStage::CommandPreparation,
                DiagnosticSubject::path(rules_path),
                DiagnosticReason::failure_with_detail(
                    DiagnosticFailureKind::RulesDefinitionInvalid,
                    source.safe_detail(),
                ),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::FixInput,
            ),
        }
    }
}

impl fmt::Display for RulesProgramError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUtf8(source) => write!(formatter, "Rules 定义不是 UTF-8：{source}"),
            Self::InvalidDefinition(source) => source.fmt(formatter),
        }
    }
}

impl Error for RulesProgramError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidUtf8(source) => Some(source),
            Self::InvalidDefinition(source) => Some(source),
        }
    }
}

/// 在项目数据库边界确认 canonical Rules 仍满足当前来源、路径与 PCRE2 语义。
pub(crate) fn validate_rules_canonical_json(source: &str) -> Result<(), RulesProgramError> {
    RulesDefinition::parse_canonical_json(source)
        .map(|_| ())
        .map_err(RulesProgramError::InvalidDefinition)
}

/// 读取、解析、匹配并原子提交一次 Rules 快照。
pub(crate) struct RulesExtractionService<D, S, C> {
    document_reader: D,
    snapshot_store: S,
    cpu_executor: C,
}

impl<D, S, C> RulesExtractionService<D, S, C> {
    pub(crate) fn new(document_reader: D, snapshot_store: S, cpu_executor: C) -> Self {
        Self {
            document_reader,
            snapshot_store,
            cpu_executor,
        }
    }
}

impl<D, S, C> RulesExtraction for RulesExtractionService<D, S, C>
where
    D: RpgMakerProjectDocumentReader,
    S: RulesSnapshotStore,
    C: CpuTaskExecutor,
{
    type Error = RulesExtractionError<D::Error, S::Error, C::Error>;

    async fn replace(
        &self,
        project: &OpenedProject,
        program: RulesProgram,
        progress: ExtractProgress,
    ) -> Result<(), Self::Error> {
        let rules_path = program.diagnostic_path().to_path_buf();
        let definition = program.definition;

        if definition.is_empty() {
            progress.indeterminate(ExtractProgressPhase::RulesCommit);
            self.snapshot_store
                .deactivate_rules(project)
                .await
                .map_err(|source| RulesExtractionError::Persist { rules_path, source })?;
            return Ok(());
        }

        let selection = document_selection(&definition);
        let documents = self
            .document_reader
            .read_with_progress(
                project,
                selection,
                DocumentReadProgress::new({
                    let progress = progress.clone();
                    move |completed, total| {
                        progress.determinate(
                            ExtractProgressPhase::RulesDocuments,
                            completed,
                            total,
                        );
                    }
                }),
            )
            .await
            .map_err(|source| RulesExtractionError::ReadDocuments {
                rules_path: rules_path.clone(),
                source,
            })?;
        let (definition, input) = self
            .cpu_executor
            .execute(move || {
                let input = build_match_input(&definition, documents)?;
                Ok::<_, RulesMatchError>((definition, input))
            })
            .await
            .map_err(|source| RulesExtractionError::MatchSourceCompute {
                rules_path: rules_path.clone(),
                source,
            })?
            .map_err(|source| RulesExtractionError::InvalidTarget {
                rules_path: rules_path.clone(),
                source,
            })?;

        let matches = self
            .match_rules_parallel(definition, input, progress.clone())
            .await
            .map_err(|error| match error {
                ParallelRulesBuildError::MatchCompute(source) => {
                    RulesExtractionError::MatchSourceCompute {
                        rules_path: rules_path.clone(),
                        source,
                    }
                }
                ParallelRulesBuildError::FinalizeCompute(source) => {
                    RulesExtractionError::BuildSnapshotCompute {
                        rules_path: rules_path.clone(),
                        source,
                    }
                }
                ParallelRulesBuildError::Match(source) => RulesExtractionError::InvalidTarget {
                    rules_path: rules_path.clone(),
                    source,
                },
                ParallelRulesBuildError::Snapshot(source) => {
                    RulesExtractionError::InvalidSnapshot {
                        rules_path: rules_path.clone(),
                        source,
                    }
                }
            })?;

        progress.indeterminate(ExtractProgressPhase::RulesCommit);
        self.snapshot_store
            .replace_rules(project, matches)
            .await
            .map_err(|source| RulesExtractionError::Persist { rules_path, source })
    }
}

impl<D, S, C> RulesExtractionService<D, S, C>
where
    C: CpuTaskExecutor,
{
    async fn match_rules_parallel(
        &self,
        definition: RulesDefinition,
        input: RulesMatchInput,
        progress: ExtractProgress,
    ) -> Result<RulesSnapshot, ParallelRulesBuildError<C::Error>> {
        let plan = build_source_match_plan(definition.into_rules(), input);
        let (rule_count, work_units) = plan.into_parts();
        let total = u64::try_from(work_units.len()).expect("Rules 来源工作单元数必须能用 u64 表达");
        progress.determinate(ExtractProgressPhase::RulesMatches, 0, total);
        let completed = self
            .cpu_executor
            .execute_ordered_map_observed(work_units, RulesSourceMatchWorkUnit::run, {
                let progress = progress.clone();
                move |completed| {
                    progress.determinate(ExtractProgressPhase::RulesMatches, completed, total);
                }
            })
            .await
            .map_err(ParallelRulesBuildError::MatchCompute)?;

        self.cpu_executor
            .execute(move || {
                let targets = finish_source_matches(rule_count, completed)
                    .map_err(ParallelRulesBuildError::Match)?;
                snapshot_from_targets(targets).map_err(ParallelRulesBuildError::Snapshot)
            })
            .await
            .map_err(ParallelRulesBuildError::FinalizeCompute)?
    }
}

fn document_selection(definition: &RulesDefinition) -> RpgMakerDocumentSelection {
    let mut selection = RpgMakerDocumentSelection::empty();
    for rule in definition.rules() {
        match rule.source() {
            RuleSource::File(FileRuleSource::AllMaps) => selection.request_all_maps(),
            RuleSource::File(FileRuleSource::Exact(file)) => {
                select_exact_data_file(&mut selection, file)
            }
            RuleSource::Plugin(_) => selection.request_plugins(),
            RuleSource::Command { .. } => {
                selection.insert_standard_file(StandardDataFile::CommonEvents);
                selection.insert_standard_file(StandardDataFile::Troops);
                selection.request_all_maps();
            }
        }
    }
    selection
}

fn select_exact_data_file(selection: &mut RpgMakerDocumentSelection, file: &str) {
    let file =
        DataFileName::parse(file.to_owned()).expect("Rules 定义解析已经校验安全的精确 JSON 基名");
    selection.insert_data_file(file);
}

fn build_match_input(
    definition: &RulesDefinition,
    documents: RpgMakerProjectDocuments,
) -> Result<RulesMatchInput, RulesMatchError> {
    let plugin_rules = definition
        .rules()
        .iter()
        .filter_map(|rule| match rule.source() {
            RuleSource::Plugin(name) => Some((name.clone(), rule.rule_number())),
            RuleSource::File(_) | RuleSource::Command { .. } => None,
        })
        .fold(
            BTreeMap::<String, usize>::new(),
            |mut rules, (name, number)| {
                rules.entry(name).or_insert(number);
                rules
            },
        );
    let (documents, plugins, _plugins_prefix) = documents.into_shared_parts();
    let files = documents
        .into_iter()
        .map(|(id, value)| (document_file_name(id), value))
        .collect::<Vec<_>>();
    let plugins = plugins
        .into_iter()
        .map(|plugin| plugin_for_rules(plugin, &plugin_rules))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();
    Ok(RulesMatchInput::from_shared(files, plugins))
}

fn document_file_name(id: RpgMakerDocumentId) -> String {
    match id {
        RpgMakerDocumentId::Data(file) => file.file_name().to_owned(),
        RpgMakerDocumentId::DataFile(file) => file.as_str().to_owned(),
        RpgMakerDocumentId::Map(map_id) => map_id.file_name(),
    }
}

fn plugin_for_rules(
    plugin: PluginConfiguration,
    rules: &BTreeMap<String, usize>,
) -> Result<Option<RulesPlugin>, RulesMatchError> {
    let (index, mut fields) = plugin.into_parts();
    let Some(name) = fields
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Ok(None);
    };
    let Some(rule_number) = rules.get(&name).copied() else {
        return Ok(None);
    };
    let status = fields.get("status");
    let enabled = fields
        .get("status")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            RulesMatchError::invalid_plugin_field(
                rule_number,
                index,
                name.clone(),
                "status",
                "boolean",
                status,
            )
        })?;
    let parameters = if enabled {
        if !fields.get("parameters").is_some_and(Value::is_object) {
            return Err(RulesMatchError::invalid_plugin_field(
                rule_number,
                index,
                name.clone(),
                "parameters",
                "object",
                fields.get("parameters"),
            ));
        }
        let parameters = fields
            .as_object_mut()
            .and_then(|fields| fields.remove("parameters"))
            .expect("已经确认插件 parameters 是对象字段");
        StackSafeJsonValue::new(parameters)
    } else {
        StackSafeJsonValue::new(Value::Object(Default::default()))
    };
    Ok(Some(RulesPlugin::from_stack_safe(
        index, name, enabled, parameters,
    )))
}

fn snapshot_from_targets(
    targets: Vec<MatchedRuleTarget>,
) -> Result<RulesSnapshot, SnapshotModelError> {
    let mut groups = Vec::with_capacity(targets.len());
    for target in targets {
        let physical_location = target
            .physical_location()
            .expect("匹配器只会产生已通过 Rules 定义校验的来源");
        let units = target
            .units()
            .iter()
            .enumerate()
            .map(|(unit_index, unit)| {
                ExtractedTextUnit::projected(
                    target.role_for(unit_index),
                    physical_location.clone(),
                    TextUnitContent::Value(unit.source_text().to_owned()),
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let recipe = target
            .projection_recipe()
            .expect("匹配器已经校验物化配方可以逐字重建最终字符串");
        groups.push(ExtractedTextGroup::projected(
            target.kind(),
            target
                .group_location()
                .expect("匹配器只会产生已通过 Rules 定义校验的来源"),
            units,
            vec![recipe],
        )?);
    }
    RulesSnapshot::new(groups)
}

enum ParallelRulesBuildError<CE> {
    MatchCompute(CpuTaskExecutionError<CE>),
    FinalizeCompute(CpuTaskExecutionError<CE>),
    Match(RulesMatchError),
    Snapshot(SnapshotModelError),
}

/// Rules 提取在自身职责边界产生的阶段错误。
#[derive(Debug)]
pub(crate) enum RulesExtractionError<DE, SE, CE> {
    ReadDocuments {
        rules_path: PathBuf,
        source: DE,
    },
    InvalidTarget {
        rules_path: PathBuf,
        source: RulesMatchError,
    },
    InvalidSnapshot {
        rules_path: PathBuf,
        source: SnapshotModelError,
    },
    MatchSourceCompute {
        rules_path: PathBuf,
        source: CpuTaskExecutionError<CE>,
    },
    BuildSnapshotCompute {
        rules_path: PathBuf,
        source: CpuTaskExecutionError<CE>,
    },
    Persist {
        rules_path: PathBuf,
        source: SE,
    },
}

impl<DE, SE, CE> RulesExtractionError<DE, SE, CE>
where
    DE: Error + RpgMakerProjectDocumentReadingDiagnostic + Send + Sync + 'static,
    SE: Error + SafeDiagnosticSource + Send + Sync + 'static,
    CE: Error + Send + Sync + 'static,
    CpuTaskExecutionError<CE>: SafeDiagnosticSource,
{
    /// 在直接依赖仍持有文件路径、OS/SQLite code 与 CPU 终态时建立具体安全投影。
    ///
    /// 借用型映射与拥有型 `FailureReport` 共用这份闭集投影；拥有型路径另行保留具体
    /// source 以及 Store 已经拆出的相关错误。
    pub(crate) fn safe_diagnostic(&self) -> SafeDiagnostic {
        match self {
            Self::ReadDocuments { rules_path, source } => with_rules_context(
                source.safe_document_reading_diagnostic(
                    DiagnosticCode::ExtractRules,
                    DiagnosticStage::Extract,
                ),
                rules_path,
                "read_documents",
            ),
            Self::InvalidTarget { rules_path, source } => source.safe_diagnostic(rules_path),
            Self::InvalidSnapshot { rules_path, source } => {
                snapshot_model_diagnostic(source, rules_path)
            }
            Self::MatchSourceCompute { rules_path, source } => with_rules_context(
                source.safe_diagnostic_source(
                    DiagnosticStage::Extract,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::Retry,
                ),
                rules_path,
                "match_source",
            ),
            Self::BuildSnapshotCompute { rules_path, source } => with_rules_context(
                source.safe_diagnostic_source(
                    DiagnosticStage::Extract,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::Retry,
                ),
                rules_path,
                "build_snapshot",
            ),
            Self::Persist { rules_path, source } => with_rules_context(
                source.safe_diagnostic_source(
                    DiagnosticStage::Extract,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckProjectState,
                ),
                rules_path,
                "persist_snapshot",
            )
            .with_recovery(RecoveryFact::component("owner=rules")),
        }
    }

    /// 消费完整 Rules 阶段错误，保留具体主类型以及 Store 已经拆出的相关错误。
    pub(crate) fn into_failure_report(self) -> FailureReport {
        let diagnostic = self.safe_diagnostic();
        match self {
            Self::ReadDocuments { source, .. } => {
                FailureReport::new(ReportedFailure::new(diagnostic, source))
            }
            Self::InvalidTarget { source, .. } => {
                FailureReport::new(ReportedFailure::new(diagnostic, source))
            }
            Self::InvalidSnapshot { source, .. } => {
                FailureReport::new(ReportedFailure::new(diagnostic, source))
            }
            Self::MatchSourceCompute { rules_path, source } => source
                .into_failure_report(
                    DiagnosticStage::Extract,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::Retry,
                )
                .with_primary_recovery(RecoveryFact::path(rules_path))
                .with_primary_recovery(RecoveryFact::component("rules_operation=match_source")),
            Self::BuildSnapshotCompute { rules_path, source } => source
                .into_failure_report(
                    DiagnosticStage::Extract,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::Retry,
                )
                .with_primary_recovery(RecoveryFact::path(rules_path))
                .with_primary_recovery(RecoveryFact::component("rules_operation=build_snapshot")),
            Self::Persist { rules_path, source } => source
                .into_failure_report(
                    DiagnosticStage::Extract,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckProjectState,
                )
                .with_primary_recovery(RecoveryFact::path(rules_path))
                .with_primary_recovery(RecoveryFact::component("rules_operation=persist_snapshot"))
                .with_primary_recovery(RecoveryFact::component("owner=rules")),
        }
    }
}

fn with_rules_context(
    diagnostic: SafeDiagnostic,
    rules_path: &std::path::Path,
    operation: &'static str,
) -> SafeDiagnostic {
    diagnostic
        .with_recovery(RecoveryFact::path(rules_path))
        .with_recovery(RecoveryFact::component(format!(
            "rules_operation={operation}"
        )))
}

fn snapshot_model_diagnostic(
    source: &SnapshotModelError,
    rules_path: &std::path::Path,
) -> SafeDiagnostic {
    SafeDiagnostic::new(
        DiagnosticCode::ExtractRules,
        DiagnosticStage::Extract,
        DiagnosticSubject::path(rules_path),
        DiagnosticReason::failure_with_detail(
            DiagnosticFailureKind::RulesSnapshotInvalid,
            snapshot_model_fact(source),
        ),
        DiagnosticImpact::Unchanged,
        DiagnosticAction::ReportBug,
    )
}

fn snapshot_model_fact(source: &SnapshotModelError) -> String {
    let variant = match source {
        SnapshotModelError::BlankSourceContent { .. } => "blank_source_content",
        SnapshotModelError::ContentShapeMismatch { .. } => "content_shape_mismatch",
        SnapshotModelError::DirectGroupRequiresValue { .. } => "direct_group_requires_value",
        SnapshotModelError::InvalidSourceLine { .. } => "invalid_source_line",
        SnapshotModelError::EmptyGroup { .. } => "empty_group",
        SnapshotModelError::EmptyProjection { .. } => "empty_projection",
        SnapshotModelError::DuplicateLogicalLocation { .. } => "duplicate_logical_location",
        SnapshotModelError::ConflictingGroupKind { .. } => "conflicting_group_kind",
        SnapshotModelError::MutationClaimConflict { .. } => "mutation_claim_conflict",
        SnapshotModelError::RecipeRoleMismatch { .. } => "recipe_role_mismatch",
        SnapshotModelError::RecipeLineMismatch { .. } => "recipe_line_mismatch",
        SnapshotModelError::Projection(source) => projection_model_variant(source),
    };
    format!("snapshot_error={variant}")
}

fn projection_model_variant(source: &ProjectionModelError) -> &'static str {
    match source {
        ProjectionModelError::EmptyScalarFieldKey => "projection.empty_scalar_field_key",
        ProjectionModelError::EventBlockCoverageRequired => {
            "projection.event_block_coverage_required"
        }
        ProjectionModelError::InvalidEventBlockCoverage => {
            "projection.invalid_event_block_coverage"
        }
        ProjectionModelError::MutationClaimTargetMismatch => {
            "projection.mutation_claim_target_mismatch"
        }
        ProjectionModelError::RecipeHasNoTextSlot => "projection.recipe_has_no_text_slot",
        ProjectionModelError::DuplicateProjectionSlot { .. } => {
            "projection.duplicate_projection_slot"
        }
        ProjectionModelError::MultipleBodyLinesInPhysicalLine => {
            "projection.multiple_body_lines_in_physical_line"
        }
        ProjectionModelError::DuplicateDialogueBodyLine { .. } => {
            "projection.duplicate_dialogue_body_line"
        }
        ProjectionModelError::NonContiguousDialogueBodyLines { .. } => {
            "projection.non_contiguous_dialogue_body_lines"
        }
        ProjectionModelError::MixedDirectAndInlineSpeaker => {
            "projection.mixed_direct_and_inline_speaker"
        }
    }
}

impl<DE, SE, CE> fmt::Display for RulesExtractionError<DE, SE, CE>
where
    DE: Error,
    SE: Error,
    CE: Error,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadDocuments { rules_path, source } => write!(
                formatter,
                "读取 Rules 所需 RPG Maker 文档失败 {}：{source}",
                rules_path.display()
            ),
            Self::InvalidTarget { rules_path, source } => write!(
                formatter,
                "Rules 匹配失败 {}：{source}",
                rules_path.display()
            ),
            Self::InvalidSnapshot { rules_path, source } => write!(
                formatter,
                "Rules 快照无效 {}：{source}",
                rules_path.display()
            ),
            Self::MatchSourceCompute { rules_path, source } => write!(
                formatter,
                "调度 Rules 来源匹配失败 {}：{source}",
                rules_path.display()
            ),
            Self::BuildSnapshotCompute { rules_path, source } => write!(
                formatter,
                "调度 Rules 快照汇总失败 {}：{source}",
                rules_path.display()
            ),
            Self::Persist { rules_path, source } => {
                write!(
                    formatter,
                    "保存 Rules 快照失败 {}：{source}",
                    rules_path.display()
                )
            }
        }
    }
}

impl<DE, SE, CE> Error for RulesExtractionError<DE, SE, CE>
where
    DE: Error + 'static,
    SE: Error + 'static,
    CE: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MatchSourceCompute { source, .. } | Self::BuildSnapshotCompute { source, .. } => {
                Some(source)
            }
            Self::ReadDocuments { source, .. } => Some(source),
            Self::InvalidTarget { source, .. } => Some(source),
            Self::InvalidSnapshot { source, .. } => Some(source),
            Self::Persist { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::fs::File;
    use std::io::{BufWriter, Write as _};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use serde_json::{Map, json};

    use super::*;
    use crate::execution::cpu::CpuTaskExecutionError;
    use crate::progress::{ProgressObserver, ProgressSnapshot};
    use crate::rpg_maker::ProjectName;
    use crate::rpg_maker::extract::document::RpgMakerProjectDocumentReadingError;
    use crate::rpg_maker::model::{
        DirectTextPart, DirectTextRecipe, TextProjectionRecipe, TextUnitRole,
    };
    use crate::rpg_maker::text::MapId;
    use crate::runtime::filesystem::{SystemFileSystem, SystemFileSystemConfig};
    use crate::storage::file_system::FileReader;

    #[tokio::test]
    async fn thousands_of_semantic_rules_larger_than_seventeen_mibibytes_cross_production_read_and_state_round_trip()
     {
        const RULE_COUNT: usize = 4_096;
        const PATH_SEGMENTS_PER_RULE: usize = 112;

        let directory = tempfile::tempdir().expect("应建立大 Rules 临时目录");
        let path = directory.path().join("large-semantic-rules.toml");
        let file = File::create(&path).expect("应建立大 Rules 文件");
        let mut writer = BufWriter::new(file);
        for index in 0..RULE_COUNT {
            let mut semantic_path = String::with_capacity(PATH_SEGMENTS_PER_RULE * 56);
            for segment in 0..PATH_SEGMENTS_PER_RULE {
                if segment != 0 {
                    semantic_path.push('.');
                }
                write!(
                    semantic_path,
                    "node_{index:04}_{segment:02}_localized_text_payload_field"
                )
                .expect("写入 String 不会失败");
            }
            writeln!(
                writer,
                "[[rule]]\nplugin = \"plugin-{index:04}\"\npath = '{semantic_path}'\ndecode_json = true"
            )
            .expect("应写入语义 Rules");
        }
        writer.flush().expect("应刷新大 Rules 文件");
        let file = writer.into_inner().expect("应取回大 Rules 文件");
        file.sync_all().expect("应完整落盘大 Rules 文件");
        assert!(
            file.metadata().expect("应读取 Rules 元数据").len() > 17 * 1024 * 1024,
            "测试输入必须真实超过 17 MiB"
        );

        let file_system = SystemFileSystem::new(SystemFileSystemConfig::production())
            .expect("生产文件系统应启动");
        let source = file_system
            .read_file(path)
            .await
            .expect("17 MiB 以上语义 Rules 应通过生产文件读取");
        let diagnostic_path = source.resolved_path().to_path_buf();
        let program = RulesProgram::from_toml(diagnostic_path.clone(), source.into_bytes())
            .expect("全部规则应通过生产 TOML、来源、路径和 canonical 编译边界");

        assert_eq!(program.definition.rules().len(), RULE_COUNT);
        assert_eq!(
            program
                .definition
                .rules()
                .iter()
                .map(|rule| rule.path().expect("Plugin 规则必须有路径").segments().len())
                .sum::<usize>(),
            RULE_COUNT * PATH_SEGMENTS_PER_RULE
        );
        assert!(program.canonical_json().len() > 17 * 1024 * 1024);
        let last = program
            .definition
            .rules()
            .last()
            .expect("大 Rules 必须包含最后一条规则");
        assert_eq!(last.rule_number(), RULE_COUNT);
        let last_path = last.path().expect("Plugin 规则必须有路径");
        assert_eq!(last_path.segments().len(), PATH_SEGMENTS_PER_RULE);
        assert_eq!(
            last_path.segments().first(),
            Some(&definition::PathSegment::Key(
                "node_4095_00_localized_text_payload_field".to_owned()
            ))
        );
        assert_eq!(
            last_path.segments().last(),
            Some(&definition::PathSegment::Key(
                "node_4095_111_localized_text_payload_field".to_owned()
            ))
        );
        assert!(last.decode_json());

        let canonical_snapshot = program.canonical_json().to_owned();
        drop(program);
        let restored = RulesProgram::from_canonical_json(diagnostic_path, &canonical_snapshot)
            .expect("17 MiB 以上 canonical Rules 应能从项目状态重建");
        assert_eq!(restored.definition.rules().len(), RULE_COUNT);

        file_system.shutdown().await.expect("文件系统应关闭");
    }

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

    #[test]
    fn selection_requests_only_declared_sources_and_builtin_event_documents() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Disciplines.json"
path = '[].Name'

[[rule]]
file = "Map042.json"
path = 'displayName'

[[rule]]
file = "Map000.json"
path = 'displayName'

[[rule]]
plugin = "Quest"
path = 'Title'

[[rule]]
code = 356
parameter = 0
pattern = '\A(?<text>.+)\z'
"#,
        )
        .expect("规则应合法");

        let selection = document_selection(&definition);

        assert!(selection.includes_all_maps());
        assert!(selection.includes_plugins());
        assert!(
            selection
                .standard_files()
                .contains(&StandardDataFile::CommonEvents)
        );
        assert!(
            selection
                .standard_files()
                .contains(&StandardDataFile::Troops)
        );
        assert!(selection.map_ids().contains(&MapId::new(42).unwrap()));
        assert!(
            selection
                .data_files()
                .iter()
                .any(|file| file.as_str() == "Disciplines.json")
        );
        assert!(
            selection
                .data_files()
                .iter()
                .any(|file| file.as_str() == "Map000.json")
        );
    }

    #[test]
    fn rules_targets_follow_canonical_cross_source_order() {
        assert_eq!(
            StandardDataFile::ALL.map(StandardDataFile::file_name),
            [
                "Actors.json",
                "Animations.json",
                "Armors.json",
                "Classes.json",
                "CommonEvents.json",
                "Enemies.json",
                "Items.json",
                "MapInfos.json",
                "Skills.json",
                "States.json",
                "System.json",
                "Tilesets.json",
                "Troops.json",
                "Weapons.json",
            ],
            "标准 DataFile 的固定顺序是 Rules canonical 来源顺序的一部分"
        );
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
plugin = "Quest"
path = 'alpha'

[[rule]]
file = "Map010.json"
path = 'displayName'

[[rule]]
file = "Zulu.json"
path = 'text'

[[rule]]
file = "Items.json"
path = '[].name'

[[rule]]
plugin = "Earlier"
path = 'only'

[[rule]]
file = "Map002.json"
path = 'displayName'

[[rule]]
file = "AlphaCustom.json"
path = 'text'

[[rule]]
file = "Actors.json"
path = '[].name'

[[rule]]
plugin = "Quest"
path = 'zeta'
"#,
        )
        .expect("跨来源排序规则应合法");

        let mut quest_parameters = Map::new();
        quest_parameters.insert("zeta".to_owned(), json!("插件后声明字母"));
        quest_parameters.insert("alpha".to_owned(), json!("插件先声明字母"));
        let mut quest = Map::new();
        quest.insert("name".to_owned(), json!("Quest"));
        quest.insert("status".to_owned(), json!(true));
        quest.insert("parameters".to_owned(), Value::Object(quest_parameters));

        let mut earlier = Map::new();
        earlier.insert("name".to_owned(), json!("Earlier"));
        earlier.insert("status".to_owned(), json!(true));
        earlier.insert("parameters".to_owned(), json!({"only":"较早插件"}));

        let documents = RpgMakerProjectDocuments::new(
            BTreeMap::from([
                (
                    RpgMakerDocumentId::Map(MapId::new(10).unwrap()),
                    json!({"displayName":"地图十"}),
                ),
                (
                    RpgMakerDocumentId::DataFile(
                        DataFileName::parse("Zulu.json").expect("测试文件名应合法"),
                    ),
                    json!({"text":"自定义后"}),
                ),
                (
                    RpgMakerDocumentId::Data(StandardDataFile::Items),
                    json!([null, {"name":"物品"}]),
                ),
                (
                    RpgMakerDocumentId::DataFile(
                        DataFileName::parse("AlphaCustom.json").expect("测试文件名应合法"),
                    ),
                    json!({"text":"自定义前"}),
                ),
                (
                    RpgMakerDocumentId::Map(MapId::new(2).unwrap()),
                    json!({"displayName":"地图二"}),
                ),
                (
                    RpgMakerDocumentId::Data(StandardDataFile::Actors),
                    json!([null, {"name":"角色"}]),
                ),
            ]),
            vec![
                PluginConfiguration::new(7, quest),
                PluginConfiguration::new(2, earlier),
            ],
        );
        let input = build_match_input(&definition, documents).expect("冻结来源应建立匹配输入");

        let targets = matcher::match_rules(&definition, &input).expect("全部规则都应命中");
        assert_eq!(
            targets
                .iter()
                .flat_map(MatchedRuleTarget::units)
                .map(|unit| unit.source_text())
                .collect::<Vec<_>>(),
            [
                "角色",
                "物品",
                "自定义前",
                "自定义后",
                "地图二",
                "地图十",
                "较早插件",
                "插件后声明字母",
                "插件先声明字母",
            ],
            "规则编号、调用方插入顺序和 OS 枚举都不得改变 canonical 来源顺序"
        );
    }

    #[test]
    fn documented_extract_rules_match_frozen_sources_and_round_trip_materialized_recipes() {
        const EXAMPLE: &str = include_str!("../../../docs/rpg-maker/examples/extract-rules.toml");

        fn rebuild(recipe: &DirectTextRecipe, values: &BTreeMap<TextUnitRole, String>) -> String {
            let mut output = String::new();
            for part in recipe.parts() {
                match part {
                    DirectTextPart::Literal(literal) => output.push_str(literal),
                    DirectTextPart::TextSlot { role } => output.push_str(
                        values
                            .get(role)
                            .expect("文档示例每个 recipe slot 都应有对应单元"),
                    ),
                    DirectTextPart::LineSlot { .. } => {
                        panic!("Extract Rules 的 Scalar recipe 不应产生 LineSlot")
                    }
                }
            }
            output
        }

        let definition = RulesDefinition::parse(EXAMPLE)
            .expect("完整 Extract Rules 示例必须通过生产解析与 PCRE2 编译边界");

        let plugin_entry = json!({"title":"插件标题"}).to_string();
        let encoded_plugin_entries = json!([plugin_entry]).to_string();
        let mut plugin_parameters = Map::new();
        plugin_parameters.insert("entries".to_owned(), Value::String(encoded_plugin_entries));
        let mut plugin = Map::new();
        plugin.insert("name".to_owned(), json!("QuestWindow"));
        plugin.insert("status".to_owned(), json!(true));
        plugin.insert("parameters".to_owned(), Value::Object(plugin_parameters));

        let encoded_final_title = serde_json::to_string("终点标题").unwrap();
        let encoded_title_object = json!({"title":encoded_final_title}).to_string();
        let encoded_payload_object = json!({"payload":encoded_title_object}).to_string();
        let encoded_empty_key_root = json!({"":encoded_payload_object}).to_string();
        let documents = RpgMakerProjectDocuments::new(
            BTreeMap::from([
                (
                    RpgMakerDocumentId::Data(StandardDataFile::CommonEvents),
                    json!([
                        null,
                        {
                            "list": [
                                {"code":356,"parameters":["DisplayNotice 出航命令"]},
                                {"code":357,"parameters":["QuestWindow","Show","",encoded_empty_key_root]}
                            ]
                        }
                    ]),
                ),
                (
                    RpgMakerDocumentId::DataFile(
                        DataFileName::parse("QuestEntries.json").expect("示例自定义文件名应合法"),
                    ),
                    json!([{"title":"委托标题"}]),
                ),
            ]),
            vec![PluginConfiguration::new(4, plugin)],
        );
        let input = build_match_input(&definition, documents).expect("冻结来源应建立匹配输入");
        let targets = matcher::match_rules(&definition, &input)
            .expect("完整示例的四条规则都应命中代表性冻结来源");

        assert_eq!(targets.len(), 4);
        assert!(targets.iter().all(|target| target.units().len() == 1));
        assert_eq!(
            targets
                .iter()
                .map(|target| target.units()[0].source_text())
                .collect::<Vec<_>>(),
            ["出航命令", "终点标题", "委托标题", "插件标题"],
            "插件与 357 来源必须经过生产路径的逐层 JSON 解码"
        );

        let mut original_round_trips = Vec::new();
        let mut translated_round_trips = Vec::new();
        for (target_index, target) in targets.iter().enumerate() {
            let TextProjectionRecipe::Direct(recipe) = target
                .projection_recipe()
                .expect("匹配目标必须物化为 Direct recipe")
            else {
                panic!("Extract Rules 只应物化 Direct recipe")
            };
            let originals = target
                .units()
                .iter()
                .enumerate()
                .map(|(unit_index, unit)| {
                    (target.role_for(unit_index), unit.source_text().to_owned())
                })
                .collect::<BTreeMap<_, _>>();
            let translations = target
                .units()
                .iter()
                .enumerate()
                .map(|(unit_index, _)| {
                    (
                        target.role_for(unit_index),
                        format!("译文{}", target_index + 1),
                    )
                })
                .collect::<BTreeMap<_, _>>();

            let original = rebuild(&recipe, &originals);
            assert_eq!(
                original,
                recipe.expected_raw(),
                "未翻译 recipe 必须逐字 round-trip"
            );
            original_round_trips.push(original);
            translated_round_trips.push(rebuild(&recipe, &translations));
        }

        assert_eq!(
            original_round_trips,
            ["DisplayNotice 出航命令", "终点标题", "委托标题", "插件标题",]
        );
        assert_eq!(
            translated_round_trips,
            ["DisplayNotice 译文1", "译文2", "译文3", "译文4",],
            "翻译后 recipe 必须只替换槽位并精确保留冻结外壳"
        );
    }

    #[test]
    fn matched_regex_slots_become_one_direct_recipe_and_multiple_logical_units() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Items.json"
path = '[].note'
pattern = '<x>(?<text>.*?)</x>'
"#,
        )
        .expect("规则应合法");
        let input = RulesMatchInput::new(
            BTreeMap::from([(
                "Items.json".to_owned(),
                json!([null, {"note":"<x>甲</x><x>乙</x>"}]),
            )]),
            Vec::new(),
        );
        let targets = matcher::match_rules(&definition, &input).expect("规则应命中");

        let snapshot = snapshot_from_targets(targets).expect("投影应形成快照");

        let groups = snapshot.groups();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].units().len(), 2);
        assert_eq!(groups[0].mutation_claims().claims().len(), 1);
        let TextProjectionRecipe::Direct(recipe) = &groups[0].recipes()[0] else {
            panic!("Rules 局部文本必须生成直接配方")
        };
        assert_eq!(
            recipe
                .parts()
                .iter()
                .filter(|part| matches!(part, DirectTextPart::TextSlot { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn rules_for_fields_of_one_array_entry_form_one_logical_group() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Items.json"
path = '[].name'

[[rule]]
file = "Items.json"
path = '[].description'
"#,
        )
        .expect("规则应合法");
        let input = RulesMatchInput::new(
            BTreeMap::from([(
                "Items.json".to_owned(),
                json!([null, {"name":"药草", "description":"恢复少量生命"}]),
            )]),
            Vec::new(),
        );
        let targets = matcher::match_rules(&definition, &input).expect("两条规则都应命中");

        let snapshot = snapshot_from_targets(targets).expect("同一数据库条目应合并为复合文本组");

        assert_eq!(snapshot.groups().len(), 1);
        assert_eq!(snapshot.groups()[0].units().len(), 2);
        assert_eq!(snapshot.groups()[0].mutation_claims().claims().len(), 2);
        assert_eq!(snapshot.groups()[0].recipes().len(), 2);
    }

    #[test]
    fn terminal_parent_prevents_unrelated_nested_objects_from_sharing_a_group() {
        let definition = RulesDefinition::parse(
            r#"
[[rule]]
file = "Custom.json"
path = 'menu.title'

[[rule]]
file = "Custom.json"
path = 'quest.body'

[[rule]]
file = "Custom.json"
path = 'entries[0].left.Name'

[[rule]]
file = "Custom.json"
path = 'entries[0].right.Name'
"#,
        )
        .expect("规则应合法");
        let input = RulesMatchInput::new(
            BTreeMap::from([(
                "Custom.json".to_owned(),
                json!({
                    "menu": {"title": "菜单"},
                    "quest": {"body": "任务正文"},
                    "entries": [{
                        "left": {"Name": "左"},
                        "right": {"Name": "右"}
                    }]
                }),
            )]),
            Vec::new(),
        );
        let targets = matcher::match_rules(&definition, &input).expect("四条规则都应命中");

        let snapshot = snapshot_from_targets(targets).expect("终点父容器应形成稳定组边界");

        assert_eq!(snapshot.groups().len(), 4);
        assert!(
            snapshot
                .groups()
                .iter()
                .all(|group| group.units().len() == 1)
        );
    }

    #[test]
    fn zero_byte_and_comment_only_are_invalid_but_explicit_empty_deactivates() {
        for bytes in [Vec::new(), b"# comment only\n".to_vec()] {
            assert!(matches!(
                RulesProgram::from_toml(PathBuf::from("rules.toml"), bytes),
                Err(RulesProgramError::InvalidDefinition(_))
            ));
        }
        assert!(
            RulesProgram::from_toml(PathBuf::from("rules.toml"), b"rule = []".to_vec(),)
                .expect("显式空集合应合法")
                .is_empty()
        );
    }

    #[test]
    fn rules_definition_diagnostics_keep_typed_locations_without_rule_payloads() {
        fn toml_diagnostic(source: &str) -> String {
            let path = PathBuf::from("rules/safe-main.toml");
            let error = RulesProgram::from_toml(path.clone(), source.as_bytes().to_vec())
                .expect_err("样本必须被 Rules 输入边界拒绝");
            serde_json::to_string(&error.safe_diagnostic(&path)).expect("诊断应可序列化")
        }

        let invalid_toml =
            toml_diagnostic("[[rule]]\ncode = 'TOML_VALUE_SENTINEL'\nparameter = 0\n");
        assert!(invalid_toml.contains("format=toml"));
        assert!(invalid_toml.contains("byte_start="));
        assert!(!invalid_toml.contains("TOML_VALUE_SENTINEL"));

        let invalid_path = toml_diagnostic(
            "[[rule]]\nfile = 'Actors.json'\npath = 'PATH_PAYLOAD_SENTINEL..name'\n",
        );
        assert!(invalid_path.contains("rule=1"));
        assert!(invalid_path.contains("target=path"));
        assert!(invalid_path.contains("path_error=unexpected_dot"));
        assert!(invalid_path.contains("byte_offset="));
        assert!(!invalid_path.contains("PATH_PAYLOAD_SENTINEL"));

        let invalid_pattern = toml_diagnostic(
            "[[rule]]\ncode = 401\nparameter = 0\npattern = '(?<text>PATTERN_PAYLOAD_SENTINEL'\n",
        );
        assert!(invalid_pattern.contains("rule=1"));
        assert!(invalid_pattern.contains("pcre2_kind=compile"));
        assert!(invalid_pattern.contains("pcre2_code="));
        assert!(invalid_pattern.contains("pcre2_offset="));
        assert!(!invalid_pattern.contains("PATTERN_PAYLOAD_SENTINEL"));

        let invalid_capture = toml_diagnostic(
            "[[rule]]\ncode = 401\nparameter = 0\npattern = '(?<CAPTURE_NAME_SENTINEL>.+)'\n",
        );
        assert!(invalid_capture.contains("actual_count=1"));
        assert!(!invalid_capture.contains("CAPTURE_NAME_SENTINEL"));

        let path = PathBuf::from("rules/safe-main.toml");
        let canonical_error = RulesProgram::from_canonical_json(
            path.clone(),
            r#"[{"code":"CANONICAL_VALUE_SENTINEL","parameter":0}]"#,
        )
        .expect_err("错误类型的 canonical 字段必须失败");
        let canonical = serde_json::to_string(&canonical_error.safe_diagnostic(&path))
            .expect("canonical 诊断应可序列化");
        assert!(canonical.contains("format=canonical_json"));
        assert!(canonical.contains("json_category=data"));
        assert!(canonical.contains("json_line=1"));
        assert!(canonical.contains("json_column="));
        assert!(!canonical.contains("CANONICAL_VALUE_SENTINEL"));
    }

    #[tokio::test]
    async fn failed_candidate_never_replaces_or_deactivates_the_previous_snapshot() {
        let state = Arc::new(StoreState::default());
        let (service, program) = test_service(
            br#"
[[rule]]
file = "Items.json"
path = '[].name'
"#
            .to_vec(),
            RpgMakerProjectDocuments::new(
                BTreeMap::from([(
                    RpgMakerDocumentId::Data(StandardDataFile::Items),
                    json!([null, {"name":"   "}]),
                )]),
                Vec::new(),
            ),
            Arc::clone(&state),
        );
        let progress = RecordingProgress::default();

        let error = service
            .replace(&project(), program, ExtractProgress::new(progress.clone()))
            .await
            .expect_err("零个非空语义单元必须放弃整个替换");

        assert!(matches!(
            error,
            RulesExtractionError::InvalidTarget {
                source: RulesMatchError::NoNonBlankMatch { rule_number: 1 },
                ..
            }
        ));
        assert_eq!(state.replacements.load(Ordering::SeqCst), 0);
        assert_eq!(state.deactivations.load(Ordering::SeqCst), 0);
        assert_eq!(
            progress.snapshots(),
            [
                ProgressSnapshot::determinate(ExtractProgressPhase::RulesMatches, 0, 1),
                ProgressSnapshot::determinate(ExtractProgressPhase::RulesMatches, 1, 1),
            ]
        );
    }

    #[tokio::test]
    async fn explicit_empty_deactivates_without_reading_project_documents() {
        let state = Arc::new(StoreState::default());
        let document_reads = Arc::new(AtomicUsize::new(0));
        let service = RulesExtractionService::new(
            FakeDocumentReader {
                documents: RpgMakerProjectDocuments::empty(),
                reads: Arc::clone(&document_reads),
            },
            FakeStore {
                state: Arc::clone(&state),
            },
            InlineCpu,
        );
        let progress = RecordingProgress::default();

        service
            .replace(
                &project(),
                RulesProgram::from_toml(PathBuf::from("rules.toml"), b"rule = []".to_vec())
                    .expect("显式空定义应可建立"),
                ExtractProgress::new(progress.clone()),
            )
            .await
            .expect("显式空集合应停用 Rules owner");

        assert_eq!(document_reads.load(Ordering::SeqCst), 0);
        assert_eq!(state.replacements.load(Ordering::SeqCst), 0);
        assert_eq!(state.deactivations.load(Ordering::SeqCst), 1);
        assert_eq!(
            progress.snapshots(),
            [ProgressSnapshot::indeterminate(
                ExtractProgressPhase::RulesCommit
            )]
        );
    }

    fn test_service(
        bytes: Vec<u8>,
        documents: RpgMakerProjectDocuments,
        state: Arc<StoreState>,
    ) -> (
        RulesExtractionService<FakeDocumentReader, FakeStore, InlineCpu>,
        RulesProgram,
    ) {
        let program = RulesProgram::from_toml(PathBuf::from("rules.toml"), bytes)
            .expect("测试 Rules 应通过输入边界");
        let service = RulesExtractionService::new(
            FakeDocumentReader {
                documents,
                reads: Arc::new(AtomicUsize::new(0)),
            },
            FakeStore { state },
            InlineCpu,
        );
        (service, program)
    }

    fn project() -> OpenedProject {
        OpenedProject::new(
            "rules-test".parse::<ProjectName>().expect("项目名应合法"),
            PathBuf::from("C:/att/projects/rules-test"),
            PathBuf::from("C:/att/projects/rules-test/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::rpg_maker::project::test_layout_profile(),
        )
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct FakeError;

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("测试错误")
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
                DiagnosticSubject::component("fake_rules_dependency"),
                DiagnosticReason::failure(DiagnosticFailureKind::InternalInvariant),
                impact,
                fallback_action,
            )
        }
    }

    impl RpgMakerProjectDocumentReadingDiagnostic for FakeError {
        fn safe_document_reading_diagnostic(
            &self,
            code: DiagnosticCode,
            stage: DiagnosticStage,
        ) -> SafeDiagnostic {
            SafeDiagnostic::new(
                code,
                stage,
                DiagnosticSubject::component("fake_rules_document_reader"),
                DiagnosticReason::failure(DiagnosticFailureKind::InvalidValue),
                DiagnosticImpact::Unchanged,
                DiagnosticAction::CheckProjectState,
            )
        }
    }

    impl SafeDiagnosticSource for CpuTaskExecutionError<FakeError> {
        fn safe_diagnostic_source(
            &self,
            stage: DiagnosticStage,
            impact: DiagnosticImpact,
            fallback_action: DiagnosticAction,
        ) -> SafeDiagnostic {
            match self {
                Self::Unavailable(source) => {
                    source.safe_diagnostic_source(stage, impact, fallback_action)
                }
                Self::Cancelled => SafeDiagnostic::new(
                    DiagnosticCode::InternalOperation,
                    stage,
                    DiagnosticSubject::component("fake Rules CPU worker"),
                    DiagnosticReason::failure(DiagnosticFailureKind::LockCancelled),
                    impact,
                    DiagnosticAction::Retry,
                ),
                Self::TaskPanicked => SafeDiagnostic::new(
                    DiagnosticCode::InternalOperation,
                    stage,
                    DiagnosticSubject::component("fake Rules CPU worker"),
                    DiagnosticReason::failure(DiagnosticFailureKind::WorkerPanicked),
                    impact,
                    DiagnosticAction::ReportBug,
                ),
            }
        }
    }

    #[test]
    fn rules_projection_keeps_rule_and_path_without_matcher_free_text() {
        let ordinary_parameter = json!("ORIGINAL_TEXT_AND_JSON_BODY_SENTINEL");
        let error: RulesExtractionError<FakeError, FakeError, FakeError> =
            RulesExtractionError::InvalidTarget {
                rules_path: PathBuf::from("rules/main.toml"),
                source: RulesMatchError::invalid_plugin_field(
                    7,
                    3,
                    "QuestPlugin".to_owned(),
                    "parameters",
                    "object",
                    Some(&ordinary_parameter),
                ),
            };
        let diagnostic = error.safe_diagnostic();
        let serialized = serde_json::to_string(&diagnostic).expect("诊断应可序列化");

        assert!(!serialized.contains("ORIGINAL_TEXT_AND_JSON_BODY_SENTINEL"));
        assert!(serialized.contains("extract.rules"));
        assert!(serialized.contains("rules/main.toml"));
        assert!(serialized.contains("rules_invalid_target"));
        assert!(serialized.contains("rule=7"));
        assert!(serialized.contains("plugin_index=3"));
        assert!(serialized.contains("target=parameters"));
        assert!(serialized.contains("expected=object"));
        assert!(serialized.contains("actual=string"));
    }

    #[test]
    fn rules_failure_report_keeps_typed_document_cpu_match_and_snapshot_diagnostics() {
        type TypedDocumentError = RpgMakerProjectDocumentReadingError<
            crate::runtime::filesystem::SystemFileSystemError,
            crate::runtime::filesystem::SystemFileSystemError,
            crate::runtime::cpu::CpuExecutorUnavailable,
        >;
        type TypedRulesError = RulesExtractionError<
            TypedDocumentError,
            crate::runtime::sqlite::SqliteRuntimeError,
            crate::runtime::cpu::CpuExecutorUnavailable,
        >;

        let rules_path = PathBuf::from("rules/main.toml");
        let document_path = PathBuf::from(r"C:\games\demo\data\Items.json");
        let read_error: TypedRulesError = RulesExtractionError::ReadDocuments {
            rules_path: rules_path.clone(),
            source: RpgMakerProjectDocumentReadingError::ReadDocument {
                path: document_path.clone(),
                source: crate::storage::file_system::ReadFileError::Io {
                    path: document_path.clone(),
                    source: crate::runtime::filesystem::SystemFileSystemError::Io {
                        operation: "read_rules_document",
                        path: document_path.clone(),
                        source: std::io::Error::from_raw_os_error(5),
                    },
                },
            },
        };
        let borrowed = read_error.safe_diagnostic();
        assert_eq!(borrowed.code, DiagnosticCode::ExtractRules);
        assert_eq!(borrowed.stage, DiagnosticStage::Extract);
        let report = read_error.into_failure_report();
        assert!(report.primary.source_error().is::<TypedDocumentError>());
        let read = serde_json::to_string(report.primary.public()).expect("文档诊断应可序列化");
        assert_eq!(
            report.primary.public().subject,
            DiagnosticSubject::path(&document_path)
        );
        assert!(read.contains(rules_path.to_string_lossy().as_ref()));
        assert!(read.contains("read_rules_document"));
        assert!(read.contains("\"raw_os_code\":5"));

        let cpu_error: TypedRulesError = RulesExtractionError::MatchSourceCompute {
            rules_path: rules_path.clone(),
            source: CpuTaskExecutionError::Unavailable(
                crate::runtime::cpu::CpuExecutorUnavailable::StatePoisoned,
            ),
        };
        let report = cpu_error.into_failure_report();
        assert!(
            report
                .primary
                .source_error()
                .is::<CpuTaskExecutionError<crate::runtime::cpu::CpuExecutorUnavailable>>()
        );
        let cpu = serde_json::to_string(report.primary.public()).expect("CPU 诊断应可序列化");
        assert!(cpu.contains("worker_panicked"));
        assert!(cpu.contains("rules_operation=match_source"));
        assert!(cpu.contains("rules/main.toml"));

        let build_cpu_error: TypedRulesError = RulesExtractionError::BuildSnapshotCompute {
            rules_path: rules_path.clone(),
            source: CpuTaskExecutionError::Cancelled,
        };
        let report = build_cpu_error.into_failure_report();
        let build_cpu =
            serde_json::to_string(report.primary.public()).expect("快照 CPU 诊断应可序列化");
        assert!(build_cpu.contains("lock_cancelled"));
        assert!(build_cpu.contains("rules_operation=build_snapshot"));

        let ordinary_parameter = json!("ORIGINAL_AND_JSON_BODY_SENTINEL");
        let target_error: TypedRulesError = RulesExtractionError::InvalidTarget {
            rules_path: rules_path.clone(),
            source: RulesMatchError::invalid_plugin_field(
                7,
                4,
                "QuestPlugin".to_owned(),
                "parameters",
                "object",
                Some(&ordinary_parameter),
            ),
        };
        let report = target_error.into_failure_report();
        assert!(report.primary.source_error().is::<RulesMatchError>());
        let target = serde_json::to_string(report.primary.public()).expect("匹配诊断应可序列化");
        assert!(target.contains("rules_invalid_target"));
        assert!(target.contains("rule=7"));
        assert!(!target.contains("ORIGINAL_AND_JSON_BODY_SENTINEL"));

        let snapshot_error: TypedRulesError = RulesExtractionError::InvalidSnapshot {
            rules_path,
            source: SnapshotModelError::Projection(ProjectionModelError::EmptyScalarFieldKey),
        };
        let report = snapshot_error.into_failure_report();
        assert!(report.primary.source_error().is::<SnapshotModelError>());
        let snapshot = serde_json::to_string(report.primary.public()).expect("快照诊断应可序列化");
        assert!(snapshot.contains("rules_snapshot_invalid"));
        assert!(snapshot.contains("snapshot_error=projection.empty_scalar_field_key"));
    }

    #[test]
    fn rules_persist_report_preserves_store_outcome_unknown_and_related_cleanup() {
        type StoreError = crate::rpg_maker::extract::store::RpgMakerExtractionAssetStoreError<
            crate::runtime::cpu::CpuExecutorUnavailable,
            crate::runtime::sqlite::SqliteRuntimeError,
        >;
        type TypedRulesError = RulesExtractionError<
            FakeError,
            StoreError,
            crate::runtime::cpu::CpuExecutorUnavailable,
        >;

        let rules_path = PathBuf::from("rules/main.toml");
        let database_path = PathBuf::from(r"C:\projects\demo\project.db");
        let error: TypedRulesError = RulesExtractionError::Persist {
            rules_path: rules_path.clone(),
            source: StoreError::OutcomeUnknown {
                database_path: database_path.clone(),
                source: crate::runtime::sqlite::SqliteRuntimeError::Cleanup {
                    primary: Box::new(crate::runtime::sqlite::SqliteRuntimeError::Io {
                        operation: "commit_rules_snapshot",
                        path: database_path.clone(),
                        source: std::io::Error::from_raw_os_error(1117),
                    }),
                    failures: vec![crate::runtime::sqlite::SqliteRuntimeError::Io {
                        operation: "close_rules_snapshot",
                        path: database_path,
                        source: std::io::Error::from_raw_os_error(6),
                    }],
                },
            },
        };

        let borrowed = error.safe_diagnostic();
        assert_eq!(borrowed.impact, DiagnosticImpact::OutcomeUnknown);
        let report = error.into_failure_report();
        assert_eq!(
            report.primary.public().impact,
            DiagnosticImpact::OutcomeUnknown
        );
        assert_eq!(report.related.len(), 1);
        assert_eq!(
            report.related[0].public().impact,
            DiagnosticImpact::OutcomeUnknown
        );
        let primary =
            serde_json::to_string(report.primary.public()).expect("Store 主诊断应可序列化");
        let related =
            serde_json::to_string(report.related[0].public()).expect("Store 相关诊断应可序列化");
        assert!(primary.contains("rules/main.toml"));
        assert!(primary.contains("project.db"));
        assert!(primary.contains("outcome_unknown"));
        assert!(primary.contains("\"raw_os_code\":1117"));
        assert!(related.contains("\"raw_os_code\":6"));
    }

    #[derive(Clone)]
    struct FakeDocumentReader {
        documents: RpgMakerProjectDocuments,
        reads: Arc<AtomicUsize>,
    }

    impl RpgMakerProjectDocumentReader for FakeDocumentReader {
        type Error = FakeError;

        async fn read(
            &self,
            _project: &OpenedProject,
            _selection: RpgMakerDocumentSelection,
        ) -> Result<RpgMakerProjectDocuments, Self::Error> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(self.documents.clone())
        }
    }

    #[derive(Default)]
    struct StoreState {
        replacements: AtomicUsize,
        deactivations: AtomicUsize,
    }

    #[derive(Clone)]
    struct FakeStore {
        state: Arc<StoreState>,
    }

    impl RulesSnapshotStore for FakeStore {
        type Error = FakeError;

        async fn replace_rules(
            &self,
            _project: &OpenedProject,
            _snapshot: RulesSnapshot,
        ) -> Result<(), Self::Error> {
            self.state.replacements.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn deactivate_rules(&self, _project: &OpenedProject) -> Result<(), Self::Error> {
            self.state.deactivations.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

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
}
