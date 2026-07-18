//! 跨游戏引擎共享的语言分析、源文残留检查与可选安全修复。
//!
//! 本模块只理解自然语言文本和不透明边界，不依赖游戏引擎位置、数据库、CLI、
//! 占位符协议、LLM 或运行时根能力。

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;

use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};

/// 语言模块能够观察的文本片段。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LanguageTextSegment {
    NaturalText(String),
    OpaqueBoundary,
}

/// 普通文本与调用方保护区共同组成的引擎无关语言视图。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LanguageText {
    segments: Vec<LanguageTextSegment>,
}

impl LanguageText {
    pub(crate) fn new(segments: Vec<LanguageTextSegment>) -> Self {
        let mut normalized = Vec::with_capacity(segments.len());
        for segment in segments {
            match segment {
                LanguageTextSegment::NaturalText(text) if text.is_empty() => {}
                LanguageTextSegment::NaturalText(text) => {
                    if let Some(LanguageTextSegment::NaturalText(previous)) = normalized.last_mut()
                    {
                        previous.push_str(&text);
                    } else {
                        normalized.push(LanguageTextSegment::NaturalText(text));
                    }
                }
                LanguageTextSegment::OpaqueBoundary => {
                    normalized.push(LanguageTextSegment::OpaqueBoundary);
                }
            }
        }
        Self {
            segments: normalized,
        }
    }

    #[cfg(test)]
    pub(crate) fn natural(text: impl Into<String>) -> Self {
        Self::new(vec![LanguageTextSegment::NaturalText(text.into())])
    }

    pub(crate) fn segments(&self) -> &[LanguageTextSegment] {
        &self.segments
    }

    pub(crate) fn has_non_whitespace_natural_text(&self) -> bool {
        self.segments.iter().any(|segment| match segment {
            LanguageTextSegment::NaturalText(text) => !text.trim().is_empty(),
            LanguageTextSegment::OpaqueBoundary => false,
        })
    }

    /// 验证并应用语言模块给出的字符级修复。
    pub(crate) fn apply_repair(
        &self,
        plan: &LanguageRepairPlan,
    ) -> Result<Self, LanguageRepairApplicationError> {
        let mut segments = self.segments.clone();
        let mut by_segment = BTreeMap::<usize, Vec<&LanguageCharacterReplacement>>::new();
        for replacement in plan.replacements() {
            by_segment
                .entry(replacement.segment_index())
                .or_default()
                .push(replacement);
        }

        for (segment_index, mut replacements) in by_segment {
            let Some(LanguageTextSegment::NaturalText(text)) = segments.get_mut(segment_index)
            else {
                return Err(LanguageRepairApplicationError::InvalidNaturalSegment {
                    segment_index,
                });
            };
            replacements.sort_by_key(|replacement| replacement.byte_offset());
            for pair in replacements.windows(2) {
                if pair[0].byte_offset() == pair[1].byte_offset() {
                    return Err(LanguageRepairApplicationError::DuplicatePosition {
                        segment_index,
                        byte_offset: pair[0].byte_offset(),
                    });
                }
            }
            for replacement in replacements.into_iter().rev() {
                let byte_offset = replacement.byte_offset();
                if !text.is_char_boundary(byte_offset) {
                    return Err(LanguageRepairApplicationError::InvalidCharacterBoundary {
                        segment_index,
                        byte_offset,
                    });
                }
                let Some(actual) = text[byte_offset..].chars().next() else {
                    return Err(LanguageRepairApplicationError::MissingCharacter {
                        segment_index,
                        byte_offset,
                    });
                };
                if actual != replacement.expected() {
                    return Err(LanguageRepairApplicationError::UnexpectedCharacter {
                        segment_index,
                        byte_offset,
                        expected: replacement.expected(),
                        actual,
                    });
                }
                let end = byte_offset + actual.len_utf8();
                text.replace_range(byte_offset..end, &replacement.replacement().to_string());
            }
        }
        Ok(Self::new(segments))
    }
}

/// 自然文本中的一个字符替换位置。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LanguageCharacterReplacement {
    segment_index: usize,
    byte_offset: usize,
    expected: char,
    replacement: char,
}

impl LanguageCharacterReplacement {
    fn new(segment_index: usize, byte_offset: usize, expected: char, replacement: char) -> Self {
        Self {
            segment_index,
            byte_offset,
            expected,
            replacement,
        }
    }

    pub(crate) const fn segment_index(&self) -> usize {
        self.segment_index
    }

    pub(crate) const fn byte_offset(&self) -> usize {
        self.byte_offset
    }

    pub(crate) const fn expected(&self) -> char {
        self.expected
    }

    pub(crate) const fn replacement(&self) -> char {
        self.replacement
    }
}

/// 可选阅读风格修复；空计划表示无需修改或无法唯一证明修改安全。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LanguageRepairPlan {
    replacements: Vec<LanguageCharacterReplacement>,
}

impl LanguageRepairPlan {
    pub(crate) const fn unchanged() -> Self {
        Self {
            replacements: Vec::new(),
        }
    }

    fn replacing(replacements: Vec<LanguageCharacterReplacement>) -> Self {
        Self { replacements }
    }

    pub(crate) fn replacements(&self) -> &[LanguageCharacterReplacement] {
        &self.replacements
    }

    #[cfg(test)]
    pub(crate) fn is_unchanged(&self) -> bool {
        self.replacements.is_empty()
    }
}

/// 调用方提交了无法安全应用的修复计划。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LanguageRepairApplicationError {
    InvalidNaturalSegment {
        segment_index: usize,
    },
    DuplicatePosition {
        segment_index: usize,
        byte_offset: usize,
    },
    InvalidCharacterBoundary {
        segment_index: usize,
        byte_offset: usize,
    },
    MissingCharacter {
        segment_index: usize,
        byte_offset: usize,
    },
    UnexpectedCharacter {
        segment_index: usize,
        byte_offset: usize,
        expected: char,
        actual: char,
    },
}

impl fmt::Display for LanguageRepairApplicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidNaturalSegment { segment_index } => {
                write!(formatter, "修复位置指向非自然文本段 {segment_index}")
            }
            Self::DuplicatePosition {
                segment_index,
                byte_offset,
            } => write!(
                formatter,
                "自然文本段 {segment_index} 的字节 {byte_offset} 存在重复修复"
            ),
            Self::InvalidCharacterBoundary {
                segment_index,
                byte_offset,
            } => write!(
                formatter,
                "自然文本段 {segment_index} 的字节 {byte_offset} 不是字符边界"
            ),
            Self::MissingCharacter {
                segment_index,
                byte_offset,
            } => write!(
                formatter,
                "自然文本段 {segment_index} 的字节 {byte_offset} 没有字符"
            ),
            Self::UnexpectedCharacter {
                segment_index,
                byte_offset,
                expected,
                actual,
            } => write!(
                formatter,
                "自然文本段 {segment_index} 的字节 {byte_offset} 预期 {expected:?}，实际为 {actual:?}"
            ),
        }
    }
}

