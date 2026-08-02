//! 诊断、状态影响和相关失败的核心模型。

use std::error::Error;
use std::fmt;

use serde::de::Error as _;
use serde::{Deserialize, Serialize};

use crate::i18n::{UiLocalizer, UiMessage};

use super::{DiagnosticIssue, DiagnosticStage};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StateEffect {
    Unchanged,
    ProgressPreserved,
    Applied,
    AppliedRunPlanNotSaved,
    AppliedFinalizationFailed,
    RecoveryRequired,
    OutcomeUnknown,
}

impl StateEffect {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::ProgressPreserved => "progress_preserved",
            Self::Applied => "applied",
            Self::AppliedRunPlanNotSaved => "applied_run_plan_not_saved",
            Self::AppliedFinalizationFailed => "applied_finalization_failed",
            Self::RecoveryRequired => "recovery_required",
            Self::OutcomeUnknown => "outcome_unknown",
        }
    }

    const fn rank(self) -> u8 {
        match self {
            Self::Unchanged => 0,
            Self::ProgressPreserved => 1,
            Self::Applied => 2,
            Self::AppliedRunPlanNotSaved => 3,
            Self::AppliedFinalizationFailed => 4,
            Self::RecoveryRequired => 5,
            Self::OutcomeUnknown => 6,
        }
    }

    pub(crate) const fn strongest(self, other: Self) -> Self {
        if self.rank() >= other.rank() {
            self
        } else {
            other
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiagnosticResolution {
    FixConfiguration,
    FixInput,
    FixPlaceholderRules,
    AdjustManualLayout,
    CheckPathAndPermissions,
    CheckProjectState,
    ResolveContention,
    CheckModelService,
    PreserveRecoveryArtifacts,
    Retry,
    ReportBug,
}

impl DiagnosticResolution {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::FixConfiguration => "fix_configuration",
            Self::FixInput => "fix_input",
            Self::FixPlaceholderRules => "fix_placeholder_rules",
            Self::AdjustManualLayout => "adjust_manual_layout",
            Self::CheckPathAndPermissions => "check_path_and_permissions",
            Self::CheckProjectState => "check_project_state",
            Self::ResolveContention => "resolve_contention",
            Self::CheckModelService => "check_model_service",
            Self::PreserveRecoveryArtifacts => "preserve_recovery_artifacts",
            Self::Retry => "retry",
            Self::ReportBug => "report_bug",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RelatedFailureRelation {
    Cleanup,
    Rollback,
    Discard,
    Finalization,
    Shutdown,
    Observability,
}

impl RelatedFailureRelation {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Cleanup => "cleanup",
            Self::Rollback => "rollback",
            Self::Discard => "discard",
            Self::Finalization => "finalization",
            Self::Shutdown => "shutdown",
            Self::Observability => "observability",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Diagnostic {
    stage: DiagnosticStage,
    issue: Box<DiagnosticIssue>,
}

impl Diagnostic {
    pub(crate) fn configuration(issue: super::ConfigurationIssue) -> Self {
        Self::from_issue(issue.into())
    }

    pub(crate) fn translation(issue: super::TranslationIssue) -> Self {
        Self::from_issue(issue.into())
    }

    pub(crate) fn generic(issue: super::GenericIssue) -> Self {
        Self::from_issue(issue.into())
    }

    pub(crate) fn lua(issue: super::LuaIssue) -> Self {
        Self::from_issue(issue.into())
    }

    pub(crate) fn rpg_maker(issue: super::RpgMakerIssue) -> Self {
        Self::from_issue(issue.into())
    }

    pub(crate) fn publication(issue: super::PublicationIssue) -> Self {
        Self::from_issue(issue.into())
    }

    pub(crate) fn runtime(issue: super::RuntimeIssue) -> Self {
        Self::from_issue(issue.into())
    }

    pub(crate) fn file_system(issue: super::FileSystemIssue) -> Self {
        Self::from_issue(issue.into())
    }

    pub(crate) fn sqlite(issue: super::SqliteIssue) -> Self {
        Self::from_issue(issue.into())
    }

    pub(crate) fn http(issue: super::HttpIssue) -> Self {
        Self::from_issue(issue.into())
    }

    pub(crate) fn observability(issue: super::ObservabilityIssue) -> Self {
        Self::from_issue(issue.into())
    }

    fn from_issue(issue: DiagnosticIssue) -> Self {
        Self {
            stage: issue.stage(),
            issue: Box::new(issue),
        }
    }

    pub(crate) const fn stage(&self) -> DiagnosticStage {
        self.stage
    }

    pub(crate) fn issue(&self) -> &DiagnosticIssue {
        self.issue.as_ref()
    }

    pub(crate) fn code(&self) -> &'static str {
        self.issue.code()
    }

    pub(crate) const fn resolution(&self) -> DiagnosticResolution {
        self.issue.resolution()
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct DiagnosticWire<'a> {
    code: &'static str,
    stage: DiagnosticStage,
    issue: &'a DiagnosticIssue,
    resolution: DiagnosticResolution,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedDiagnosticWire {
    code: String,
    stage: DiagnosticStage,
    issue: DiagnosticIssue,
    resolution: DiagnosticResolution,
}

impl Serialize for Diagnostic {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        DiagnosticWire {
            code: self.code(),
            stage: self.stage(),
            issue: self.issue(),
            resolution: self.resolution(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Diagnostic {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = OwnedDiagnosticWire::deserialize(deserializer)?;
        let diagnostic = Self::from_issue(wire.issue);
        if wire.code != diagnostic.code() {
            return Err(D::Error::custom("诊断 code 与 issue 不一致"));
        }
        if wire.stage != diagnostic.stage() {
            return Err(D::Error::custom("诊断 stage 与 issue 不一致"));
        }
        if wire.resolution != diagnostic.resolution() {
            return Err(D::Error::custom("诊断 resolution 与 issue 不一致"));
        }
        Ok(diagnostic)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DiagnosticReport {
    effect: StateEffect,
    primary: Diagnostic,
    related: Vec<RelatedDiagnosticReport>,
}

impl DiagnosticReport {
    pub(crate) fn new(effect: StateEffect, primary: Diagnostic) -> Self {
        Self {
            effect,
            primary,
            related: Vec::new(),
        }
    }

    pub(crate) fn with_related(
        mut self,
        relation: RelatedFailureRelation,
        report: DiagnosticReport,
    ) -> Self {
        self.effect = self.effect.strongest(report.effect);
        self.related.push(RelatedDiagnosticReport {
            relation,
            report: Box::new(report),
        });
        self
    }

    /// 上层编排已经确认有更强状态影响时，只能提升而不能削弱叶子报告的终态。
    pub(crate) fn with_effect(mut self, effect: StateEffect) -> Self {
        self.effect = self.effect.strongest(effect);
        self
    }

    pub(crate) const fn effect(&self) -> StateEffect {
        self.effect
    }

    pub(crate) fn primary(&self) -> &Diagnostic {
        &self.primary
    }

    pub(crate) fn related(&self) -> &[RelatedDiagnosticReport] {
        &self.related
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RelatedDiagnosticReport {
    relation: RelatedFailureRelation,
    report: Box<DiagnosticReport>,
}

/// 原始 Rust 错误与唯一安全公开报告的绑定。原始错误绝不参与 JSONL 或 CLI 呈现。
pub(crate) struct ReportedFailure {
    report: DiagnosticReport,
    sources: ReportedFailureSources,
}

struct ReportedFailureSources {
    primary: super::BoxedError,
    related: Vec<RelatedReportedFailureSources>,
}

struct RelatedReportedFailureSources {
    #[allow(dead_code)]
    relation: RelatedFailureRelation,
    #[allow(dead_code)]
    sources: Box<ReportedFailureSources>,
}

impl ReportedFailure {
    pub(crate) fn new(
        report: DiagnosticReport,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            report,
            sources: ReportedFailureSources {
                primary: Box::new(source),
                related: Vec::new(),
            },
        }
    }

    /// 同时保留安全报告树和对应的 Rust 原始错误树；相关错误不会被压平成正文。
    pub(crate) fn with_related(mut self, relation: RelatedFailureRelation, related: Self) -> Self {
        self.report = self.report.with_related(relation, related.report);
        self.sources.related.push(RelatedReportedFailureSources {
            relation,
            sources: Box::new(related.sources),
        });
        self
    }

    /// 保留原始错误树，只提升公开报告的状态影响。
    pub(crate) fn with_effect(mut self, effect: StateEffect) -> Self {
        self.report = self.report.with_effect(effect);
        self
    }

    pub(crate) fn report(&self) -> &DiagnosticReport {
        &self.report
    }

    pub(crate) fn into_report(self) -> DiagnosticReport {
        self.report
    }

    pub(crate) fn source_error(&self) -> &(dyn Error + 'static) {
        self.sources.primary.as_ref()
    }
}

impl fmt::Debug for ReportedFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReportedFailure")
            .field("report", &self.report)
            .finish_non_exhaustive()
    }
}

impl fmt::Display for ReportedFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.report.primary().code())
    }
}

impl Error for ReportedFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.sources.primary.as_ref())
    }
}

impl RelatedDiagnosticReport {
    pub(crate) const fn relation(&self) -> RelatedFailureRelation {
        self.relation
    }

    pub(crate) fn report(&self) -> &DiagnosticReport {
        &self.report
    }
}

fn render_diagnostic_with_effect(
    diagnostic: &Diagnostic,
    effect: StateEffect,
    localizer: &UiLocalizer,
) -> String {
    let issue = diagnostic.issue();
    let stage = localizer.format(UiMessage::DiagnosticStageValue {
        code: diagnostic.stage().as_str(),
    });
    let subject = issue.subject();
    let summary = localizer.format(UiMessage::DiagnosticFailureValue {
        code: issue.summary_code(),
    });
    let facts = issue
        .facts()
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ");
    let reason = if facts.is_empty() {
        summary
    } else {
        format!("{summary}; {facts}")
    };
    let effect = localizer.format(UiMessage::DiagnosticEffectValue {
        code: effect.as_str(),
    });
    let resolution = localizer.format(UiMessage::DiagnosticResolutionValue {
        code: diagnostic.resolution().as_str(),
    });
    [
        localizer.format(UiMessage::DiagnosticTitle {
            code: diagnostic.code(),
        }),
        localizer.format(UiMessage::DiagnosticStage { stage: &stage }),
        localizer.format(UiMessage::DiagnosticLocation { subject: &subject }),
        localizer.format(UiMessage::DiagnosticExplanation { reason: &reason }),
        localizer.format(UiMessage::DiagnosticEffect { impact: &effect }),
        localizer.format(UiMessage::DiagnosticResolution {
            action: &resolution,
        }),
    ]
    .join("\n")
}

/// CLI、日志退化提示和任务记录共同使用的报告呈现。
pub(crate) fn render_diagnostic_report(
    report: &DiagnosticReport,
    localizer: &UiLocalizer,
) -> String {
    let mut blocks = vec![render_diagnostic_with_effect(
        report.primary(),
        report.effect(),
        localizer,
    )];
    for (index, related) in report.related().iter().enumerate() {
        let relation = localizer.format(UiMessage::DiagnosticRelationValue {
            code: related.relation().as_str(),
        });
        blocks.push(format!(
            "{} ({relation})\n{}",
            localizer.format(UiMessage::DiagnosticRelated {
                // Rust 当前支持目标的 usize 不宽于 u64；此转换不会丢失实际计数。
                index: index as u64 + 1,
            }),
            render_diagnostic_report(related.report(), localizer)
        ));
    }
    blocks.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::{
        ByteRange, GenericUnitLocator, PlaceholderIssue, PlaceholderRuleSource, TranslationIssue,
    };

    fn missing_capture() -> Diagnostic {
        Diagnostic::translation(TranslationIssue::Placeholder {
            rule_source: PlaceholderRuleSource::external_file("D:/rules.toml"),
            unit: GenericUnitLocator::new("dialogue/a.jsonl", "group-3", "unit-7", None),
            problem: PlaceholderIssue::MissingTextCapture {
                rule_number: 2,
                match_range: ByteRange::new(4, 12).expect("有效范围"),
            },
        })
    }

    #[test]
    fn missing_capture_wire_is_derived_from_issue() {
        let value = serde_json::to_value(DiagnosticReport::new(
            StateEffect::Unchanged,
            missing_capture(),
        ))
        .expect("诊断可序列化");
        assert_eq!(
            value["primary"]["code"],
            "translation.placeholder.missing_text_capture"
        );
        assert_eq!(value["primary"]["stage"], "translate");
        assert_eq!(value["primary"]["resolution"], "fix_placeholder_rules");
        assert_eq!(value["effect"], "unchanged");
        assert_eq!(
            value["primary"]["issue"]["details"]["problem"]["match_range"]["start"],
            4
        );
    }

    #[test]
    fn diagnostic_wire_rejects_code_stage_and_resolution_mismatch() {
        let mut value = serde_json::to_value(missing_capture()).expect("诊断可序列化");
        for (field, invalid) in [
            ("code", serde_json::json!("unknown.code")),
            ("stage", serde_json::json!("extract")),
            ("resolution", serde_json::json!("retry")),
        ] {
            let original = value[field].clone();
            value[field] = invalid;
            assert!(
                serde_json::from_value::<Diagnostic>(value.clone()).is_err(),
                "{field} 与 issue 不一致时必须拒绝"
            );
            value[field] = original;
        }
    }

    #[test]
    fn related_failure_promotes_strongest_effect() {
        let report = DiagnosticReport::new(StateEffect::Unchanged, missing_capture()).with_related(
            RelatedFailureRelation::Finalization,
            DiagnosticReport::new(StateEffect::OutcomeUnknown, missing_capture()),
        );
        assert_eq!(report.effect(), StateEffect::OutcomeUnknown);
        assert_eq!(report.related().len(), 1);
    }

    #[test]
    fn generic_locator_omits_unsafe_external_ids_without_panicking() {
        let sentinel = "GROUP\r\nAUTHORIZATION_SENTINEL";
        let report = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::translation(TranslationIssue::Placeholder {
                rule_source: PlaceholderRuleSource::ProjectSnapshot,
                unit: GenericUnitLocator::new(
                    "dialogue/a.jsonl",
                    sentinel,
                    "UNIT\u{202e}SENTINEL",
                    Some("ROLE\0SENTINEL"),
                ),
                problem: PlaceholderIssue::MissingTextCapture {
                    rule_number: 1,
                    match_range: ByteRange::new(0, 1).expect("有效范围"),
                },
            }),
        );

        let wire = serde_json::to_string(&report).expect("安全 locator 可序列化");
        assert!(!wire.contains(sentinel));
        assert!(!wire.contains("SENTINEL"));
        assert_eq!(
            serde_json::to_value(report).expect("诊断可序列化")["primary"]["issue"]["details"]["unit"]
                ["group_id"],
            serde_json::Value::Null
        );
    }
}
