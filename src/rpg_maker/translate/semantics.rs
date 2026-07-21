//! Standard 与可信 Lua 共同使用的一次翻译语义快照。

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::fingerprint::Sha256Fingerprint;
use crate::language::{LanguageAnalysis, LanguageModule, LanguagePair, LanguageTextSegment};
use crate::rpg_maker::RpgMakerEngine;
use crate::rpg_maker::model::TextUnitContent;
use crate::rpg_maker::text::TextGroupKind;

use super::executor::accept_prepared_translation_candidate;
use super::language_projection::{LanguageTextProjectionError, project_protected_text};
use super::placeholder::{
    CompiledPlaceholderRules, Pcre2PlaceholderService, PlaceholderProtectionError,
};
use super::planning_resource::CompiledTerminology;
use super::standard::{AppliedPlaceholder, TerminologyDependency, TranslationUnitRejectionReason};

/// 一轮 Standard 与 Lua 共享且不可变的当前翻译语义。
pub(crate) struct ResolvedTranslationSemantics {
    engine: RpgMakerEngine,
    system_prompt: String,
    language_pair: LanguagePair,
    terminology: Arc<CompiledTerminology>,
    placeholder_service: Pcre2PlaceholderService,
    custom_placeholders: CompiledPlaceholderRules,
    source_language: Arc<dyn LanguageModule>,
    global_fingerprint: Sha256Fingerprint,
}

impl fmt::Debug for ResolvedTranslationSemantics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedTranslationSemantics")
            .field("engine", &self.engine)
            .field("language_pair", &self.language_pair)
            .field("term_count", &self.terminology.entries().len())
            .field("global_fingerprint", &self.global_fingerprint)
            .finish_non_exhaustive()
    }
}

impl PartialEq for ResolvedTranslationSemantics {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}

impl Eq for ResolvedTranslationSemantics {}

impl ResolvedTranslationSemantics {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        engine: RpgMakerEngine,
        system_prompt: String,
        language_pair: LanguagePair,
        terminology: Arc<CompiledTerminology>,
        placeholder_service: Pcre2PlaceholderService,
        custom_placeholders: CompiledPlaceholderRules,
        source_language: Arc<dyn LanguageModule>,
        global_fingerprint: Sha256Fingerprint,
    ) -> Self {
        Self {
            engine,
            system_prompt,
            language_pair,
            terminology,
            placeholder_service,
            custom_placeholders,
            source_language,
            global_fingerprint,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        use std::num::NonZeroUsize;

        use crate::language::{
            JapaneseLanguageModule, JapaneseResidualPolicy, LanguageId, LanguagePair,
        };

        let placeholder_service = Pcre2PlaceholderService::new().expect("测试内建占位符应可编译");
        let custom_placeholders = placeholder_service
            .compile_custom(Vec::new())
            .expect("测试空占位符集应可编译");
        let source_language = Arc::new(JapaneseLanguageModule::new(
            JapaneseResidualPolicy::new(NonZeroUsize::new(1).expect("常量非零"), Vec::new())
                .expect("测试日文残留策略应有效"),
            None,
        ));
        Self::new(
            RpgMakerEngine::Mz,
            "test system".to_owned(),
            LanguagePair::new(
                LanguageId::parse("ja").expect("测试源语言应合法"),
                LanguageId::parse("zh-Hans").expect("测试目标语言应合法"),
            ),
            Arc::new(CompiledTerminology::empty()),
            placeholder_service,
            custom_placeholders,
            source_language,
            Sha256Fingerprint::from_bytes([0x5a; 32]),
        )
    }

    pub(crate) fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    pub(crate) const fn engine(&self) -> RpgMakerEngine {
        self.engine
    }

    pub(crate) fn language_pair(&self) -> &LanguagePair {
        &self.language_pair
    }

    pub(crate) const fn global_fingerprint(&self) -> Sha256Fingerprint {
        self.global_fingerprint
    }

    pub(crate) fn prepare(
        &self,
        kind: TextGroupKind,
        original: &str,
    ) -> Result<PreparedTranslationText, ResolvedTranslationSemanticError> {
        self.prepare_text(kind, original, &[])
    }

    /// Standard 的 `Lines` 保持完整原文参与 Placeholder 与语言分析，但术语不能跨越
    /// 两个物理数组元素。`Value` 没有这层边界，其中的 LF 仍是普通自然文本。
    pub(crate) fn prepare_content(
        &self,
        kind: TextGroupKind,
        content: &TextUnitContent,
    ) -> Result<PreparedTranslationText, ResolvedTranslationSemanticError> {
        match content {
            TextUnitContent::Value(original) => self.prepare(kind, original),
            TextUnitContent::Lines(lines) => {
                let original = lines.join("\n");
                let line_separators = line_separator_offsets(lines);
                self.prepare_text(kind, &original, &line_separators)
            }
        }
    }

    fn prepare_text(
        &self,
        kind: TextGroupKind,
        original: &str,
        line_separators: &[usize],
    ) -> Result<PreparedTranslationText, ResolvedTranslationSemanticError> {
        let (model_text, placeholders) = self
            .placeholder_service
            .protect(self.engine, kind, original, &self.custom_placeholders)
            .map_err(ResolvedTranslationSemanticError::ProtectPlaceholder)?
            .into_parts();
        let language_text = project_protected_text(&model_text, &placeholders)
            .map_err(ResolvedTranslationSemanticError::ProjectLanguageText)?;
        let language_analysis = self.source_language.analyze_source(&language_text);
        let status = if !language_text.has_non_whitespace_natural_text() {
            PreparedTranslationStatus::FullyProtected
        } else if language_analysis.needs_translation() {
            PreparedTranslationStatus::Active
        } else {
            PreparedTranslationStatus::NonSourceLanguage
        };
        let term_indices = if line_separators.is_empty() {
            self.terminology
                .triggered_indices(natural_segments(&language_text))
        } else {
            let domains =
                terminology_line_domains(original, &model_text, &placeholders, line_separators);
            let mut natural_texts = Vec::new();
            for domain in domains {
                let domain_placeholders = placeholders
                    .iter()
                    .filter(|placeholder| domain.contains(placeholder.token()))
                    .cloned()
                    .collect::<Vec<_>>();
                let projected = project_protected_text(domain, &domain_placeholders)
                    .map_err(ResolvedTranslationSemanticError::ProjectLanguageText)?;
                natural_texts.extend(projected.segments().iter().filter_map(
                    |segment| match segment {
                        LanguageTextSegment::NaturalText(text) => Some(text.clone()),
                        LanguageTextSegment::OpaqueBoundary => None,
                    },
                ));
            }
            self.terminology
                .triggered_indices(natural_texts.iter().map(String::as_str))
        };
        let terms = term_indices
            .iter()
            .copied()
            .map(|index| self.terminology.entries()[index].dependency())
            .collect();
        Ok(PreparedTranslationText {
            status,
            model_text,
            terms,
            term_indices,
            placeholders,
            language_analysis,
            source_language: Arc::clone(&self.source_language),
        })
    }
}