impl Error for LanguageRepairApplicationError {}

/// 一段足以说明译文仍残留源语言的真实连续文本。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LanguageResidual {
    fragment: String,
}

impl LanguageResidual {
    fn new(fragment: impl Into<String>) -> Self {
        Self {
            fragment: fragment.into(),
        }
    }

    pub(crate) fn fragment(&self) -> &str {
        &self.fragment
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LanguageModuleKind {
    Japanese,
    English,
}

impl LanguageModuleKind {
    const fn name(self) -> &'static str {
        match self {
            Self::Japanese => "JapaneseLanguageModule",
            Self::English => "EnglishLanguageModule",
        }
    }
}

/// 译前建立、随翻译单元进入译后的语言事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LanguageAnalysis {
    Japanese(JapaneseLanguageAnalysis),
    English(EnglishLanguageAnalysis),
}

impl LanguageAnalysis {
    pub(crate) const fn needs_translation(&self) -> bool {
        match self {
            Self::Japanese(analysis) => analysis.needs_translation,
            Self::English(analysis) => analysis.needs_translation,
        }
    }

    const fn kind(&self) -> LanguageModuleKind {
        match self {
            Self::Japanese(_) => LanguageModuleKind::Japanese,
            Self::English(_) => LanguageModuleKind::English,
        }
    }
}

/// 一个语言模块只拥有译前判断、源文残留和可选安全修复三项职责。
pub(crate) trait LanguageModule: Send + Sync {
    /// 返回仅由当前语言策略决定的稳定语义指纹。
    fn semantic_fingerprint(&self) -> Sha256Fingerprint;

    fn analyze_source(&self, text: &LanguageText) -> LanguageAnalysis;

    fn find_source_residual(
        &self,
        analysis: &LanguageAnalysis,
        translation: &LanguageText,
    ) -> Result<Option<LanguageResidual>, LanguageModuleError>;

    fn plan_translation_repair(
        &self,
        analysis: &LanguageAnalysis,
        translation: &LanguageText,
    ) -> Result<LanguageRepairPlan, LanguageModuleError>;
}

/// TaskBlock 的分析事实与当前精确源语言绑定不一致。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LanguageModuleError {
    expected: LanguageModuleKind,
    actual: LanguageModuleKind,
}

impl LanguageModuleError {
    fn analysis_mismatch(expected: LanguageModuleKind, analysis: &LanguageAnalysis) -> Self {
        Self {
            expected,
            actual: analysis.kind(),
        }
    }
}

impl fmt::Display for LanguageModuleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "语言分析属于 {}，当前模块是 {}",
            self.actual.name(),
            self.expected.name()
        )
    }
}

impl Error for LanguageModuleError {}

/// 精确源语言 ID 到语言模块的受信绑定集合。
#[derive(Clone)]
pub(crate) struct LanguageModuleCatalog {
    modules: BTreeMap<String, Arc<dyn LanguageModule>>,
}

impl LanguageModuleCatalog {
    pub(crate) fn new(
        bindings: impl IntoIterator<Item = (String, Arc<dyn LanguageModule>)>,
    ) -> Result<Self, LanguageModuleCatalogBuildError> {
        let mut modules = BTreeMap::new();
        for (language_id, module) in bindings {
            if language_id.trim().is_empty() {
                return Err(LanguageModuleCatalogBuildError::BlankLanguageId);
            }
            if language_id.trim() != language_id {
                return Err(
                    LanguageModuleCatalogBuildError::SurroundingWhitespaceInLanguageId {
                        language_id,
                    },
                );
            }
            if modules.insert(language_id.clone(), module).is_some() {
                return Err(LanguageModuleCatalogBuildError::DuplicateLanguageId { language_id });
            }
        }
        if modules.is_empty() {
            return Err(LanguageModuleCatalogBuildError::MissingLanguageModule);
        }
        Ok(Self { modules })
    }

    pub(crate) fn resolve(
        &self,
        language_id: &str,
    ) -> Result<Arc<dyn LanguageModule>, LanguageModuleCatalogError> {
        self.modules.get(language_id).cloned().ok_or_else(|| {
            LanguageModuleCatalogError::UnknownLanguageId {
                language_id: language_id.to_owned(),
                available_ids: self.modules.keys().cloned().collect(),
            }
        })
    }
}

impl fmt::Debug for LanguageModuleCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LanguageModuleCatalog")
            .field("language_ids", &self.modules.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LanguageModuleCatalogBuildError {
    MissingLanguageModule,
    BlankLanguageId,
    SurroundingWhitespaceInLanguageId { language_id: String },
    DuplicateLanguageId { language_id: String },
}

impl fmt::Display for LanguageModuleCatalogBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingLanguageModule => formatter.write_str("没有绑定任何源语言模块"),
            Self::BlankLanguageId => formatter.write_str("源语言 ID 不能为空白"),
            Self::SurroundingWhitespaceInLanguageId { language_id } => {
                write!(formatter, "源语言 ID 含首尾空白：{language_id:?}")
            }
            Self::DuplicateLanguageId { language_id } => {
                write!(formatter, "源语言 ID 重复：{language_id}")
            }
        }
    }
}

impl Error for LanguageModuleCatalogBuildError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LanguageModuleCatalogError {
    UnknownLanguageId {
        language_id: String,
        available_ids: Vec<String>,
    },
}

impl fmt::Display for LanguageModuleCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownLanguageId {
                language_id,
                available_ids,
            } => write!(
                formatter,
                "未知源语言 ID {language_id:?}；可用 ID：{}",
                available_ids.join(", ")
            ),
        }
    }
}

impl Error for LanguageModuleCatalogError {}

/// 日文残留判断的全部外部选择。
#[derive(Clone, Debug)]
pub(crate) struct JapaneseResidualPolicy {
    minimum_kana_characters: NonZeroUsize,
    allowed_terms: BTreeSet<String>,
}

impl JapaneseResidualPolicy {
    pub(crate) fn new(
        minimum_kana_characters: NonZeroUsize,
        allowed_terms: impl IntoIterator<Item = String>,
    ) -> Result<Self, LanguagePolicyConfigurationError> {
        Ok(Self {
            minimum_kana_characters,
            allowed_terms: collect_terms(allowed_terms, TermComparison::Exact)?,
        })
    }
}

/// 一个可以在译文中被识别的成对引号字符。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct QuotePair {
    opening: char,
    closing: char,
}

impl QuotePair {
    pub(crate) const fn new(opening: char, closing: char) -> Self {
        Self { opening, closing }
    }

    const fn opening(self) -> char {
        self.opening
    }

    const fn closing(self) -> char {
        self.closing
    }
}

