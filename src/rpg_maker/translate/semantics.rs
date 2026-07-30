//! RPG Maker 翻译规划和结果验收共同使用的一次语义快照。

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::language::{LanguageAnalysis, LanguageModule, LanguagePair, LanguageTextSegment};
use crate::rpg_maker::RpgMakerEngine;
use crate::rpg_maker::location_codec::{
    RpgMakerLocationCodec, RpgMakerLocationCodecError, RpgMakerProjectionCodec,
    RpgMakerProjectionCodecError,
};
use crate::rpg_maker::model::TextUnitContent;
use crate::rpg_maker::text::TextGroupKind;
use crate::translation::placeholder_projection::{
    LanguageTextProjectionError, project_protected_text,
};

#[cfg(test)]
use super::executor::accept_prepared_translation_candidate;
#[cfg(test)]
use super::pipeline::TranslationUnitRejectionReason;
use super::pipeline::{AppliedPlaceholder, TerminologyDependency, TranslationUnitIdentity};
use super::placeholder::{
    CompiledPlaceholderRules, Pcre2PlaceholderService, PlaceholderProtectionError,
};
use crate::translation::planning_resource::CompiledTerminology;

/// 一轮 RPG Maker 翻译共享且不可变的当前语义。
pub(crate) struct ResolvedTranslationSemantics {
    engine: RpgMakerEngine,
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
        language_pair: LanguagePair,
        terminology: Arc<CompiledTerminology>,
        placeholder_service: Pcre2PlaceholderService,
        custom_placeholders: CompiledPlaceholderRules,
        source_language: Arc<dyn LanguageModule>,
        global_fingerprint: Sha256Fingerprint,
    ) -> Self {
        Self {
            engine,
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
        Self::for_test_with_placeholders(Vec::new())
    }

    #[cfg(test)]
    pub(crate) fn for_test_with_placeholders(
        definitions: Vec<super::placeholder::PlaceholderRuleDefinition>,
    ) -> Self {
        use std::num::NonZeroUsize;

        use crate::language::{
            JapaneseLanguageModule, JapaneseResidualPolicy, LanguageId, LanguagePair,
        };

        let placeholder_service = Pcre2PlaceholderService::new().expect("测试内建占位符应可编译");
        let custom_placeholders = placeholder_service
            .compile_custom(definitions)
            .expect("测试占位符集应可编译");
        let source_language = Arc::new(JapaneseLanguageModule::new(
            JapaneseResidualPolicy::new(NonZeroUsize::new(1).expect("常量非零"), Vec::new())
                .expect("测试日文残留策略应有效"),
            None,
        ));
        Self::new(
            RpgMakerEngine::Mz,
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

    /// RPG Maker 的 `Lines` 保持完整原文参与 Placeholder 与语言分析，但 opaque 保护和
    /// 术语都不能跨越两个物理数组元素。`Value` 没有这层边界，其中的 LF 仍是普通内容。
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
            .protect_with_line_boundaries(
                self.engine,
                kind,
                original,
                line_separators,
                &self.custom_placeholders,
            )
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
            .map(|index| {
                let entry = &self.terminology.entries()[index];
                TerminologyDependency::new(entry.term(), entry.translation())
            })
            .collect();
        Ok(PreparedTranslationText {
            status,
            model_text,
            terms,
            term_indices,
            placeholders,
            language_analysis,
            #[cfg(test)]
            source_language: Arc::clone(&self.source_language),
        })
    }
}

/// 建立人工译文的稳定语义状态。
///
/// 该状态只绑定会决定人工译文是否仍适用于当前 Unit 的事实。Prompt、Profile、
/// Client、术语和译文正文不参与；因此对已有人工译文作精确修订不会使它失去
/// Current，调整自动翻译配置也不会清除人工修订。
pub(crate) fn manual_translation_state_fingerprint(
    engine: RpgMakerEngine,
    language_pair: &LanguagePair,
    identity: &TranslationUnitIdentity,
    placeholders: &[AppliedPlaceholder],
) -> Result<Sha256Fingerprint, ManualTranslationStateError> {
    let group_location = RpgMakerLocationCodec::encode(identity.group_location())
        .map_err(ManualTranslationStateError::EncodeLocation)?;
    let unit_role = RpgMakerProjectionCodec::encode_role(identity.role())
        .map_err(ManualTranslationStateError::EncodeRole)?;
    let mut hasher = Sha256FramedHasher::new(b"att.rpg_maker.translation-state.manual");
    hasher
        .frame(1, engine.storage_name().as_bytes())
        .frame(2, language_pair.source().as_str().as_bytes())
        .frame(3, language_pair.target().as_str().as_bytes())
        .frame(4, identity.owner().storage_name().as_bytes())
        .frame(5, identity.kind().storage_name().as_bytes())
        .frame(6, group_location.as_bytes())
        .frame(7, unit_role.as_bytes())
        .frame(8, identity.source_context_json().as_bytes());
    match identity.source_content() {
        TextUnitContent::Value(value) => {
            hasher.frame(9, b"value").frame(10, value.as_bytes());
        }
        TextUnitContent::Lines(lines) => {
            let count = u64::try_from(lines.len())
                .expect("RPG Maker Unit 行数必须能表示为 u64")
                .to_le_bytes();
            hasher.frame(9, b"lines").frame(10, &count);
            for line in lines {
                hasher.frame(11, line.as_bytes());
            }
        }
    }
    for placeholder in placeholders {
        let origin = match placeholder.origin() {
            super::pipeline::PlaceholderRuleOrigin::BuiltIn => b"builtin".as_slice(),
            super::pipeline::PlaceholderRuleOrigin::Custom => b"custom".as_slice(),
        };
        let segment = match placeholder.segment() {
            super::pipeline::PlaceholderSegment::Whole => b"whole".as_slice(),
            super::pipeline::PlaceholderSegment::Begin => b"begin".as_slice(),
            super::pipeline::PlaceholderSegment::End => b"end".as_slice(),
        };
        hasher
            .frame(20, placeholder.token().as_bytes())
            .frame(21, placeholder.original().as_bytes())
            .frame(22, origin)
            .frame(23, placeholder.label().as_bytes())
            .frame(24, placeholder.scope().as_bytes())
            .frame(25, segment);
    }
    Ok(hasher.finish())
}

/// 人工译文状态无法编码受信 Unit 身份。
#[derive(Debug)]
pub(crate) enum ManualTranslationStateError {
    EncodeLocation(RpgMakerLocationCodecError),
    EncodeRole(RpgMakerProjectionCodecError),
}

impl fmt::Display for ManualTranslationStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EncodeLocation(source) => {
                write!(formatter, "无法编码人工译文状态位置：{source}")
            }
            Self::EncodeRole(source) => write!(formatter, "无法编码人工译文状态角色：{source}"),
        }
    }
}