fn natural_segments(language_text: &crate::language::LanguageText) -> impl Iterator<Item = &str> {
    language_text
        .segments()
        .iter()
        .filter_map(|segment| match segment {
            LanguageTextSegment::NaturalText(text) => Some(text.as_str()),
            LanguageTextSegment::OpaqueBoundary => None,
        })
}

fn line_separator_offsets(lines: &[String]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(lines.len().saturating_sub(1));
    let mut cursor = 0;
    for line in lines.iter().take(lines.len().saturating_sub(1)) {
        cursor += line.len();
        offsets.push(cursor);
        cursor += 1;
    }
    offsets
}

/// 把没有被 Placeholder 吞入 opaque span 的 Lines 分隔 LF 映射到模型文本，并据此
/// 切开术语扫描域。若分隔 LF 位于 opaque span 内，现有 OpaqueBoundary 已经阻止跨界，
/// 无需再制造一个模型侧位置。
fn terminology_line_domains<'a>(
    original: &str,
    model_text: &'a str,
    placeholders: &[AppliedPlaceholder],
    line_separators: &[usize],
) -> Vec<&'a str> {
    let mut mapped = Vec::with_capacity(line_separators.len());
    let mut separator_index = 0;
    let mut source_cursor = 0;
    let mut model_cursor = 0;

    for placeholder in placeholders {
        let token_offset = model_text[model_cursor..]
            .find(placeholder.token())
            .expect("Placeholder 投影已经保证每个 token 在模型文本中恰好出现一次");
        let token_start = model_cursor + token_offset;
        let source_span_start = source_cursor + token_offset;
        let source_span_end = source_span_start + placeholder.original().len();

        debug_assert_eq!(
            &original[source_cursor..source_span_start],
            &model_text[model_cursor..token_start],
            "Placeholder 之前的自然文本必须逐字保持"
        );
        debug_assert_eq!(
            &original[source_span_start..source_span_end],
            placeholder.original(),
            "Placeholder 绑定必须对应原文中的当前源跨度"
        );

        while line_separators
            .get(separator_index)
            .is_some_and(|separator| *separator < source_span_start)
        {
            let separator = line_separators[separator_index];
            mapped.push(model_cursor + separator - source_cursor);
            separator_index += 1;
        }
        while line_separators
            .get(separator_index)
            .is_some_and(|separator| *separator < source_span_end)
        {
            debug_assert!(line_separators[separator_index] >= source_span_start);
            separator_index += 1;
        }

        source_cursor = source_span_end;
        model_cursor = token_start + placeholder.token().len();
    }

    for &separator in &line_separators[separator_index..] {
        mapped.push(model_cursor + separator - source_cursor);
    }

    let mut domains = Vec::with_capacity(mapped.len() + 1);
    let mut start = 0;
    for separator in mapped {
        debug_assert_eq!(model_text.as_bytes().get(separator), Some(&b'\n'));
        domains.push(&model_text[start..separator]);
        start = separator + 1;
    }
    domains.push(&model_text[start..]);
    domains
}