const JAPANESE_QUOTE_PAIRS: [QuotePair; 2] =
    [QuotePair::new('「', '」'), QuotePair::new('『', '』')];

/// 日文引号安全修复所识别的外部候选对。
#[derive(Clone, Debug)]
pub(crate) struct JapaneseQuoteRepairPolicy {
    candidate_pairs: Vec<QuotePair>,
}

impl JapaneseQuoteRepairPolicy {
    pub(crate) fn new(
        candidate_pairs: Vec<QuotePair>,
    ) -> Result<Self, JapaneseQuoteRepairPolicyError> {
        if candidate_pairs.is_empty() {
            return Err(JapaneseQuoteRepairPolicyError::EmptyCandidatePairs);
        }
        let mut seen_pairs = BTreeSet::new();
        let mut used_characters = BTreeMap::<char, QuotePair>::new();
        for pair in JAPANESE_QUOTE_PAIRS
            .into_iter()
            .chain(candidate_pairs.iter().copied())
        {
            for character in [pair.opening(), pair.closing()] {
                if character.is_alphanumeric()
                    || character.is_whitespace()
                    || character.is_control()
                {
                    return Err(JapaneseQuoteRepairPolicyError::InvalidDelimiterCharacter {
                        character,
                    });
                }
            }
            if !seen_pairs.insert(pair) {
                return Err(JapaneseQuoteRepairPolicyError::DuplicatePair { pair });
            }
            for character in [pair.opening(), pair.closing()] {
                if let Some(existing) = used_characters.insert(character, pair)
                    && existing != pair
                {
                    return Err(JapaneseQuoteRepairPolicyError::AmbiguousCharacter {
                        character,
                        first: existing,
                        second: pair,
                    });
                }
            }
        }
        Ok(Self { candidate_pairs })
    }

    fn all_pairs(&self) -> Vec<QuotePair> {
        JAPANESE_QUOTE_PAIRS
            .into_iter()
            .chain(self.candidate_pairs.iter().copied())
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum JapaneseQuoteRepairPolicyError {
    EmptyCandidatePairs,
    InvalidDelimiterCharacter {
        character: char,
    },
    DuplicatePair {
        pair: QuotePair,
    },
    AmbiguousCharacter {
        character: char,
        first: QuotePair,
        second: QuotePair,
    },
}

impl fmt::Display for JapaneseQuoteRepairPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyCandidatePairs => formatter.write_str("日文引号修复候选对不能为空"),
            Self::InvalidDelimiterCharacter { character } => write!(
                formatter,
                "日文引号修复候选字符不能是字母、数字、空白或控制字符：{character:?}"
            ),
            Self::DuplicatePair { pair } => write!(
                formatter,
                "日文引号修复候选对重复：{:?}{:?}",
                pair.opening(),
                pair.closing()
            ),
            Self::AmbiguousCharacter {
                character,
                first,
                second,
            } => write!(
                formatter,
                "引号字符 {character:?} 同时属于 {:?}{:?} 和 {:?}{:?}",
                first.opening(),
                first.closing(),
                second.opening(),
                second.closing()
            ),
        }
    }
}

impl Error for JapaneseQuoteRepairPolicyError {}

/// 日文译前分析与译后修复实现。
#[derive(Clone, Debug)]
pub(crate) struct JapaneseLanguageModule {
    residual_policy: JapaneseResidualPolicy,
    quote_repair_policy: Option<JapaneseQuoteRepairPolicy>,
}