impl Error for ManualTranslationStateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::EncodeLocation(source) => Some(source),
            Self::EncodeRole(source) => Some(source),
        }
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

/// 把 Lines 分隔 LF 映射到 Placeholder 投影后的模型文本，并据此切开术语扫描域。
///
/// 译前保护边界已经保证不透明跨度不会吞掉这些分隔符。
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
        debug_assert!(
            line_separators
                .get(separator_index)
                .is_none_or(|separator| *separator >= source_span_end),
            "不透明 Placeholder 跨越 Lines 槽边界必须在保护阶段被拒绝"
        );

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
    #[cfg(test)]
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
    #[cfg(test)]
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

    #[cfg(test)]
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

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PreparedTranslationAcceptance {
    Accepted(String),
    Rejected(PreparedTranslationRejection),
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PreparedTranslationRejection {
    NotActive(PreparedTranslationStatus),
    Candidate(TranslationUnitRejectionReason),
}

#[cfg(test)]
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
    #[cfg(test)]
    AcceptCandidate(super::executor::TranslationCandidateTechnicalError),
}

impl fmt::Display for ResolvedTranslationSemanticError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProtectPlaceholder(source) => write!(formatter, "无法保护占位符：{source}"),
            Self::ProjectLanguageText(source) => write!(formatter, "无法建立语言视图：{source}"),
            #[cfg(test)]
            Self::AcceptCandidate(source) => write!(formatter, "无法验收候选译文：{source}"),
        }
    }
}