/// 当前单段文本相对于源语言与保护规则的处理状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PreparedTranslationStatus {
    Active,
    NonSourceLanguage,
    FullyProtected,
}

impl PreparedTranslationStatus {
    pub(crate) const fn storage_name(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::NonSourceLanguage => "non_source_language",
            Self::FullyProtected => "fully_protected",
        }
    }
}

/// `prepare` 建立的不可伪造、可跨 worker 持有的单段文本验收句柄。
#[derive(Clone)]
pub(crate) struct PreparedTranslationText {
    status: PreparedTranslationStatus,
    model_text: String,
    terms: Vec<TerminologyDependency>,
    term_indices: Vec<usize>,
    placeholders: Vec<AppliedPlaceholder>,
    language_analysis: LanguageAnalysis,
    source_language: Arc<dyn LanguageModule>,
}

impl PreparedTranslationText {
    pub(crate) const fn status(&self) -> PreparedTranslationStatus {
        self.status
    }

    pub(crate) fn model_text(&self) -> &str {
        &self.model_text
    }

    pub(crate) fn terms(&self) -> &[TerminologyDependency] {
        &self.terms
    }

    pub(crate) fn term_indices(&self) -> &[usize] {
        &self.term_indices
    }

    pub(crate) fn placeholders(&self) -> &[AppliedPlaceholder] {
        &self.placeholders
    }

    pub(super) fn language_analysis(&self) -> &LanguageAnalysis {
        &self.language_analysis
    }

    pub(crate) fn accept(
        &self,
        candidate: impl Into<String>,
    ) -> Result<PreparedTranslationAcceptance, ResolvedTranslationSemanticError> {
        if self.status != PreparedTranslationStatus::Active {
            return Ok(PreparedTranslationAcceptance::Rejected(
                PreparedTranslationRejection::NotActive(self.status),
            ));
        }
        accept_prepared_translation_candidate(
            candidate.into(),
            &self.placeholders,
            &self.language_analysis,
            self.source_language.as_ref(),
        )
        .map_err(ResolvedTranslationSemanticError::AcceptCandidate)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PreparedTranslationAcceptance {
    Accepted(String),
    Rejected(PreparedTranslationRejection),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PreparedTranslationRejection {
    NotActive(PreparedTranslationStatus),
    Candidate(TranslationUnitRejectionReason),
}

impl fmt::Display for PreparedTranslationRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotActive(status) => write!(
                formatter,
                "文本状态 {} 不接受候选译文",
                status.storage_name()
            ),
            Self::Candidate(reason) => write!(formatter, "{reason:?}"),
        }
    }
}

#[derive(Debug)]
pub(crate) enum ResolvedTranslationSemanticError {
    ProtectPlaceholder(PlaceholderProtectionError),
    ProjectLanguageText(LanguageTextProjectionError),
    AcceptCandidate(super::executor::TranslationCandidateTechnicalError),
}

impl fmt::Display for ResolvedTranslationSemanticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProtectPlaceholder(source) => write!(formatter, "无法保护占位符：{source}"),
            Self::ProjectLanguageText(source) => write!(formatter, "无法建立语言视图：{source}"),
            Self::AcceptCandidate(source) => write!(formatter, "无法验收候选译文：{source}"),
        }
    }
}

