//! 诊断、状态影响和相关失败的核心模型。

use std::error::Error;
use std::fmt;

use serde::de::Error as _;
use serde::{Deserialize, Serialize};

use crate::i18n::{UiLocalizer, UiMessage};

use super::{
    ConfigurationIssue, DiagnosticIssue, DiagnosticStage, HttpIssue, PlaceholderIssue,
    PlaceholderRuleSource, TranslationIssue,
};

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
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiagnosticResolution {
    FixConfiguration,
    FixInput,
    FixPlaceholderRules,
    ReviewDisabledRules,
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
            Self::ReviewDisabledRules => "review_disabled_rules",
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
    source: super::BoxedError,
}

impl ReportedFailure {
    pub(crate) fn new(
        report: DiagnosticReport,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            report,
            source: Box::new(source),
        }
    }

    /// 把相关错误的安全报告加入公开树；根错误仍作为 Error source。
    pub(crate) fn with_related(mut self, relation: RelatedFailureRelation, related: Self) -> Self {
        self.report = self.report.with_related(relation, related.report);
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
        self.source.as_ref()
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
        Some(self.source.as_ref())
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

fn render_diagnostic(report: &DiagnosticReport, localizer: &UiLocalizer) -> String {
    let fields = render_diagnostic_fields(report, localizer);
    [
        localizer.format(UiMessage::DiagnosticObject {
            subject: &fields.object,
        }),
        localizer.format(UiMessage::DiagnosticExplanation {
            reason: &fields.reason,
        }),
        localizer.format(UiMessage::DiagnosticImpact {
            impact: &fields.impact,
        }),
        localizer.format(UiMessage::DiagnosticResolution {
            action: &fields.help,
        }),
    ]
    .join("\n")
}

/// CLI、日志退化提示和任务记录共同使用的报告呈现。
pub(crate) fn render_diagnostic_report(
    report: &DiagnosticReport,
    localizer: &UiLocalizer,
) -> String {
    let mut blocks = vec![render_diagnostic(report, localizer)];
    for related in report.related() {
        blocks.push(format!(
            "{}\n{}",
            localizer.format(UiMessage::DiagnosticRelated {
                relation: related.relation().as_str(),
            }),
            render_diagnostic_report(related.report(), localizer)
        ));
    }
    blocks.join("\n\n")
}

/// 面向项目日志的最小诊断正文。
///
/// 项目日志不承担调试协议或恢复状态的传递职责，只告诉读者哪个对象出了什么问题以及
/// 对业务状态的影响以及应该怎么处理。SQLite 查询编号、供应商请求标识、指纹对比和
/// 递归内部状态不会进入这些字段。
pub(crate) struct RenderedDiagnosticFields {
    pub(crate) object: String,
    pub(crate) reason: String,
    pub(crate) impact: String,
    pub(crate) help: String,
}

pub(crate) fn render_diagnostic_fields(
    report: &DiagnosticReport,
    localizer: &UiLocalizer,
) -> RenderedDiagnosticFields {
    render_diagnostic_fields_for(report.primary(), report.effect(), localizer)
}

/// 把类型化的业务状态影响转换为用户当前语言下的公开文本。
///
/// CLI 的诊断、Manual 的逐项错误和不完整结果警告必须共用这一入口，避免各自用裸字符串
/// 或平行文案解释同一个 `StateEffect`。
pub(crate) fn render_state_effect_impact(effect: StateEffect, localizer: &UiLocalizer) -> String {
    localizer.format(UiMessage::DiagnosticImpactValue {
        effect: effect.as_str(),
    })
}

fn render_diagnostic_fields_for(
    diagnostic: &Diagnostic,
    effect: StateEffect,
    localizer: &UiLocalizer,
) -> RenderedDiagnosticFields {
    let issue = diagnostic.issue();
    let summary = localizer.format(UiMessage::DiagnosticFailureValue {
        code: issue.summary_code(),
    });
    let reason = render_diagnostic_reason(issue, summary, localizer);
    RenderedDiagnosticFields {
        object: issue.subject(),
        reason,
        impact: render_state_effect_impact(effect, localizer),
        help: localizer.format(UiMessage::DiagnosticResolutionValue {
            code: diagnostic.resolution().as_str(),
        }),
    }
}

fn render_diagnostic_reason(
    issue: &DiagnosticIssue,
    summary: String,
    localizer: &UiLocalizer,
) -> String {
    if let DiagnosticIssue::Configuration(ConfigurationIssue::InvalidValue { rule, .. }) = issue {
        return rule.render_localized(localizer);
    }

    let mut details = Vec::new();
    match issue {
        DiagnosticIssue::Translation(TranslationIssue::Placeholder {
            rule_source,
            problem,
            ..
        }) => {
            if let Some(rule_number) = placeholder_rule_number(problem)
                && let Ok(number) = u64::try_from(rule_number)
            {
                details.push(match rule_source {
                    PlaceholderRuleSource::ExternalFile { path } => {
                        localizer.format(UiMessage::DiagnosticPlaceholderRuleFile {
                            number,
                            path: &path.to_string(),
                        })
                    }
                    PlaceholderRuleSource::ProjectSnapshot => {
                        localizer.format(UiMessage::DiagnosticPlaceholderRuleProject { number })
                    }
                });
            }
        }
        DiagnosticIssue::Http(HttpIssue::Status {
            status,
            retry_after_seconds,
            provider_code,
            provider_type,
            provider_message,
            ..
        }) => {
            details.push(localizer.format(UiMessage::DiagnosticHttpStatus {
                status: u64::from(*status),
            }));
            if let Some(seconds) = retry_after_seconds {
                details
                    .push(localizer.format(UiMessage::DiagnosticRetryAfter { seconds: *seconds }));
            }
            if let Some(code) = provider_code {
                details.push(localizer.format(UiMessage::DiagnosticProviderCode {
                    code: &code.to_string(),
                }));
            }
            if let Some(kind) = provider_type {
                details.push(localizer.format(UiMessage::DiagnosticProviderType {
                    kind: &kind.to_string(),
                }));
            }
            if let Some(message) = provider_message {
                details.push(localizer.format(UiMessage::DiagnosticProviderMessage {
                    message: &message.to_string(),
                }));
            }
        }
        DiagnosticIssue::Http(
            HttpIssue::RequestSerialization { line, column, .. }
            | HttpIssue::ResponseJson { line, column, .. },
        ) => {
            if let (Ok(line), Ok(column)) = (u64::try_from(*line), u64::try_from(*column)) {
                details.push(localizer.format(UiMessage::DiagnosticJsonPosition { line, column }));
            }
        }
        DiagnosticIssue::RpgMaker(issue) => {
            if let Some(detail) = issue.manual_layout_reason_detail() {
                details.push(detail);
            }
        }
        _ => {}
    }

    if let Some(system_message) = issue
        .facts()
        .into_iter()
        .find_map(|(name, value)| (name == "system_message").then_some(value))
        .and_then(|value| readable_system_message(&value))
    {
        details.push(system_message);
    }
    if details.is_empty() {
        summary
    } else {
        format!("{summary} ({})", details.join("; "))
    }
}

fn placeholder_rule_number(problem: &PlaceholderIssue) -> Option<usize> {
    match problem {
        PlaceholderIssue::PatternMatch { rule_number, .. }
        | PlaceholderIssue::EmptyMatch { rule_number, .. }
        | PlaceholderIssue::CrossesLineBoundary { rule_number, .. } => *rule_number,
        PlaceholderIssue::MissingTextCapture { rule_number, .. }
        | PlaceholderIssue::InvalidMatchRange { rule_number, .. } => Some(*rule_number),
        PlaceholderIssue::OverlappingMatches {
            first_rule_number,
            second_rule_number,
            ..
        } => first_rule_number.or(*second_rule_number),
        PlaceholderIssue::WorkerStart { .. } | PlaceholderIssue::ReservedTokenNamespace { .. } => {
            None
        }
    }
}

fn readable_system_message(value: &str) -> Option<String> {
    let mut message = value.trim();
    if let Some(prefix) = message.strip_suffix(')')
        && let Some((text, code)) = prefix.rsplit_once(" (os error ")
        && !code.is_empty()
        && code.bytes().all(|byte| byte.is_ascii_digit())
    {
        message = text.trim_end();
    }
    (!message.is_empty()).then(|| message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::{
        ByteRange, ConfigurationValueRule, GenericUnitLocator, HttpEndpoint, HttpScheme,
        PlaceholderIssue, PlaceholderRuleSource, RpgMakerIssue, RpgMakerRulesDiagnosticSource,
        RpgMakerRulesMatchContext, RpgMakerRulesMatchProblem, RpgMakerRulesValueStep,
        SafeIdentifier, SafeText, TranslationIssue,
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
    fn every_state_effect_has_a_distinct_readable_impact() {
        for locale in crate::i18n::UiLocale::ALL {
            let localizer = UiLocalizer::new(locale);
            let mut impacts = std::collections::BTreeSet::new();

            for effect in [
                StateEffect::Unchanged,
                StateEffect::ProgressPreserved,
                StateEffect::Applied,
                StateEffect::AppliedRunPlanNotSaved,
                StateEffect::AppliedFinalizationFailed,
                StateEffect::RecoveryRequired,
                StateEffect::OutcomeUnknown,
            ] {
                let report = DiagnosticReport::new(effect, missing_capture());
                let impact = render_diagnostic_fields(&report, &localizer).impact;
                assert!(
                    !impact.is_empty(),
                    "{locale} 的 {effect:?} 必须有公开影响说明"
                );
                assert!(
                    !impact.contains("__ATT_FALLBACK__"),
                    "{locale} 的 {effect:?} 缺少本地化影响说明"
                );
                impacts.insert(impact);
            }

            assert_eq!(
                impacts.len(),
                7,
                "{locale} 的七种状态影响不能合并成模糊的同一句话"
            );
        }
    }

    #[test]
    fn related_reports_keep_their_natural_relation_headings() {
        let relations = [
            RelatedFailureRelation::Cleanup,
            RelatedFailureRelation::Rollback,
            RelatedFailureRelation::Discard,
            RelatedFailureRelation::Finalization,
            RelatedFailureRelation::Shutdown,
            RelatedFailureRelation::Observability,
        ];
        for locale in crate::i18n::UiLocale::ALL {
            let localizer = UiLocalizer::new(locale);
            let headings = relations
                .iter()
                .map(|relation| {
                    localizer.format(UiMessage::DiagnosticRelated {
                        relation: relation.as_str(),
                    })
                })
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(headings.len(), relations.len());
            assert!(
                headings
                    .iter()
                    .all(|heading| !heading.contains("__ATT_FALLBACK__")),
                "{locale} 缺少相关失败标题"
            );
        }

        let mut report = DiagnosticReport::new(StateEffect::Unchanged, missing_capture());
        for relation in relations {
            report = report.with_related(
                relation,
                DiagnosticReport::new(StateEffect::Unchanged, missing_capture()),
            );
        }

        let rendered =
            render_diagnostic_report(&report, &UiLocalizer::new(crate::i18n::UiLocale::English));
        assert!(!rendered.contains("__ATT_FALLBACK__"));
        for relation in [
            "cleanup",
            "rollback",
            "discard",
            "finalization",
            "shutdown",
            "observability",
        ] {
            assert!(
                !rendered.contains(&format!("{relation}:")),
                "终端关系标题不能直接泄漏内部 relation code"
            );
        }
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

    #[test]
    fn configuration_value_reason_keeps_the_specific_localized_rule() {
        let report = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::configuration(ConfigurationIssue::InvalidValue {
                path: None,
                field: SafeIdentifier::new("llm.clients.primary.max_concurrent_requests")
                    .expect("测试字段必须安全"),
                rule: ConfigurationValueRule::RuntimeMaximumExceeded {
                    actual: 2_000_000,
                    maximum: 1_000_000,
                },
            }),
        );

        let fields =
            render_diagnostic_fields(&report, &UiLocalizer::new(crate::i18n::UiLocale::English));

        assert_eq!(
            fields.reason.replace(['\u{2068}', '\u{2069}'], ""),
            "Value exceeds runtime maximum (actual=2000000, maximum=1000000)"
        );
    }

    #[test]
    fn http_reason_keeps_the_safe_provider_response_projection() {
        let report = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::http(HttpIssue::Status {
                endpoint: HttpEndpoint::new(HttpScheme::Https, "api.example.test", None),
                status: 429,
                retry_after_seconds: Some(12),
                provider_code: Some(SafeIdentifier::new("rate_limit").expect("测试 code 必须安全")),
                provider_type: Some(SafeIdentifier::new("requests").expect("测试 type 必须安全")),
                provider_message: Some(SafeText::new("Please retry later")),
                response_read_failure: None,
            }),
        );

        let fields =
            render_diagnostic_fields(&report, &UiLocalizer::new(crate::i18n::UiLocale::English));
        let reason = fields.reason.replace(['\u{2068}', '\u{2069}'], "");

        for expected in [
            "HTTP status 429",
            "Retry-After: 12 seconds",
            "Provider code:",
            "rate_limit",
            "Provider type:",
            "requests",
            "Provider message:",
            "Please retry later",
        ] {
            assert!(reason.contains(expected), "缺少 {expected:?}");
        }
    }

    #[test]
    fn placeholder_reason_keeps_the_natural_rule_number() {
        let report = DiagnosticReport::new(StateEffect::Unchanged, missing_capture());

        let fields =
            render_diagnostic_fields(&report, &UiLocalizer::new(crate::i18n::UiLocale::English));
        let reason = fields.reason.replace(['\u{2068}', '\u{2069}'], "");

        assert!(reason.contains("Placeholder rule 2"));
        assert!(reason.contains("D:/rules.toml"));
        assert!(!reason.contains("4..12"), "不得公开编码位置");
    }

    #[test]
    fn rules_zero_width_diagnostic_names_rule_target_and_direct_reason() {
        let report = DiagnosticReport::new(
            StateEffect::Unchanged,
            Diagnostic::rpg_maker(RpgMakerIssue::rules_match(
                "rules.toml",
                Some(RpgMakerRulesMatchContext::new(
                    RpgMakerRulesDiagnosticSource::DataFile {
                        file: SafeText::new("States.json"),
                    },
                    true,
                )),
                RpgMakerRulesMatchProblem::ZeroWidthMatch {
                    rule_number: 3,
                    at: vec![
                        RpgMakerRulesValueStep::Index { index: 415 },
                        RpgMakerRulesValueStep::key("note"),
                    ],
                    match_range: ByteRange::new(27, 27).expect("零宽范围仍是有效字节范围"),
                },
            )),
        );

        let fields =
            render_diagnostic_fields(&report, &UiLocalizer::new(crate::i18n::UiLocale::English));

        assert_eq!(
            fields.object,
            r#"rules.toml:Rules[3] -> States.json$[415]["note"]"#
        );
        assert_eq!(fields.reason, "The text capture is empty");
        assert!(!fields.reason.contains("27"), "不得公开原文字节位置");
    }
}