impl Error for ResolvedTranslationSemanticError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ProtectPlaceholder(source) => Some(source),
            Self::ProjectLanguageText(source) => Some(source),
            #[cfg(test)]
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
    use crate::rpg_maker::asset::RpgMakerAssetOwner;
    use crate::rpg_maker::model::{ScalarFieldKey, TextUnitRole};
    use crate::rpg_maker::text::{
        RpgMakerLocation, RpgMakerLocationStep, RpgMakerSource, StandardDataFile,
    };
    use crate::rpg_maker::translate::placeholder::PlaceholderRuleDefinition;
    use crate::translation::planning_resource::{TerminologyEntry, compile_terminology};

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
    fn manual_state_binds_unit_language_context_and_actual_placeholders_only() {
        let semantics = semantics_with(
            RpgMakerEngine::Mz,
            vec![TerminologyEntry::new(
                "勇者",
                "英雄",
                vec!["勇者".to_owned()],
            )],
            Vec::new(),
        );
        let identity = |context: &str, source: &str| {
            TranslationUnitIdentity::new(
                RpgMakerAssetOwner::Builtin,
                TextGroupKind::DatabaseEntry,
                RpgMakerLocation::value(
                    RpgMakerSource::data(StandardDataFile::Actors),
                    vec![RpgMakerLocationStep::index(1)],
                ),
                TextUnitRole::Scalar(ScalarFieldKey::new("name").expect("字段键应有效")),
                TextUnitContent::Value(source.to_owned()),
                context,
            )
        };
        let base_identity = identity("{}", r"\V[1]勇者");
        let prepared = semantics
            .prepare_content(base_identity.kind(), base_identity.source_content())
            .expect("应准备包含内置控制符的原文");
        let base = manual_translation_state_fingerprint(
            semantics.engine(),
            semantics.language_pair(),
            &base_identity,
            prepared.placeholders(),
        )
        .expect("应建立人工状态");

        let changed_context = identity(r#"{"speaker":"actor"}"#, r"\V[1]勇者");
        assert_ne!(
            base,
            manual_translation_state_fingerprint(
                semantics.engine(),
                semantics.language_pair(),
                &changed_context,
                prepared.placeholders(),
            )
            .expect("应建立上下文变化后的人工状态")
        );
        let changed_source = identity("{}", r"\V[2]勇者");
        let changed_prepared = semantics
            .prepare_content(changed_source.kind(), changed_source.source_content())
            .expect("应准备变化后的控制符");
        assert_ne!(
            base,
            manual_translation_state_fingerprint(
                semantics.engine(),
                semantics.language_pair(),
                &changed_source,
                changed_prepared.placeholders(),
            )
            .expect("应建立 Placeholder 变化后的人工状态")
        );
        let changed_language = LanguagePair::new(
            LanguageId::parse("ja").expect("来源语言应有效"),
            LanguageId::parse("en").expect("目标语言应有效"),
        );
        assert_ne!(
            base,
            manual_translation_state_fingerprint(
                semantics.engine(),
                &changed_language,
                &base_identity,
                prepared.placeholders(),
            )
            .expect("应建立语言变化后的人工状态")
        );
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
    fn opaque_placeholder_cannot_consume_a_lines_slot_separator() {
        let semantics = semantics_with(
            RpgMakerEngine::Mz,
            Vec::new(),
            vec![PlaceholderRuleDefinition::new(
                None,
                r"(?s)<opaque>.*?</opaque>",
            )],
        );
        let content = TextUnitContent::Lines(vec![
            "翻訳<opaque>前半".to_owned(),
            "後半</opaque>続き".to_owned(),
        ]);

        for kind in [
            TextGroupKind::EventDialogue,
            TextGroupKind::EventChoices,
            TextGroupKind::EventScrollingText,
        ] {
            let error = match semantics.prepare_content(kind, &content) {
                Ok(_) => panic!("不透明保护跨度不得吞掉两个 Lines 槽之间的 LF"),
                Err(error) => error,
            };
            assert!(matches!(
                error,
                ResolvedTranslationSemanticError::ProtectPlaceholder(
                    PlaceholderProtectionError::CrossesLineBoundary {
                        rule_number: Some(1),
                        source_line_index: 0,
                    }
                )
            ));
        }
        assert!(
            semantics
                .prepare(
                    TextGroupKind::EventChoices,
                    "翻訳<opaque>前半\n後半</opaque>続き"
                )
                .is_ok(),
            "Value 内的 LF 不是槽边界，允许同一规则保护"
        );
    }

    #[test]
    fn structured_placeholder_can_wrap_lines_when_the_separator_stays_natural_text() {
        let semantics = semantics_with(
            RpgMakerEngine::Mz,
            Vec::new(),
            vec![PlaceholderRuleDefinition::new(
                Some(vec!["event_choices".to_owned()]),
                r"(?s)<msg>(?<text>.*?)</msg>",
            )],
        );
        let content = TextUnitContent::Lines(vec!["<msg>翻訳".to_owned(), "続き</msg>".to_owned()]);

        let prepared = semantics
            .prepare_content(TextGroupKind::EventChoices, &content)
            .expect("LF 位于 text 捕获中时，结构化外壳本身没有跨越槽边界");

        assert!(prepared.model_text().contains("翻訳\n続き"));
        assert_eq!(
            prepared
                .placeholders()
                .iter()
                .map(AppliedPlaceholder::original)
                .collect::<Vec<_>>(),
            ["<msg>", "</msg>"]
        );
    }

    #[test]
    fn help_wrapper_protects_shell_without_splitting_the_complete_value_unit() {
        let semantics = semantics_with(
            RpgMakerEngine::Mz,
            Vec::new(),
            vec![PlaceholderRuleDefinition::new(
                Some(vec!["database_entry".to_owned()]),
                r"\A<Help:(?<text>.*?)>\z",
            )],
        );
        let original = "<Help:炎の剣の説明>";

        let prepared = semantics
            .prepare(TextGroupKind::DatabaseEntry, original)
            .expect("完整 Value 应在形成 Unit 后投影 Placeholder");

        assert!(prepared.model_text().contains("炎の剣の説明"));
        assert_eq!(
            prepared
                .placeholders()
                .iter()
                .map(AppliedPlaceholder::original)
                .collect::<Vec<_>>(),
            ["<Help:", ">"]
        );
        let candidate = prepared
            .model_text()
            .replace("炎の剣の説明", "炎之剑<说明>追加");
        assert_eq!(
            prepared
                .accept(&candidate)
                .expect("正文中的裸尖括号不应被猜成 opaque 外壳"),
            PreparedTranslationAcceptance::Accepted("<Help:炎之剑<说明>追加>".to_owned())
        );
        assert_eq!(
            prepared
                .accept("<Help:炎之剑的说明>")
                .expect("候选译文应能用唯一原片段恢复 Custom 外壳"),
            PreparedTranslationAcceptance::Accepted("<Help:炎之剑的说明>".to_owned())
        );

        let other_kind = semantics
            .prepare(TextGroupKind::Map, original)
            .expect("异 kind 不应消费 database_entry Placeholder");
        assert_eq!(other_kind.model_text(), original);
        assert!(other_kind.placeholders().is_empty());
    }

    #[test]
    fn custom_shell_candidate_rejects_repeated_missing_bindings_as_ambiguous() {
        let semantics = semantics_with(
            RpgMakerEngine::Mz,
            Vec::new(),
            vec![PlaceholderRuleDefinition::new(
                Some(vec!["database_entry".to_owned()]),
                r"<x>(?<text>.*?)</x>",
            )],
        );
        let prepared = semantics
            .prepare(TextGroupKind::DatabaseEntry, "<x>一つ目</x><x>二つ目</x>")
            .expect("两个结构化壳应建立四个独立 Custom 绑定");

        assert!(matches!(
            prepared
                .accept("<x>第一项</x><x>第二项</x>")
                .expect("无法唯一归位应是普通候选拒绝"),
            PreparedTranslationAcceptance::Rejected(PreparedTranslationRejection::Candidate(
                TranslationUnitRejectionReason::PlaceholderNormalizationAmbiguous { .. }
            ))
        ));
    }

    #[test]
    fn custom_literal_normalization_uses_the_frozen_candidate_not_inserted_tokens() {
        let scope = Some(vec!["database_entry".to_owned()]);
        let semantics = semantics_with(
            RpgMakerEngine::Mz,
            Vec::new(),
            vec![
                PlaceholderRuleDefinition::new(scope.clone(), r"<x>"),
                PlaceholderRuleDefinition::new(scope, r"ATT"),
            ],
        );
        let prepared = semantics
            .prepare(TextGroupKind::DatabaseEntry, "<x>翻訳ATT")
            .expect("两个互不重叠的 Custom 原片段应可保护");

        for candidate in [
            "<x>译文ATT".to_owned(),
            prepared.model_text().replace("翻訳", "译文").replace(
                prepared
                    .placeholders()
                    .iter()
                    .find(|placeholder| placeholder.original() == "ATT")
                    .expect("ATT 绑定应存在")
                    .token(),
                "ATT",
            ),
        ] {
            assert_eq!(
                prepared
                    .accept(candidate)
                    .expect("原片段扫描不得进入既有或刚插入的 ATT token"),
                PreparedTranslationAcceptance::Accepted("<x>译文ATT".to_owned())
            );
        }
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
        assert_eq!(
            prepared
                .accept("译文>仍是普通标量")
                .expect("通用准备接口不应猜测写回目标"),
            PreparedTranslationAcceptance::Accepted("译文>仍是普通标量".to_owned())
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
    fn candidate_keeps_strict_ambiguity_for_repeated_original_placeholders() {
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

    #[test]
    fn candidate_rejects_extra_builtin_control_when_all_tokens_are_present() {
        // 重复 original 的 token 全部在场时，多抄的一份原始控制片段同样无法
        // 唯一归位；混排必须与单绑定分支一致拒绝，不得作为字面文本混入译文。
        let prepared = ResolvedTranslationSemantics::for_test()
            .prepare(TextGroupKind::EventDialogue, r"\C[2]翻訳\C[2]")
            .expect("重复控制符原文应可准备");
        let tokens = prepared
            .placeholders()
            .iter()
            .map(AppliedPlaceholder::token)
            .collect::<Vec<_>>();
        let candidate = format!(r"{}译文{}\C[2]", tokens[0], tokens[1]);

        assert!(matches!(
            prepared.accept(candidate).expect("混排应是普通拒绝"),
            PreparedTranslationAcceptance::Rejected(PreparedTranslationRejection::Candidate(
                TranslationUnitRejectionReason::PlaceholderNormalizationAmbiguous { .. }
            ))
        ));
    }
}