impl Error for ResolvedTranslationSemanticError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ProtectPlaceholder(source) => Some(source),
            Self::ProjectLanguageText(source) => Some(source),
            Self::AcceptCandidate(source) => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use super::*;
    use crate::language::{
        JapaneseLanguageModule, JapaneseResidualPolicy, LanguageId, LanguagePair,
    };
    use crate::rpg_maker::translate::placeholder::PlaceholderRuleDefinition;
    use crate::rpg_maker::translate::planning_resource::{TerminologyEntry, compile_terminology};

    fn semantics_with(
        engine: RpgMakerEngine,
        terms: Vec<TerminologyEntry>,
        placeholders: Vec<PlaceholderRuleDefinition>,
    ) -> ResolvedTranslationSemantics {
        let placeholder_service = Pcre2PlaceholderService::new().expect("内建占位符应可编译");
        let custom_placeholders = placeholder_service
            .compile_custom(placeholders)
            .expect("测试占位符应可编译");
        let source_language = Arc::new(JapaneseLanguageModule::new(
            JapaneseResidualPolicy::new(NonZeroUsize::MIN, Vec::new()).expect("测试残留策略应有效"),
            None,
        ));
        ResolvedTranslationSemantics::new(
            engine,
            "test system".to_owned(),
            LanguagePair::new(
                LanguageId::parse("ja").expect("源语言应有效"),
                LanguageId::parse("zh-Hans").expect("目标语言应有效"),
            ),
            Arc::new(compile_terminology(terms).expect("测试术语应可编译")),
            placeholder_service,
            custom_placeholders,
            source_language,
            Sha256Fingerprint::from_bytes([0x3c; 32]),
        )
    }

    #[test]
    fn terminology_matches_each_natural_segment_without_scanning_or_crossing_opaque_shells() {
        let semantics = semantics_with(
            RpgMakerEngine::Mz,
            vec![
                TerminologyEntry::new("勇者", "英雄", vec!["勇者".to_owned()]),
                TerminologyEntry::new("前後", "前后", vec!["前後".to_owned()]),
            ],
            vec![PlaceholderRuleDefinition::new(None, r"<code:[^>]+>")],
        );

        let hidden = semantics
            .prepare(TextGroupKind::PluginParameter, r"<code:勇者>前\C[2]後翻訳")
            .expect("混合协议文本应可准备");
        assert!(hidden.terms().is_empty());

        let visible = semantics
            .prepare(TextGroupKind::PluginParameter, r"<code:x>勇者")
            .expect("自然正文应可准备");
        assert_eq!(
            visible
                .terms()
                .iter()
                .map(TerminologyDependency::term)
                .collect::<Vec<_>>(),
            ["勇者"]
        );
        assert_eq!(visible.term_indices(), [0]);
    }

    #[test]
    fn terminology_keeps_lines_elements_separate_and_value_can_match_lf() {
        let semantics = semantics_with(
            RpgMakerEngine::Mz,
            vec![
                TerminologyEntry::new("跨元素", "不应命中", vec!["海へ\n出よう".to_owned()]),
                TerminologyEntry::new("标量换行", "应命中", vec!["鐘が\n鳴る".to_owned()]),
            ],
            Vec::new(),
        );

        let lines = semantics
            .prepare_content(
                TextGroupKind::EventDialogue,
                &TextUnitContent::Lines(vec![
                    "海へ".to_owned(),
                    "出よう".to_owned(),
                    "別の翻訳".to_owned(),
                ]),
            )
            .expect("Lines 术语边界应可准备");
        assert!(lines.terms().is_empty());
        assert!(lines.term_indices().is_empty());

        let value = semantics
            .prepare(TextGroupKind::DatabaseEntry, "鐘が\n鳴る翻訳")
            .expect("Value 内部 LF 是可达的标量内容");
        assert_eq!(
            value
                .terms()
                .iter()
                .map(TerminologyDependency::term)
                .collect::<Vec<_>>(),
            ["标量换行"]
        );
        assert_eq!(value.term_indices(), [1]);
    }

    #[test]
    fn scalar_accept_allows_lf_but_rejects_cr_nul_and_all_whitespace() {
        let prepared = ResolvedTranslationSemantics::for_test()
            .prepare(TextGroupKind::DatabaseEntry, "翻訳")
            .expect("日文原文应可准备");

        assert_eq!(
            prepared.accept("译文\n第二行").expect("LF 候选应可验收"),
            PreparedTranslationAcceptance::Accepted("译文\n第二行".to_owned())
        );
        for invalid in ["译文\r第二行", "译文\0第二行"] {
            assert!(matches!(
                prepared.accept(invalid).expect("非法标量应返回普通拒绝"),
                PreparedTranslationAcceptance::Rejected(PreparedTranslationRejection::Candidate(
                    TranslationUnitRejectionReason::InvalidLineText { .. }
                ))
            ));
        }
        assert!(matches!(
            prepared.accept(" \n\t ").expect("全空白候选应返回普通拒绝"),
            PreparedTranslationAcceptance::Rejected(PreparedTranslationRejection::Candidate(
                TranslationUnitRejectionReason::BlankTranslation
            ))
        ));
    }

    #[test]
    fn new_candidate_keeps_strict_ambiguity_for_repeated_original_placeholders() {
        let prepared = ResolvedTranslationSemantics::for_test()
            .prepare(TextGroupKind::EventDialogue, r"\C[2]翻訳\C[2]")
            .expect("重复控制符原文应可准备");

        assert!(matches!(
            prepared
                .accept(r"\C[2]译文\C[2]")
                .expect("占位符歧义应是普通拒绝"),
            PreparedTranslationAcceptance::Rejected(PreparedTranslationRejection::Candidate(
                TranslationUnitRejectionReason::PlaceholderNormalizationAmbiguous { .. }
            ))
        ));
    }
}