impl JapaneseLanguageModule {
    pub(crate) fn new(
        residual_policy: JapaneseResidualPolicy,
        quote_repair_policy: Option<JapaneseQuoteRepairPolicy>,
    ) -> Self {
        Self {
            residual_policy,
            quote_repair_policy,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct JapaneseLanguageAnalysis {
    needs_translation: bool,
    quote_structure: Option<Vec<JapaneseQuoteNode>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct JapaneseQuoteNode {
    pair: QuotePair,
    parent: Option<usize>,
}

impl LanguageModule for JapaneseLanguageModule {
    fn semantic_fingerprint(&self) -> Sha256Fingerprint {
        let mut hasher = Sha256FramedHasher::new(b"att.language.japanese");
        hasher.frame(
            1,
            &u64::try_from(self.residual_policy.minimum_kana_characters.get())
                .expect("x86_64 usize 必须可表示为 u64")
                .to_be_bytes(),
        );
        for term in &self.residual_policy.allowed_terms {
            hasher.frame(2, term.as_bytes());
        }
        match &self.quote_repair_policy {
            None => {
                hasher.frame(3, &[0]);
            }
            Some(policy) => {
                hasher.frame(3, &[1]);
                for pair in &policy.candidate_pairs {
                    let mut encoded = [0_u8; 8];
                    encoded[..4].copy_from_slice(&u32::from(pair.opening()).to_be_bytes());
                    encoded[4..].copy_from_slice(&u32::from(pair.closing()).to_be_bytes());
                    hasher.frame(4, &encoded);
                }
            }
        }
        hasher.finish()
    }

    fn analyze_source(&self, text: &LanguageText) -> LanguageAnalysis {
        let needs_translation = natural_texts(text)
            .flat_map(str::chars)
            .any(is_japanese_source_character);
        let quote_structure = parse_quote_structure(text, &JAPANESE_QUOTE_PAIRS).map(|nodes| {
            nodes
                .into_iter()
                .map(|node| JapaneseQuoteNode {
                    pair: node.pair,
                    parent: node.parent,
                })
                .collect()
        });
        LanguageAnalysis::Japanese(JapaneseLanguageAnalysis {
            needs_translation,
            quote_structure,
        })
    }

    fn find_source_residual(
        &self,
        analysis: &LanguageAnalysis,
        translation: &LanguageText,
    ) -> Result<Option<LanguageResidual>, LanguageModuleError> {
        let LanguageAnalysis::Japanese(_) = analysis else {
            return Err(LanguageModuleError::analysis_mismatch(
                LanguageModuleKind::Japanese,
                analysis,
            ));
        };
        for text in natural_texts(translation) {
            if let Some(fragment) = first_japanese_residual(
                text,
                self.residual_policy.minimum_kana_characters,
                &self.residual_policy.allowed_terms,
            ) {
                return Ok(Some(LanguageResidual::new(fragment)));
            }
        }
        Ok(None)
    }

    fn plan_translation_repair(
        &self,
        analysis: &LanguageAnalysis,
        translation: &LanguageText,
    ) -> Result<LanguageRepairPlan, LanguageModuleError> {
        let LanguageAnalysis::Japanese(analysis) = analysis else {
            return Err(LanguageModuleError::analysis_mismatch(
                LanguageModuleKind::Japanese,
                analysis,
            ));
        };
        let (Some(policy), Some(source_nodes)) =
            (&self.quote_repair_policy, &analysis.quote_structure)
        else {
            return Ok(LanguageRepairPlan::unchanged());
        };
        if source_nodes.is_empty() {
            return Ok(LanguageRepairPlan::unchanged());
        }
        let Some(target_nodes) =
            match_quote_structure(translation, &policy.all_pairs(), source_nodes)
        else {
            return Ok(LanguageRepairPlan::unchanged());
        };

        let mut replacements = Vec::new();
        for (source, target) in source_nodes.iter().zip(target_nodes) {
            if target.pair.opening() != source.pair.opening() {
                replacements.push(LanguageCharacterReplacement::new(
                    target.opening.segment_index,
                    target.opening.byte_offset,
                    target.pair.opening(),
                    source.pair.opening(),
                ));
            }
            if target.pair.closing() != source.pair.closing() {
                replacements.push(LanguageCharacterReplacement::new(
                    target.closing.segment_index,
                    target.closing.byte_offset,
                    target.pair.closing(),
                    source.pair.closing(),
                ));
            }
        }
        Ok(LanguageRepairPlan::replacing(replacements))
    }
}

/// 英文译前判定的外部选择。
#[derive(Clone, Debug)]
pub(crate) struct EnglishTranslationDetectionPolicy {
    minimum_word_count: NonZeroUsize,
    minimum_letter_count: NonZeroUsize,
    ignored_terms: BTreeSet<String>,
}

impl EnglishTranslationDetectionPolicy {
    pub(crate) fn new(
        minimum_word_count: NonZeroUsize,
        minimum_letter_count: NonZeroUsize,
        ignored_terms: impl IntoIterator<Item = String>,
    ) -> Result<Self, LanguagePolicyConfigurationError> {
        Ok(Self {
            minimum_word_count,
            minimum_letter_count,
            ignored_terms: collect_terms(ignored_terms, TermComparison::AsciiInsensitive)?,
        })
    }
}

/// 英文源文复制残留判断的外部选择。
#[derive(Clone, Debug)]
pub(crate) struct EnglishResidualPolicy {
    minimum_copied_word_count: NonZeroUsize,
    minimum_copied_letter_count: NonZeroUsize,
    allowed_terms: BTreeSet<String>,
}

impl EnglishResidualPolicy {
    pub(crate) fn new(
        minimum_copied_word_count: NonZeroUsize,
        minimum_copied_letter_count: NonZeroUsize,
        allowed_terms: impl IntoIterator<Item = String>,
    ) -> Result<Self, LanguagePolicyConfigurationError> {
        Ok(Self {
            minimum_copied_word_count,
            minimum_copied_letter_count,
            allowed_terms: collect_terms(allowed_terms, TermComparison::AsciiInsensitive)?,
        })
    }
}

/// 英文译前分析与译后源文复制检查。
#[derive(Clone, Debug)]
pub(crate) struct EnglishLanguageModule {
    detection_policy: EnglishTranslationDetectionPolicy,
    residual_policy: EnglishResidualPolicy,
}

impl EnglishLanguageModule {
    pub(crate) fn new(
        detection_policy: EnglishTranslationDetectionPolicy,
        residual_policy: EnglishResidualPolicy,
    ) -> Self {
        Self {
            detection_policy,
            residual_policy,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EnglishLanguageAnalysis {
    needs_translation: bool,
    residual_source_runs: Vec<Vec<String>>,
}

impl LanguageModule for EnglishLanguageModule {
    fn semantic_fingerprint(&self) -> Sha256Fingerprint {
        let mut hasher = Sha256FramedHasher::new(b"att.language.english");
        for (tag, value) in [
            (1, self.detection_policy.minimum_word_count.get()),
            (2, self.detection_policy.minimum_letter_count.get()),
            (4, self.residual_policy.minimum_copied_word_count.get()),
            (5, self.residual_policy.minimum_copied_letter_count.get()),
        ] {
            hasher.frame(
                tag,
                &u64::try_from(value)
                    .expect("x86_64 usize 必须可表示为 u64")
                    .to_be_bytes(),
            );
        }
        for term in &self.detection_policy.ignored_terms {
            hasher.frame(3, term.as_bytes());
        }
        for term in &self.residual_policy.allowed_terms {
            hasher.frame(6, term.as_bytes());
        }
        hasher.finish()
    }

    fn analyze_source(&self, text: &LanguageText) -> LanguageAnalysis {
        let detection_runs = english_runs(text, &self.detection_policy.ignored_terms);
        let needs_translation = detection_runs.iter().any(|run| {
            reaches_word_threshold(
                run,
                self.detection_policy.minimum_word_count,
                self.detection_policy.minimum_letter_count,
            )
        });
        let residual_source_runs = english_runs(text, &self.residual_policy.allowed_terms)
            .into_iter()
            .map(|run| run.into_iter().map(|word| word.normalized).collect())
            .collect();
        LanguageAnalysis::English(EnglishLanguageAnalysis {
            needs_translation,
            residual_source_runs,
        })
    }

    fn find_source_residual(
        &self,
        analysis: &LanguageAnalysis,
        translation: &LanguageText,
    ) -> Result<Option<LanguageResidual>, LanguageModuleError> {
        let LanguageAnalysis::English(analysis) = analysis else {
            return Err(LanguageModuleError::analysis_mismatch(
                LanguageModuleKind::English,
                analysis,
            ));
        };
        for segment in natural_texts(translation) {
            for run in english_runs_in_segment(segment, &self.residual_policy.allowed_terms) {
                if let Some(fragment) = first_copied_english_fragment(
                    segment,
                    &run,
                    &analysis.residual_source_runs,
                    self.residual_policy.minimum_copied_word_count,
                    self.residual_policy.minimum_copied_letter_count,
                ) {
                    return Ok(Some(LanguageResidual::new(fragment)));
                }
            }
        }
        Ok(None)
    }

    fn plan_translation_repair(
        &self,
        analysis: &LanguageAnalysis,
        _translation: &LanguageText,
    ) -> Result<LanguageRepairPlan, LanguageModuleError> {
        let LanguageAnalysis::English(_) = analysis else {
            return Err(LanguageModuleError::analysis_mismatch(
                LanguageModuleKind::English,
                analysis,
            ));
        };
        Ok(LanguageRepairPlan::unchanged())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LanguagePolicyConfigurationError {
    BlankTerm,
    SurroundingWhitespace { term: String },
    DuplicateTerm { term: String },
}

impl fmt::Display for LanguagePolicyConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BlankTerm => formatter.write_str("语言策略允许项不能为空白"),
            Self::SurroundingWhitespace { term } => {
                write!(formatter, "语言策略允许项含首尾空白：{term:?}")
            }
            Self::DuplicateTerm { term } => {
                write!(formatter, "语言策略允许项重复：{term}")
            }
        }
    }
}

impl Error for LanguagePolicyConfigurationError {}

#[derive(Clone, Copy)]
enum TermComparison {
    Exact,
    AsciiInsensitive,
}

fn collect_terms(
    terms: impl IntoIterator<Item = String>,
    comparison: TermComparison,
) -> Result<BTreeSet<String>, LanguagePolicyConfigurationError> {
    let mut result = BTreeSet::new();
    for term in terms {
        if term.trim().is_empty() {
            return Err(LanguagePolicyConfigurationError::BlankTerm);
        }
        if term.trim() != term {
            return Err(LanguagePolicyConfigurationError::SurroundingWhitespace { term });
        }
        let key = match comparison {
            TermComparison::Exact => term.clone(),
            TermComparison::AsciiInsensitive => term.to_ascii_lowercase(),
        };
        if !result.insert(key) {
            return Err(LanguagePolicyConfigurationError::DuplicateTerm { term });
        }
    }
    Ok(result)
}

fn merge_byte_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.sort_unstable();
    let mut merged = Vec::<(usize, usize)>::with_capacity(ranges.len());
    for (start, end) in ranges {
        if let Some((_, previous_end)) = merged.last_mut()
            && start <= *previous_end
        {
            *previous_end = (*previous_end).max(end);
        } else {
            merged.push((start, end));
        }
    }
    merged
}

fn natural_texts(text: &LanguageText) -> impl Iterator<Item = &str> {
    text.segments().iter().filter_map(|segment| match segment {
        LanguageTextSegment::NaturalText(text) => Some(text.as_str()),
        LanguageTextSegment::OpaqueBoundary => None,
    })
}

fn is_japanese_source_character(character: char) -> bool {
    is_japanese_kana_letter(character)
        || matches!(
            character as u32,
            0x3007
                | 0x3400..=0x4DBF
                | 0x4E00..=0x9FFF
                | 0xF900..=0xFAFF
                | 0x20000..=0x2A6DF
                | 0x2A700..=0x2B73F
                | 0x2B740..=0x2B81F
                | 0x2B820..=0x2EE5F
                | 0x2F800..=0x2FA1F
                | 0x30000..=0x323AF
        )
}

fn is_japanese_kana_letter(character: char) -> bool {
    matches!(
        character as u32,
        0x3041..=0x3096
            | 0x309F
            | 0x30A1..=0x30FA
            | 0x30FF
            | 0x31F0..=0x31FF
            | 0xFF66..=0xFF9D
    )
}

fn is_japanese_kana_continuation(character: char) -> bool {
    matches!(
        character as u32,
        0x3099..=0x309E | 0x30FC..=0x30FE | 0xFF70 | 0xFF9E..=0xFF9F
    )
}

fn first_japanese_residual(
    text: &str,
    minimum: NonZeroUsize,
    allowed_terms: &BTreeSet<String>,
) -> Option<String> {
    let allowed_ranges = merge_byte_ranges(
        allowed_terms
            .iter()
            .flat_map(|term| {
                text.match_indices(term)
                    .map(move |(start, _)| (start, start + term.len()))
            })
            .collect(),
    );
    let mut range_index = 0_usize;
    let mut fragment = String::new();
    let mut count = 0_usize;
    for (byte_index, character) in text.char_indices() {
        while allowed_ranges
            .get(range_index)
            .is_some_and(|(_, end)| *end <= byte_index)
        {
            range_index += 1;
        }
        let allowed = allowed_ranges
            .get(range_index)
            .is_some_and(|(start, end)| *start <= byte_index && byte_index < *end);
        if !allowed && is_japanese_kana_letter(character) {
            fragment.push(character);
            count += 1;
            continue;
        }
        if !allowed && !fragment.is_empty() && is_japanese_kana_continuation(character) {
            fragment.push(character);
            continue;
        }
        if count >= minimum.get() {
            return Some(fragment);
        }
        fragment.clear();
        count = 0;
    }
    (count >= minimum.get()).then_some(fragment)
}

#[derive(Clone, Copy)]
struct LanguageCharacterPosition {
    segment_index: usize,
    byte_offset: usize,
}

struct ParsedQuoteNode {
    pair: QuotePair,
    parent: Option<usize>,
    opening: LanguageCharacterPosition,
    closing: LanguageCharacterPosition,
}

struct PendingQuoteNode {
    pair: QuotePair,
    parent: Option<usize>,
    opening: LanguageCharacterPosition,
    closing: Option<LanguageCharacterPosition>,
}

fn parse_quote_structure(text: &LanguageText, pairs: &[QuotePair]) -> Option<Vec<ParsedQuoteNode>> {
    let roles = quote_roles(pairs)?;

    let mut nodes = Vec::<PendingQuoteNode>::new();
    let mut stack = Vec::<usize>::new();
    for (segment_index, segment) in text.segments().iter().enumerate() {
        let LanguageTextSegment::NaturalText(text) = segment else {
            continue;
        };
        for (byte_offset, character) in text.char_indices() {
            let Some(&(pair_index, role)) = roles.get(&character) else {
                continue;
            };
            let pair = pairs[pair_index];
            let position = LanguageCharacterPosition {
                segment_index,
                byte_offset,
            };
            let should_close = match role {
                QuoteCharacterRole::Closing => true,
                QuoteCharacterRole::Opening => false,
                QuoteCharacterRole::Symmetric => stack
                    .last()
                    .is_some_and(|node_index| nodes[*node_index].pair == pair),
            };
            if should_close {
                let node_index = stack.pop()?;
                if nodes[node_index].pair != pair {
                    return None;
                }
                nodes[node_index].closing = Some(position);
            } else {
                let node_index = nodes.len();
                nodes.push(PendingQuoteNode {
                    pair,
                    parent: stack.last().copied(),
                    opening: position,
                    closing: None,
                });
                stack.push(node_index);
            }
        }
    }
    if !stack.is_empty() {
        return None;
    }
    nodes
        .into_iter()
        .map(|node| {
            Some(ParsedQuoteNode {
                pair: node.pair,
                parent: node.parent,
                opening: node.opening,
                closing: node.closing?,
            })
        })
        .collect()
}

/// 按源文已经确认的拓扑解释译文引号。
///
/// 对称引号没有先验开闭方向，因此不能先用贪心 toggle 固定一种结构。这里先由
/// 源文拓扑生成唯一的开闭事件，再验证译文每个分隔符是否能够无冲突地承担对应
/// 事件；只有完整且唯一对应时才产生修复位置。
fn match_quote_structure(
    text: &LanguageText,
    pairs: &[QuotePair],
    source_nodes: &[JapaneseQuoteNode],
) -> Option<Vec<ParsedQuoteNode>> {
    let roles = quote_roles(pairs)?;
    let events = quote_events(source_nodes)?;
    let mut occurrences = Vec::new();
    for (segment_index, segment) in text.segments().iter().enumerate() {
        let LanguageTextSegment::NaturalText(text) = segment else {
            continue;
        };
        for (byte_offset, character) in text.char_indices() {
            let Some(&(pair_index, role)) = roles.get(&character) else {
                continue;
            };
            occurrences.push((
                pairs[pair_index],
                role,
                LanguageCharacterPosition {
                    segment_index,
                    byte_offset,
                },
            ));
        }
    }
    if occurrences.len() != events.len() {
        return None;
    }

    let mut matched = source_nodes
        .iter()
        .map(|node| PendingQuoteNode {
            pair: node.pair,
            parent: node.parent,
            opening: LanguageCharacterPosition {
                segment_index: usize::MAX,
                byte_offset: usize::MAX,
            },
            closing: None,
        })
        .collect::<Vec<_>>();
    let mut target_pairs = vec![None; source_nodes.len()];
    for (event, (pair, role, position)) in events.into_iter().zip(occurrences) {
        match event {
            QuoteEvent::Open(node_index) => {
                if !matches!(
                    role,
                    QuoteCharacterRole::Opening | QuoteCharacterRole::Symmetric
                ) {
                    return None;
                }
                target_pairs[node_index] = Some(pair);
                matched[node_index].pair = pair;
                matched[node_index].opening = position;
            }
            QuoteEvent::Close(node_index) => {
                if !matches!(
                    role,
                    QuoteCharacterRole::Closing | QuoteCharacterRole::Symmetric
                ) || target_pairs[node_index] != Some(pair)
                {
                    return None;
                }
                matched[node_index].closing = Some(position);
            }
        }
    }

    matched
        .into_iter()
        .map(|node| {
            Some(ParsedQuoteNode {
                pair: node.pair,
                parent: node.parent,
                opening: node.opening,
                closing: node.closing?,
            })
        })
        .collect()
}

#[derive(Clone, Copy)]
enum QuoteEvent {
    Open(usize),
    Close(usize),
}

fn quote_events(nodes: &[JapaneseQuoteNode]) -> Option<Vec<QuoteEvent>> {
    let mut events = Vec::with_capacity(nodes.len().saturating_mul(2));
    let mut stack = Vec::<usize>::new();
    for (node_index, node) in nodes.iter().enumerate() {
        while stack.last().copied() != node.parent {
            events.push(QuoteEvent::Close(stack.pop()?));
        }
        if node.parent.is_some_and(|parent| parent >= node_index) {
            return None;
        }
        events.push(QuoteEvent::Open(node_index));
        stack.push(node_index);
    }
    while let Some(node_index) = stack.pop() {
        events.push(QuoteEvent::Close(node_index));
    }
    Some(events)
}

fn quote_roles(pairs: &[QuotePair]) -> Option<BTreeMap<char, (usize, QuoteCharacterRole)>> {
    let mut roles = BTreeMap::<char, (usize, QuoteCharacterRole)>::new();
    for (pair_index, pair) in pairs.iter().copied().enumerate() {
        let opening_role = if pair.opening() == pair.closing() {
            QuoteCharacterRole::Symmetric
        } else {
            QuoteCharacterRole::Opening
        };
        if roles
            .insert(pair.opening(), (pair_index, opening_role))
            .is_some()
        {
            return None;
        }
        if pair.opening() != pair.closing()
            && roles
                .insert(pair.closing(), (pair_index, QuoteCharacterRole::Closing))
                .is_some()
        {
            return None;
        }
    }
    Some(roles)
}

#[derive(Clone, Copy)]
enum QuoteCharacterRole {
    Opening,
    Closing,
    Symmetric,
}

#[derive(Clone)]
struct EnglishWord {
    normalized: String,
    start: usize,
    end: usize,
}

fn english_runs(text: &LanguageText, excluded_terms: &BTreeSet<String>) -> Vec<Vec<EnglishWord>> {
    natural_texts(text)
        .flat_map(|segment| english_runs_in_segment(segment, excluded_terms))
        .collect()
}

fn english_runs_in_segment(text: &str, excluded_terms: &BTreeSet<String>) -> Vec<Vec<EnglishWord>> {
    let excluded_ranges = ascii_insensitive_term_ranges(text, excluded_terms);
    let words = ascii_words(text);
    let mut runs = Vec::new();
    let mut current = Vec::new();
    let mut previous_end = None;
    for word in words {
        let excluded = excluded_ranges
            .iter()
            .any(|(start, end)| word.start < *end && *start < word.end);
        let disconnected = previous_end.is_some_and(|previous_end| {
            !text[previous_end..word.start]
                .chars()
                .all(is_english_run_connector)
                || excluded_ranges
                    .iter()
                    .any(|(start, end)| previous_end < *end && *start < word.start)
        });
        if (excluded || disconnected) && !current.is_empty() {
            runs.push(std::mem::take(&mut current));
        }
        if excluded {
            previous_end = None;
            continue;
        }
        previous_end = Some(word.end);
        current.push(word);
    }
    if !current.is_empty() {
        runs.push(current);
    }
    runs
}

fn is_english_run_connector(character: char) -> bool {
    character.is_whitespace() || character.is_ascii_punctuation()
}

fn ascii_insensitive_term_ranges(
    text: &str,
    excluded_terms: &BTreeSet<String>,
) -> Vec<(usize, usize)> {
    let normalized = text.to_ascii_lowercase();
    let mut ranges = Vec::new();
    for term in excluded_terms {
        for (start, _) in normalized.match_indices(term) {
            let end = start + term.len();
            let starts_at_boundary = !term
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_alphabetic())
                || normalized[..start]
                    .chars()
                    .next_back()
                    .is_none_or(|character| !character.is_ascii_alphabetic());
            let ends_at_boundary = !term
                .chars()
                .next_back()
                .is_some_and(|character| character.is_ascii_alphabetic())
                || normalized[end..]
                    .chars()
                    .next()
                    .is_none_or(|character| !character.is_ascii_alphabetic());
            if starts_at_boundary && ends_at_boundary {
                ranges.push((start, end));
            }
        }
    }
    merge_byte_ranges(ranges)
}

fn ascii_words(text: &str) -> Vec<EnglishWord> {
    let mut words = Vec::new();
    let mut start = None;
    for (byte_index, character) in text.char_indices() {
        if character.is_ascii_alphabetic() {
            start.get_or_insert(byte_index);
        } else if let Some(word_start) = start.take() {
            words.push(EnglishWord {
                normalized: text[word_start..byte_index].to_ascii_lowercase(),
                start: word_start,
                end: byte_index,
            });
        }
    }
    if let Some(word_start) = start {
        words.push(EnglishWord {
            normalized: text[word_start..].to_ascii_lowercase(),
            start: word_start,
            end: text.len(),
        });
    }
    words
}

fn reaches_word_threshold(
    words: &[EnglishWord],
    minimum_words: NonZeroUsize,
    minimum_letters: NonZeroUsize,
) -> bool {
    words.len() >= minimum_words.get()
        && words
            .iter()
            .map(|word| word.normalized.len())
            .sum::<usize>()
            >= minimum_letters.get()
}

fn first_copied_english_fragment(
    text: &str,
    target_run: &[EnglishWord],
    source_runs: &[Vec<String>],
    minimum_words: NonZeroUsize,
    minimum_letters: NonZeroUsize,
) -> Option<String> {
    for start in 0..target_run.len() {
        for end in (start + 1..=target_run.len()).rev() {
            let candidate = &target_run[start..end];
            if !reaches_word_threshold(candidate, minimum_words, minimum_letters) {
                continue;
            }
            let copied = source_runs.iter().any(|source| {
                source.windows(candidate.len()).any(|window| {
                    window
                        .iter()
                        .zip(candidate)
                        .all(|(left, right)| left == &right.normalized)
                })
            });
            if copied {
                return Some(
                    text[candidate[0].start..candidate[candidate.len() - 1].end].to_owned(),
                );
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn non_zero(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).expect("测试值必须非零")
    }

    fn japanese_module() -> JapaneseLanguageModule {
        JapaneseLanguageModule::new(
            JapaneseResidualPolicy::new(non_zero(2), ["カタカナ名".to_owned()])
                .expect("日文残留策略有效"),
            Some(
                JapaneseQuoteRepairPolicy::new(vec![
                    QuotePair::new('“', '”'),
                    QuotePair::new('‘', '’'),
                    QuotePair::new('"', '"'),
                ])
                .expect("日文引号策略有效"),
            ),
        )
    }

    fn english_module() -> EnglishLanguageModule {
        EnglishLanguageModule::new(
            EnglishTranslationDetectionPolicy::new(non_zero(2), non_zero(8), ["HP".to_owned()])
                .expect("英文译前策略有效"),
            EnglishResidualPolicy::new(non_zero(2), non_zero(8), ["Alice".to_owned()])
                .expect("英文残留策略有效"),
        )
    }

    #[test]
    fn catalog_only_resolves_exact_source_ids() {
        let module: Arc<dyn LanguageModule> = Arc::new(japanese_module());
        let catalog = LanguageModuleCatalog::new([
            ("ja".to_owned(), Arc::clone(&module)),
            ("ja-JP".to_owned(), module),
        ])
        .expect("显式精确绑定有效");

        assert!(catalog.resolve("ja").is_ok());
        assert!(matches!(
            catalog.resolve("JA"),
            Err(LanguageModuleCatalogError::UnknownLanguageId { .. })
        ));
    }

    #[test]
    fn catalog_rejects_missing_blank_surrounded_and_duplicate_ids() {
        assert!(matches!(
            LanguageModuleCatalog::new(std::iter::empty()),
            Err(LanguageModuleCatalogBuildError::MissingLanguageModule)
        ));

        for invalid in ["", "   "] {
            let module: Arc<dyn LanguageModule> = Arc::new(japanese_module());
            assert!(matches!(
                LanguageModuleCatalog::new([(invalid.to_owned(), module)]),
                Err(LanguageModuleCatalogBuildError::BlankLanguageId)
            ));
        }

        let surrounded: Arc<dyn LanguageModule> = Arc::new(japanese_module());
        assert!(matches!(
            LanguageModuleCatalog::new([(" ja".to_owned(), surrounded)]),
            Err(LanguageModuleCatalogBuildError::SurroundingWhitespaceInLanguageId { .. })
        ));

        let first: Arc<dyn LanguageModule> = Arc::new(japanese_module());
        let second: Arc<dyn LanguageModule> = Arc::new(japanese_module());
        assert!(matches!(
            LanguageModuleCatalog::new([("ja".to_owned(), first), ("ja".to_owned(), second),]),
            Err(LanguageModuleCatalogBuildError::DuplicateLanguageId { .. })
        ));
    }

    #[test]
    fn language_text_canonicalizes_incidental_natural_segmentation() {
        assert_eq!(
            LanguageText::new(vec![
                LanguageTextSegment::NaturalText("前".to_owned()),
                LanguageTextSegment::NaturalText(String::new()),
                LanguageTextSegment::NaturalText("半".to_owned()),
                LanguageTextSegment::OpaqueBoundary,
                LanguageTextSegment::NaturalText("后半".to_owned()),
            ]),
            LanguageText::new(vec![
                LanguageTextSegment::NaturalText("前半".to_owned()),
                LanguageTextSegment::OpaqueBoundary,
                LanguageTextSegment::NaturalText("后半".to_owned()),
            ])
        );
    }

    #[test]
    fn japanese_analysis_and_residual_use_only_natural_text() {
        let module = japanese_module();
        let source = LanguageText::new(vec![
            LanguageTextSegment::NaturalText("魔法".to_owned()),
            LanguageTextSegment::OpaqueBoundary,
            LanguageTextSegment::NaturalText("剣".to_owned()),
        ]);
        let analysis = module.analyze_source(&source);
        assert!(analysis.needs_translation());
        assert_eq!(
            module
                .find_source_residual(&analysis, &LanguageText::natural("译文です"))
                .expect("分析类型一致")
                .expect("两个假名达到阈值")
                .fragment(),
            "です"
        );
        assert_eq!(
            module
                .find_source_residual(&analysis, &LanguageText::natural("保留カタカナ名"))
                .expect("分析类型一致"),
            None
        );
    }

    #[test]
    fn japanese_quote_repair_preserves_unique_nested_structure() {
        let module = japanese_module();
        let source = LanguageText::new(vec![
            LanguageTextSegment::NaturalText("彼は「これは『".to_owned()),
            LanguageTextSegment::OpaqueBoundary,
            LanguageTextSegment::NaturalText("勇者』の剣だ」と言った。".to_owned()),
        ]);
        let analysis = module.analyze_source(&source);
        let translated = LanguageText::new(vec![
            LanguageTextSegment::NaturalText("他说：“这是‘".to_owned()),
            LanguageTextSegment::OpaqueBoundary,
            LanguageTextSegment::NaturalText("勇者’之剑。”".to_owned()),
        ]);
        let plan = module
            .plan_translation_repair(&analysis, &translated)
            .expect("分析类型一致");
        let repaired = translated.apply_repair(&plan).expect("修复位置有效");
        assert_eq!(
            repaired,
            LanguageText::new(vec![
                LanguageTextSegment::NaturalText("他说：「这是『".to_owned()),
                LanguageTextSegment::OpaqueBoundary,
                LanguageTextSegment::NaturalText("勇者』之剑。」".to_owned()),
            ])
        );
    }

    #[test]
    fn japanese_quote_ambiguity_is_an_unchanged_normal_result() {
        let module = japanese_module();
        let analysis = module.analyze_source(&LanguageText::natural("「勇者」"));
        let translated = LanguageText::natural("“勇者”与“魔王”");
        let plan = module
            .plan_translation_repair(&analysis, &translated)
            .expect("分析类型一致");
        assert!(plan.is_unchanged());
        assert_eq!(
            translated.apply_repair(&plan).expect("空计划有效"),
            translated
        );
    }

    #[test]
    fn symmetric_quotes_are_interpreted_by_the_unique_source_topology() {
        let module = japanese_module();
        let analysis = module.analyze_source(&LanguageText::natural("「これは『勇者』だ」"));
        let translated = LanguageText::natural("\"This is \"the hero\".\"");

        let repaired = translated
            .apply_repair(
                &module
                    .plan_translation_repair(&analysis, &translated)
                    .expect("分析类型一致"),
            )
            .expect("唯一源拓扑应产生安全修复");

        assert_eq!(repaired, LanguageText::natural("「This is 『the hero』.」"));
    }

    #[test]
    fn changed_quote_topology_is_left_unchanged() {
        let module = japanese_module();
        let analysis = module.analyze_source(&LanguageText::natural("「これは『勇者』だ」"));
        let translated = LanguageText::natural("“勇者”与“魔王”");
        let plan = module
            .plan_translation_repair(&analysis, &translated)
            .expect("分析类型一致");

        assert!(plan.is_unchanged());
    }

    #[test]
    fn sibling_correct_and_unpaired_quotes_follow_safe_repair_boundaries() {
        let module = japanese_module();

        let sibling_analysis = module.analyze_source(&LanguageText::natural("「勇者」「魔王」"));
        let sibling_translation = LanguageText::natural("“Hero”“Demon King”");
        let sibling_repaired = sibling_translation
            .apply_repair(
                &module
                    .plan_translation_repair(&sibling_analysis, &sibling_translation)
                    .expect("分析类型一致"),
            )
            .expect("同级拓扑应该可以安全修复");
        assert_eq!(
            sibling_repaired,
            LanguageText::natural("「Hero」「Demon King」")
        );

        let correct = LanguageText::natural("「Hero」");
        let correct_analysis = module.analyze_source(&LanguageText::natural("「勇者」"));
        assert!(
            module
                .plan_translation_repair(&correct_analysis, &correct)
                .expect("分析类型一致")
                .is_unchanged()
        );

        let unpaired_analysis = module.analyze_source(&LanguageText::natural("「勇者"));
        let candidate = LanguageText::natural("“Hero”");
        assert!(
            module
                .plan_translation_repair(&unpaired_analysis, &candidate)
                .expect("不配对属于正常的不修复")
                .is_unchanged()
        );
    }

    #[test]
    fn japanese_detection_covers_extended_han_without_counting_punctuation_as_kana() {
        let module = japanese_module();
        assert!(
            module
                .analyze_source(&LanguageText::natural("𠮷"))
                .needs_translation()
        );
        let analysis = module.analyze_source(&LanguageText::natural("勇者"));
        assert_eq!(
            module
                .find_source_residual(&analysis, &LanguageText::natural("・・ーー"))
                .expect("分析类型一致"),
            None
        );
        assert_eq!(
            module
                .find_source_residual(&analysis, &LanguageText::natural("ゲーム"))
                .expect("分析类型一致")
                .expect("长音符不计数但应留在真实假名片段中")
                .fragment(),
            "ゲーム"
        );
    }

    #[test]
    fn quote_policy_rejects_characters_that_cannot_safely_be_delimiters() {
        assert!(matches!(
            JapaneseQuoteRepairPolicy::new(vec![QuotePair::new('a', 'b')]),
            Err(JapaneseQuoteRepairPolicyError::InvalidDelimiterCharacter { .. })
        ));
    }

    #[test]
    fn english_detection_and_source_copy_residual_are_separate() {
        let module = english_module();
        let source = LanguageText::new(vec![
            LanguageTextSegment::NaturalText("Press the".to_owned()),
            LanguageTextSegment::OpaqueBoundary,
            LanguageTextSegment::NaturalText("red switch before opening".to_owned()),
        ]);
        let analysis = module.analyze_source(&source);
        assert!(analysis.needs_translation());
        assert_eq!(
            module
                .find_source_residual(
                    &analysis,
                    &LanguageText::natural("请按下 red switch before opening")
                )
                .expect("分析类型一致")
                .expect("连续复制达到阈值")
                .fragment(),
            "red switch before opening"
        );
        assert_eq!(
            module
                .find_source_residual(
                    &analysis,
                    &LanguageText::natural("Alice 获得了 Good Ending")
                )
                .expect("分析类型一致"),
            None
        );
    }

    #[test]
    fn opaque_boundary_does_not_join_english_words() {
        let module = english_module();
        let source = LanguageText::new(vec![
            LanguageTextSegment::NaturalText("Magic".to_owned()),
            LanguageTextSegment::OpaqueBoundary,
            LanguageTextSegment::NaturalText("Sword".to_owned()),
        ]);
        assert!(!module.analyze_source(&source).needs_translation());
    }

    #[test]
    fn substantive_non_english_text_breaks_copied_english_runs() {
        let module = EnglishLanguageModule::new(
            EnglishTranslationDetectionPolicy::new(non_zero(1), non_zero(1), Vec::new())
                .expect("英文译前策略有效"),
            EnglishResidualPolicy::new(non_zero(4), non_zero(10), Vec::new())
                .expect("英文残留策略有效"),
        );
        let analysis = module.analyze_source(&LanguageText::natural("Press the red switch"));

        assert_eq!(
            module
                .find_source_residual(
                    &analysis,
                    &LanguageText::natural("Press 已翻译 the red switch")
                )
                .expect("分析类型一致"),
            None
        );
    }

    #[test]
    fn multi_word_allowed_english_terms_are_applied_instead_of_silently_ignored() {
        let module = EnglishLanguageModule::new(
            EnglishTranslationDetectionPolicy::new(non_zero(1), non_zero(1), Vec::new())
                .expect("英文译前策略有效"),
            EnglishResidualPolicy::new(non_zero(2), non_zero(8), ["New Game".to_owned()])
                .expect("英文多词允许项应该有效"),
        );
        let analysis = module.analyze_source(&LanguageText::natural("Press New Game button"));

        assert_eq!(
            module
                .find_source_residual(&analysis, &LanguageText::natural("Press new game button"))
                .expect("分析类型一致"),
            None
        );
    }

    #[test]
    fn mismatched_analysis_is_a_technical_error() {
        let japanese = japanese_module();
        let english = english_module();
        let analysis = english.analyze_source(&LanguageText::natural("Magic Sword"));
        assert!(matches!(
            japanese.find_source_residual(&analysis, &LanguageText::natural("译文")),
            Err(LanguageModuleError { .. })
        ));
    }
}
