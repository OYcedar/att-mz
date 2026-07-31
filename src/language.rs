//! 跨游戏引擎共享的语言分析、源文残留检查与可选安全修复。
//!
//! 本模块只理解自然语言文本和不透明边界，不依赖游戏引擎位置、数据库、CLI、
//! 占位符协议、LLM 或运行时根能力。

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::Arc;

use language_tags::{LanguageTag, ParseError as LanguageTagParseError, ValidationError};

use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};

const LANGUAGE_TEXT_CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

/// 已按 RFC 5646 验证并规范化的语言标签。
///
/// 构造边界会拒绝首尾空白、下划线、未在 IANA 注册表中的子标签以及
/// `und` 主语言；内部始终保存 RFC 5646 规范形式。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct LanguageId(String);

impl LanguageId {
    pub(crate) fn parse(input: &str) -> Result<Self, LanguageIdError> {
        if input.is_empty() || input.chars().all(char::is_whitespace) {
            return Err(LanguageIdError::Blank);
        }
        if input.trim() != input {
            return Err(LanguageIdError::SurroundingWhitespace {
                language_id: input.to_owned(),
            });
        }
        if input.contains('_') {
            return Err(LanguageIdError::Underscore {
                language_id: input.to_owned(),
            });
        }

        let parsed =
            LanguageTag::parse(input).map_err(|source| LanguageIdError::InvalidSyntax {
                language_id: input.to_owned(),
                source,
            })?;
        parsed
            .validate()
            .map_err(|source| LanguageIdError::InvalidRegistryTag {
                language_id: input.to_owned(),
                source,
            })?;
        let canonical =
            parsed
                .canonicalize()
                .map_err(|source| LanguageIdError::CanonicalizationFailed {
                    language_id: input.to_owned(),
                    source,
                })?;
        if canonical.primary_language() == "und" {
            return Err(LanguageIdError::UndefinedPrimaryLanguage {
                language_id: input.to_owned(),
            });
        }

        Ok(Self(canonical.into_string()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for LanguageId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for LanguageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for LanguageId {
    type Err = LanguageIdError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Self::parse(input)
    }
}

impl TryFrom<String> for LanguageId {
    type Error = LanguageIdError;

    fn try_from(input: String) -> Result<Self, Self::Error> {
        Self::parse(&input)
    }
}

/// 外部语言标签无法建立为受信语言 ID。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LanguageIdError {
    Blank,
    SurroundingWhitespace {
        language_id: String,
    },
    Underscore {
        language_id: String,
    },
    InvalidSyntax {
        language_id: String,
        source: LanguageTagParseError,
    },
    InvalidRegistryTag {
        language_id: String,
        source: ValidationError,
    },
    CanonicalizationFailed {
        language_id: String,
        source: ValidationError,
    },
    UndefinedPrimaryLanguage {
        language_id: String,
    },
}

impl fmt::Display for LanguageIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blank => formatter.write_str("语言 ID 不能为空白"),
            Self::SurroundingWhitespace { language_id } => {
                write!(formatter, "语言 ID 含首尾空白：{language_id:?}")
            }
            Self::Underscore { language_id } => {
                write!(
                    formatter,
                    "语言 ID 必须使用连字号分隔子标签：{language_id:?}"
                )
            }
            Self::InvalidSyntax {
                language_id,
                source,
            } => write!(
                formatter,
                "语言 ID 不符合 RFC 5646：{language_id:?}（{source}）"
            ),
            Self::InvalidRegistryTag {
                language_id,
                source,
            } => write!(
                formatter,
                "语言 ID 未通过 IANA 注册表校验：{language_id:?}（{source}）"
            ),
            Self::CanonicalizationFailed {
                language_id,
                source,
            } => write!(
                formatter,
                "语言 ID 无法唯一规范化：{language_id:?}（{source}）"
            ),
            Self::UndefinedPrimaryLanguage { language_id } => {
                write!(formatter, "语言 ID 不能使用 und 主语言：{language_id:?}")
            }
        }
    }
}

impl Error for LanguageIdError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidSyntax { source, .. } => Some(source),
            Self::InvalidRegistryTag { source, .. }
            | Self::CanonicalizationFailed { source, .. } => Some(source),
            Self::Blank
            | Self::SurroundingWhitespace { .. }
            | Self::Underscore { .. }
            | Self::UndefinedPrimaryLanguage { .. } => None,
        }
    }
}

/// 一次翻译的规范源语言与目标语言。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct LanguagePair {
    source: LanguageId,
    target: LanguageId,
}

impl LanguagePair {
    pub(crate) const fn new(source: LanguageId, target: LanguageId) -> Self {
        Self { source, target }
    }

    pub(crate) const fn source(&self) -> &LanguageId {
        &self.source
    }

    pub(crate) const fn target(&self) -> &LanguageId {
        &self.target
    }

    #[cfg(test)]
    pub(crate) fn into_parts(self) -> (LanguageId, LanguageId) {
        (self.source, self.target)
    }
}

impl fmt::Display for LanguagePair {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} -> {}", self.source, self.target)
    }
}

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

fn append_language_text_with_cancellation<E>(
    output: &mut String,
    text: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    let mut start = 0_usize;
    while start < text.len() {
        ensure_running()?;
        let mut end = start
            .saturating_add(LANGUAGE_TEXT_CANCELLATION_CHECK_BYTES)
            .min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&text[start..end]);
        start = end;
    }
    ensure_running()
}

impl LanguageText {
    #[cfg(test)]
    pub(crate) fn new(segments: Vec<LanguageTextSegment>) -> Self {
        match Self::new_with_cancellation(segments, || Ok::<_, Infallible>(())) {
            Ok(text) => text,
            Err(unreachable) => match unreachable {},
        }
    }

    pub(crate) fn new_with_cancellation<E>(
        segments: Vec<LanguageTextSegment>,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Self, E> {
        let mut normalized = Vec::with_capacity(segments.len());
        for segment in segments {
            ensure_running()?;
            match segment {
                LanguageTextSegment::NaturalText(text) if text.is_empty() => {}
                LanguageTextSegment::NaturalText(text) => {
                    if let Some(LanguageTextSegment::NaturalText(previous)) = normalized.last_mut()
                    {
                        append_language_text_with_cancellation(
                            previous,
                            &text,
                            &mut ensure_running,
                        )?;
                    } else {
                        normalized.push(LanguageTextSegment::NaturalText(text));
                    }
                }
                LanguageTextSegment::OpaqueBoundary => {
                    normalized.push(LanguageTextSegment::OpaqueBoundary);
                }
            }
        }
        ensure_running()?;
        Ok(Self {
            segments: normalized,
        })
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
    #[cfg(test)]
    pub(crate) fn apply_repair(
        &self,
        plan: &LanguageRepairPlan,
    ) -> Result<Self, LanguageRepairApplicationError> {
        match self.apply_repair_with_cancellation(plan, || Ok::<_, Infallible>(())) {
            Ok(result) => result,
            Err(unreachable) => match unreachable {},
        }
    }

    pub(crate) fn apply_repair_with_cancellation<E>(
        &self,
        plan: &LanguageRepairPlan,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<Self, LanguageRepairApplicationError>, E> {
        let mut segments = Vec::with_capacity(self.segments.len());
        for segment in &self.segments {
            ensure_running()?;
            segments.push(match segment {
                LanguageTextSegment::NaturalText(text) => LanguageTextSegment::NaturalText(
                    clone_language_text_with_cancellation(text, &mut ensure_running)?,
                ),
                LanguageTextSegment::OpaqueBoundary => LanguageTextSegment::OpaqueBoundary,
            });
        }
        let mut by_segment = BTreeMap::<usize, Vec<&LanguageCharacterReplacement>>::new();
        for replacement in plan.replacements() {
            ensure_running()?;
            by_segment
                .entry(replacement.segment_index())
                .or_default()
                .push(replacement);
        }

        for (segment_index, mut replacements) in by_segment {
            ensure_running()?;
            let Some(LanguageTextSegment::NaturalText(text)) = segments.get_mut(segment_index)
            else {
                return Ok(Err(LanguageRepairApplicationError::InvalidNaturalSegment {
                    segment_index,
                }));
            };
            stable_sort_language_replacements_with_cancellation(
                &mut replacements,
                &mut ensure_running,
            )?;
            for pair in replacements.windows(2) {
                ensure_running()?;
                if pair[0].byte_offset() == pair[1].byte_offset() {
                    return Ok(Err(LanguageRepairApplicationError::DuplicatePosition {
                        segment_index,
                        byte_offset: pair[0].byte_offset(),
                    }));
                }
            }
            let mut rebuilt_capacity = text.len();
            for replacement in replacements.iter().rev() {
                ensure_running()?;
                let byte_offset = replacement.byte_offset();
                if !text.is_char_boundary(byte_offset) {
                    return Ok(Err(
                        LanguageRepairApplicationError::InvalidCharacterBoundary {
                            segment_index,
                            byte_offset,
                        },
                    ));
                }
                let Some(actual) = text[byte_offset..].chars().next() else {
                    return Ok(Err(LanguageRepairApplicationError::MissingCharacter {
                        segment_index,
                        byte_offset,
                    }));
                };
                if actual != replacement.expected() {
                    return Ok(Err(LanguageRepairApplicationError::UnexpectedCharacter {
                        segment_index,
                        byte_offset,
                        expected: replacement.expected(),
                        actual,
                    }));
                }
                rebuilt_capacity = rebuilt_capacity
                    .checked_sub(actual.len_utf8())
                    .and_then(|length| length.checked_add(replacement.replacement().len_utf8()))
                    .expect("语言修复结果长度必须能由 usize 表示");
            }

            let original = std::mem::take(text);
            let mut rebuilt = String::with_capacity(rebuilt_capacity);
            let mut cursor = 0_usize;
            for replacement in replacements {
                ensure_running()?;
                let byte_offset = replacement.byte_offset();
                append_language_text_with_cancellation(
                    &mut rebuilt,
                    &original[cursor..byte_offset],
                    &mut ensure_running,
                )?;
                let actual = original[byte_offset..]
                    .chars()
                    .next()
                    .expect("修复位置已经验证");
                rebuilt.push(replacement.replacement());
                cursor = byte_offset + actual.len_utf8();
            }
            append_language_text_with_cancellation(
                &mut rebuilt,
                &original[cursor..],
                &mut ensure_running,
            )?;
            *text = rebuilt;
        }
        let repaired = Self::new_with_cancellation(segments, ensure_running)?;
        Ok(Ok(repaired))
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

fn stable_sort_language_replacements_with_cancellation<E>(
    replacements: &mut Vec<&LanguageCharacterReplacement>,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    let mut scratch = Vec::with_capacity(replacements.len());
    for replacement in replacements.iter().copied() {
        ensure_running()?;
        scratch.push(replacement);
    }
    let mut width = 1_usize;
    while width < replacements.len() {
        let run_width = width.saturating_mul(2);
        let mut run_start = 0_usize;
        while run_start < replacements.len() {
            let middle = run_start.saturating_add(width).min(replacements.len());
            let run_end = run_start.saturating_add(run_width).min(replacements.len());
            let mut left = run_start;
            let mut right = middle;
            let mut output = run_start;
            while output < run_end {
                ensure_running()?;
                let take_left = right == run_end
                    || (left < middle
                        && replacements[left].byte_offset() <= replacements[right].byte_offset());
                scratch[output] = if take_left {
                    let replacement = replacements[left];
                    left += 1;
                    replacement
                } else {
                    let replacement = replacements[right];
                    right += 1;
                    replacement
                };
                output += 1;
            }
            run_start = run_end;
        }
        std::mem::swap(replacements, &mut scratch);
        width = run_width;
    }
    ensure_running()
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

    fn semantic_fingerprint_with_cancellation(
        &self,
        ensure_running: &mut dyn FnMut() -> Result<(), LanguageOperationCancelled>,
    ) -> Result<Sha256Fingerprint, LanguageOperationCancelled> {
        ensure_running()?;
        let fingerprint = self.semantic_fingerprint();
        ensure_running()?;
        Ok(fingerprint)
    }

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

    fn analyze_source_with_cancellation(
        &self,
        text: &LanguageText,
        ensure_running: &mut dyn FnMut() -> Result<(), LanguageOperationCancelled>,
    ) -> Result<LanguageAnalysis, LanguageOperationCancelled> {
        ensure_running()?;
        let analysis = self.analyze_source(text);
        ensure_running()?;
        Ok(analysis)
    }

    fn find_source_residual_with_cancellation(
        &self,
        analysis: &LanguageAnalysis,
        translation: &LanguageText,
        ensure_running: &mut dyn FnMut() -> Result<(), LanguageOperationCancelled>,
    ) -> Result<Result<Option<LanguageResidual>, LanguageModuleError>, LanguageOperationCancelled>
    {
        ensure_running()?;
        let residual = self.find_source_residual(analysis, translation);
        ensure_running()?;
        Ok(residual)
    }

    fn plan_translation_repair_with_cancellation(
        &self,
        analysis: &LanguageAnalysis,
        translation: &LanguageText,
        ensure_running: &mut dyn FnMut() -> Result<(), LanguageOperationCancelled>,
    ) -> Result<Result<LanguageRepairPlan, LanguageModuleError>, LanguageOperationCancelled> {
        ensure_running()?;
        let repair = self.plan_translation_repair(analysis, translation);
        ensure_running()?;
        Ok(repair)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LanguageOperationCancelled;

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

    /// 返回闭集语言模块身份，不公开被分析文本。
    pub(crate) fn safe_diagnostic_detail(&self) -> String {
        format!(
            "language_analysis_module_mismatch; expected={}; actual={}",
            self.expected.name(),
            self.actual.name()
        )
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
    modules: BTreeMap<LanguageId, Arc<dyn LanguageModule>>,
}

impl LanguageModuleCatalog {
    pub(crate) fn new(
        bindings: impl IntoIterator<Item = (LanguageId, Arc<dyn LanguageModule>)>,
    ) -> Result<Self, LanguageModuleCatalogBuildError> {
        let mut modules = BTreeMap::new();
        for (language_id, module) in bindings {
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
        language_id: &LanguageId,
    ) -> Result<Arc<dyn LanguageModule>, LanguageModuleCatalogError> {
        self.modules.get(language_id).cloned().ok_or_else(|| {
            LanguageModuleCatalogError::UnknownLanguageId {
                language_id: language_id.clone(),
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
    DuplicateLanguageId { language_id: LanguageId },
}

impl fmt::Display for LanguageModuleCatalogBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingLanguageModule => formatter.write_str("没有绑定任何源语言模块"),
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
        language_id: LanguageId,
        available_ids: Vec<LanguageId>,
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
                "未知源语言 ID {language_id}；可用 ID：{}",
                available_ids
                    .iter()
                    .map(LanguageId::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
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

    fn all_pairs_with_cancellation<E>(
        &self,
        ensure_running: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Vec<QuotePair>, E> {
        let mut pairs = Vec::with_capacity(JAPANESE_QUOTE_PAIRS.len() + self.candidate_pairs.len());
        for pair in JAPANESE_QUOTE_PAIRS
            .into_iter()
            .chain(self.candidate_pairs.iter().copied())
        {
            ensure_running()?;
            pairs.push(pair);
        }
        ensure_running()?;
        Ok(pairs)
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

    fn semantic_fingerprint_with_check<E>(
        &self,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Sha256Fingerprint, E> {
        ensure_running()?;
        let chunk_size =
            NonZeroUsize::new(LANGUAGE_TEXT_CANCELLATION_CHECK_BYTES).expect("检查块大小必须非零");
        let mut hasher = Sha256FramedHasher::new(b"att.language.japanese");
        hasher.frame(
            1,
            &u64::try_from(self.residual_policy.minimum_kana_characters.get())
                .expect("x86_64 usize 必须可表示为 u64")
                .to_be_bytes(),
        );
        for term in &self.residual_policy.allowed_terms {
            ensure_running()?;
            hasher.try_frame_chunks(2, term.as_bytes(), chunk_size, &mut ensure_running)?;
        }
        match &self.quote_repair_policy {
            None => {
                hasher.frame(3, &[0]);
            }
            Some(policy) => {
                hasher.frame(3, &[1]);
                for pair in &policy.candidate_pairs {
                    ensure_running()?;
                    let mut encoded = [0_u8; 8];
                    encoded[..4].copy_from_slice(&u32::from(pair.opening()).to_be_bytes());
                    encoded[4..].copy_from_slice(&u32::from(pair.closing()).to_be_bytes());
                    hasher.frame(4, &encoded);
                }
            }
        }
        ensure_running()?;
        Ok(hasher.finish())
    }

    fn analyze_source_with_check<E>(
        &self,
        text: &LanguageText,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<LanguageAnalysis, E> {
        ensure_running()?;
        let mut needs_translation = false;
        'segments: for segment in text.segments() {
            ensure_running()?;
            let LanguageTextSegment::NaturalText(text) = segment else {
                continue;
            };
            let mut next_check = 0_usize;
            for (byte_offset, character) in text.char_indices() {
                ensure_language_text_progress(byte_offset, &mut next_check, &mut ensure_running)?;
                if is_japanese_source_character(character) {
                    needs_translation = true;
                    break 'segments;
                }
            }
        }
        let quote_structure = match parse_quote_structure_with_cancellation(
            text,
            &JAPANESE_QUOTE_PAIRS,
            &mut ensure_running,
        )? {
            Some(nodes) => {
                let mut structure = Vec::with_capacity(nodes.len());
                for node in nodes {
                    ensure_running()?;
                    structure.push(JapaneseQuoteNode {
                        pair: node.pair,
                        parent: node.parent,
                    });
                }
                Some(structure)
            }
            None => None,
        };
        ensure_running()?;
        Ok(LanguageAnalysis::Japanese(JapaneseLanguageAnalysis {
            needs_translation,
            quote_structure,
        }))
    }

    fn find_source_residual_with_check<E>(
        &self,
        analysis: &LanguageAnalysis,
        translation: &LanguageText,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<Option<LanguageResidual>, LanguageModuleError>, E> {
        ensure_running()?;
        let LanguageAnalysis::Japanese(_) = analysis else {
            return Ok(Err(LanguageModuleError::analysis_mismatch(
                LanguageModuleKind::Japanese,
                analysis,
            )));
        };
        for text in natural_texts(translation) {
            ensure_running()?;
            if let Some(fragment) = first_japanese_residual_with_cancellation(
                text,
                self.residual_policy.minimum_kana_characters,
                &self.residual_policy.allowed_terms,
                &mut ensure_running,
            )? {
                return Ok(Ok(Some(LanguageResidual::new(fragment))));
            }
        }
        ensure_running()?;
        Ok(Ok(None))
    }

    fn plan_translation_repair_with_check<E>(
        &self,
        analysis: &LanguageAnalysis,
        translation: &LanguageText,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<LanguageRepairPlan, LanguageModuleError>, E> {
        ensure_running()?;
        let LanguageAnalysis::Japanese(analysis) = analysis else {
            return Ok(Err(LanguageModuleError::analysis_mismatch(
                LanguageModuleKind::Japanese,
                analysis,
            )));
        };
        let (Some(policy), Some(source_nodes)) =
            (&self.quote_repair_policy, &analysis.quote_structure)
        else {
            ensure_running()?;
            return Ok(Ok(LanguageRepairPlan::unchanged()));
        };
        if source_nodes.is_empty() {
            ensure_running()?;
            return Ok(Ok(LanguageRepairPlan::unchanged()));
        }
        let pairs = policy.all_pairs_with_cancellation(&mut ensure_running)?;
        let Some(target_nodes) = match_quote_structure_with_cancellation(
            translation,
            &pairs,
            source_nodes,
            &mut ensure_running,
        )?
        else {
            return Ok(Ok(LanguageRepairPlan::unchanged()));
        };

        let mut replacements = Vec::new();
        for (source, target) in source_nodes.iter().zip(target_nodes) {
            ensure_running()?;
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
        ensure_running()?;
        Ok(Ok(LanguageRepairPlan::replacing(replacements)))
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
        match self.semantic_fingerprint_with_check(|| Ok::<_, Infallible>(())) {
            Ok(fingerprint) => fingerprint,
            Err(unreachable) => match unreachable {},
        }
    }

    fn semantic_fingerprint_with_cancellation(
        &self,
        ensure_running: &mut dyn FnMut() -> Result<(), LanguageOperationCancelled>,
    ) -> Result<Sha256Fingerprint, LanguageOperationCancelled> {
        self.semantic_fingerprint_with_check(ensure_running)
    }

    fn analyze_source(&self, text: &LanguageText) -> LanguageAnalysis {
        match self.analyze_source_with_check(text, || Ok::<_, Infallible>(())) {
            Ok(analysis) => analysis,
            Err(unreachable) => match unreachable {},
        }
    }

    fn find_source_residual(
        &self,
        analysis: &LanguageAnalysis,
        translation: &LanguageText,
    ) -> Result<Option<LanguageResidual>, LanguageModuleError> {
        match self.find_source_residual_with_check(
            analysis,
            translation,
            || Ok::<_, Infallible>(()),
        ) {
            Ok(result) => result,
            Err(unreachable) => match unreachable {},
        }
    }

    fn plan_translation_repair(
        &self,
        analysis: &LanguageAnalysis,
        translation: &LanguageText,
    ) -> Result<LanguageRepairPlan, LanguageModuleError> {
        match self
            .plan_translation_repair_with_check(analysis, translation, || Ok::<_, Infallible>(()))
        {
            Ok(result) => result,
            Err(unreachable) => match unreachable {},
        }
    }

    fn analyze_source_with_cancellation(
        &self,
        text: &LanguageText,
        ensure_running: &mut dyn FnMut() -> Result<(), LanguageOperationCancelled>,
    ) -> Result<LanguageAnalysis, LanguageOperationCancelled> {
        self.analyze_source_with_check(text, ensure_running)
    }

    fn find_source_residual_with_cancellation(
        &self,
        analysis: &LanguageAnalysis,
        translation: &LanguageText,
        ensure_running: &mut dyn FnMut() -> Result<(), LanguageOperationCancelled>,
    ) -> Result<Result<Option<LanguageResidual>, LanguageModuleError>, LanguageOperationCancelled>
    {
        self.find_source_residual_with_check(analysis, translation, ensure_running)
    }

    fn plan_translation_repair_with_cancellation(
        &self,
        analysis: &LanguageAnalysis,
        translation: &LanguageText,
        ensure_running: &mut dyn FnMut() -> Result<(), LanguageOperationCancelled>,
    ) -> Result<Result<LanguageRepairPlan, LanguageModuleError>, LanguageOperationCancelled> {
        self.plan_translation_repair_with_check(analysis, translation, ensure_running)
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

    fn semantic_fingerprint_with_check<E>(
        &self,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Sha256Fingerprint, E> {
        ensure_running()?;
        let chunk_size =
            NonZeroUsize::new(LANGUAGE_TEXT_CANCELLATION_CHECK_BYTES).expect("检查块大小必须非零");
        let mut hasher = Sha256FramedHasher::new(b"att.language.english");
        for (tag, value) in [
            (1, self.detection_policy.minimum_word_count.get()),
            (2, self.detection_policy.minimum_letter_count.get()),
            (4, self.residual_policy.minimum_copied_word_count.get()),
            (5, self.residual_policy.minimum_copied_letter_count.get()),
        ] {
            ensure_running()?;
            hasher.frame(
                tag,
                &u64::try_from(value)
                    .expect("x86_64 usize 必须可表示为 u64")
                    .to_be_bytes(),
            );
        }
        for term in &self.detection_policy.ignored_terms {
            ensure_running()?;
            hasher.try_frame_chunks(3, term.as_bytes(), chunk_size, &mut ensure_running)?;
        }
        for term in &self.residual_policy.allowed_terms {
            ensure_running()?;
            hasher.try_frame_chunks(6, term.as_bytes(), chunk_size, &mut ensure_running)?;
        }
        ensure_running()?;
        Ok(hasher.finish())
    }

    fn analyze_source_with_check<E>(
        &self,
        text: &LanguageText,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<LanguageAnalysis, E> {
        ensure_running()?;
        let detection_runs = english_runs_with_cancellation(
            text,
            &self.detection_policy.ignored_terms,
            &mut ensure_running,
        )?;
        let mut needs_translation = false;
        for run in &detection_runs {
            ensure_running()?;
            if reaches_word_threshold_with_cancellation(
                run,
                self.detection_policy.minimum_word_count,
                self.detection_policy.minimum_letter_count,
                &mut ensure_running,
            )? {
                needs_translation = true;
                break;
            }
        }
        let source_runs = english_runs_with_cancellation(
            text,
            &self.residual_policy.allowed_terms,
            &mut ensure_running,
        )?;
        let mut residual_source_runs = Vec::with_capacity(source_runs.len());
        for run in source_runs {
            ensure_running()?;
            let mut normalized_run = Vec::with_capacity(run.len());
            for word in run {
                ensure_running()?;
                normalized_run.push(word.normalized);
            }
            residual_source_runs.push(normalized_run);
        }
        ensure_running()?;
        Ok(LanguageAnalysis::English(EnglishLanguageAnalysis {
            needs_translation,
            residual_source_runs,
        }))
    }

    fn find_source_residual_with_check<E>(
        &self,
        analysis: &LanguageAnalysis,
        translation: &LanguageText,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<Option<LanguageResidual>, LanguageModuleError>, E> {
        ensure_running()?;
        let LanguageAnalysis::English(analysis) = analysis else {
            return Ok(Err(LanguageModuleError::analysis_mismatch(
                LanguageModuleKind::English,
                analysis,
            )));
        };
        for segment in natural_texts(translation) {
            ensure_running()?;
            let runs = english_runs_in_segment_with_cancellation(
                segment,
                &self.residual_policy.allowed_terms,
                &mut ensure_running,
            )?;
            for run in runs {
                ensure_running()?;
                if let Some(fragment) = first_copied_english_fragment_with_cancellation(
                    segment,
                    &run,
                    &analysis.residual_source_runs,
                    self.residual_policy.minimum_copied_word_count,
                    self.residual_policy.minimum_copied_letter_count,
                    &mut ensure_running,
                )? {
                    return Ok(Ok(Some(LanguageResidual::new(fragment))));
                }
            }
        }
        ensure_running()?;
        Ok(Ok(None))
    }

    fn plan_translation_repair_with_check<E>(
        &self,
        analysis: &LanguageAnalysis,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<LanguageRepairPlan, LanguageModuleError>, E> {
        ensure_running()?;
        let LanguageAnalysis::English(_) = analysis else {
            return Ok(Err(LanguageModuleError::analysis_mismatch(
                LanguageModuleKind::English,
                analysis,
            )));
        };
        ensure_running()?;
        Ok(Ok(LanguageRepairPlan::unchanged()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EnglishLanguageAnalysis {
    needs_translation: bool,
    residual_source_runs: Vec<Vec<String>>,
}

impl LanguageModule for EnglishLanguageModule {
    fn semantic_fingerprint(&self) -> Sha256Fingerprint {
        match self.semantic_fingerprint_with_check(|| Ok::<_, Infallible>(())) {
            Ok(fingerprint) => fingerprint,
            Err(unreachable) => match unreachable {},
        }
    }

    fn semantic_fingerprint_with_cancellation(
        &self,
        ensure_running: &mut dyn FnMut() -> Result<(), LanguageOperationCancelled>,
    ) -> Result<Sha256Fingerprint, LanguageOperationCancelled> {
        self.semantic_fingerprint_with_check(ensure_running)
    }

    fn analyze_source(&self, text: &LanguageText) -> LanguageAnalysis {
        match self.analyze_source_with_check(text, || Ok::<_, Infallible>(())) {
            Ok(analysis) => analysis,
            Err(unreachable) => match unreachable {},
        }
    }

    fn find_source_residual(
        &self,
        analysis: &LanguageAnalysis,
        translation: &LanguageText,
    ) -> Result<Option<LanguageResidual>, LanguageModuleError> {
        match self.find_source_residual_with_check(
            analysis,
            translation,
            || Ok::<_, Infallible>(()),
        ) {
            Ok(result) => result,
            Err(unreachable) => match unreachable {},
        }
    }

    fn plan_translation_repair(
        &self,
        analysis: &LanguageAnalysis,
        _translation: &LanguageText,
    ) -> Result<LanguageRepairPlan, LanguageModuleError> {
        match self.plan_translation_repair_with_check(analysis, || Ok::<_, Infallible>(())) {
            Ok(result) => result,
            Err(unreachable) => match unreachable {},
        }
    }

    fn analyze_source_with_cancellation(
        &self,
        text: &LanguageText,
        ensure_running: &mut dyn FnMut() -> Result<(), LanguageOperationCancelled>,
    ) -> Result<LanguageAnalysis, LanguageOperationCancelled> {
        self.analyze_source_with_check(text, ensure_running)
    }

    fn find_source_residual_with_cancellation(
        &self,
        analysis: &LanguageAnalysis,
        translation: &LanguageText,
        ensure_running: &mut dyn FnMut() -> Result<(), LanguageOperationCancelled>,
    ) -> Result<Result<Option<LanguageResidual>, LanguageModuleError>, LanguageOperationCancelled>
    {
        self.find_source_residual_with_check(analysis, translation, ensure_running)
    }

    fn plan_translation_repair_with_cancellation(
        &self,
        analysis: &LanguageAnalysis,
        _translation: &LanguageText,
        ensure_running: &mut dyn FnMut() -> Result<(), LanguageOperationCancelled>,
    ) -> Result<Result<LanguageRepairPlan, LanguageModuleError>, LanguageOperationCancelled> {
        self.plan_translation_repair_with_check(analysis, ensure_running)
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

fn clone_language_text_with_cancellation<E>(
    text: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<String, E> {
    let mut cloned = String::with_capacity(text.len());
    append_language_text_with_cancellation(&mut cloned, text, ensure_running)?;
    Ok(cloned)
}

fn ascii_lowercase_with_cancellation<E>(
    text: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<String, E> {
    let mut normalized = String::with_capacity(text.len());
    let mut start = 0_usize;
    while start < text.len() {
        ensure_running()?;
        let mut end = start
            .saturating_add(LANGUAGE_TEXT_CANCELLATION_CHECK_BYTES)
            .min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end -= 1;
        }
        normalized.push_str(&text[start..end].to_ascii_lowercase());
        start = end;
    }
    ensure_running()?;
    Ok(normalized)
}

fn find_language_substring_with_cancellation<E>(
    text: &str,
    start: usize,
    needle: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Option<usize>, E> {
    if needle.len() <= LANGUAGE_TEXT_CANCELLATION_CHECK_BYTES {
        return find_short_language_substring_with_cancellation(
            text,
            start,
            needle,
            ensure_running,
        );
    }

    let mut anchor_end = LANGUAGE_TEXT_CANCELLATION_CHECK_BYTES;
    while !needle.is_char_boundary(anchor_end) {
        anchor_end -= 1;
    }
    let anchor = &needle[..anchor_end];
    let first_character_bytes = needle
        .chars()
        .next()
        .expect("非空 needle 必须包含字符")
        .len_utf8();
    let mut cursor = start;
    while let Some(candidate) =
        find_short_language_substring_with_cancellation(text, cursor, anchor, ensure_running)?
    {
        let Some(candidate_end) = candidate.checked_add(needle.len()) else {
            return Ok(None);
        };
        if candidate_end <= text.len()
            && text.is_char_boundary(candidate_end)
            && language_text_equal_with_cancellation(
                &text[candidate..candidate_end],
                needle,
                ensure_running,
            )?
        {
            return Ok(Some(candidate));
        }
        cursor = candidate.saturating_add(first_character_bytes);
    }
    Ok(None)
}

fn find_short_language_substring_with_cancellation<E>(
    text: &str,
    start: usize,
    needle: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Option<usize>, E> {
    debug_assert!(text.is_char_boundary(start));
    debug_assert!(!needle.is_empty());
    if text.len().saturating_sub(start) < needle.len() {
        ensure_running()?;
        return Ok(None);
    }
    let overlap_bytes = needle.len().saturating_sub(1);
    let mut chunk_start = start;
    while chunk_start <= text.len() - needle.len() {
        ensure_running()?;
        let mut primary_end = chunk_start
            .saturating_add(LANGUAGE_TEXT_CANCELLATION_CHECK_BYTES)
            .min(text.len());
        while primary_end < text.len() && !text.is_char_boundary(primary_end) {
            primary_end -= 1;
        }
        let mut search_end = primary_end.saturating_add(overlap_bytes).min(text.len());
        while search_end < text.len() && !text.is_char_boundary(search_end) {
            search_end += 1;
        }
        if let Some(relative) = text[chunk_start..search_end].find(needle) {
            return Ok(Some(chunk_start + relative));
        }
        if primary_end == text.len() {
            break;
        }
        chunk_start = primary_end;
    }
    ensure_running()?;
    Ok(None)
}

fn language_text_equal_with_cancellation<E>(
    left: &str,
    right: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<bool, E> {
    if left.len() != right.len() {
        ensure_running()?;
        return Ok(false);
    }
    for (left, right) in left
        .as_bytes()
        .chunks(LANGUAGE_TEXT_CANCELLATION_CHECK_BYTES)
        .zip(
            right
                .as_bytes()
                .chunks(LANGUAGE_TEXT_CANCELLATION_CHECK_BYTES),
        )
    {
        ensure_running()?;
        if left != right {
            return Ok(false);
        }
    }
    ensure_running()?;
    Ok(true)
}

fn ensure_language_text_progress<E>(
    byte_offset: usize,
    next_check: &mut usize,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    if byte_offset >= *next_check {
        ensure_running()?;
        *next_check = byte_offset.saturating_add(LANGUAGE_TEXT_CANCELLATION_CHECK_BYTES);
    }
    Ok(())
}

fn merge_byte_ranges_with_cancellation<E>(
    mut ranges: Vec<(usize, usize)>,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Vec<(usize, usize)>, E> {
    let mut scratch = Vec::with_capacity(ranges.len());
    for _ in 0..ranges.len() {
        ensure_running()?;
        scratch.push((0_usize, 0_usize));
    }
    let mut width = 1_usize;
    while width < ranges.len() {
        let run_width = width.saturating_mul(2);
        let mut run_start = 0_usize;
        while run_start < ranges.len() {
            let middle = run_start.saturating_add(width).min(ranges.len());
            let run_end = run_start.saturating_add(run_width).min(ranges.len());
            let mut left = run_start;
            let mut right = middle;
            let mut output = run_start;
            while output < run_end {
                ensure_running()?;
                let take_left =
                    right == run_end || (left < middle && ranges[left] <= ranges[right]);
                scratch[output] = if take_left {
                    let range = ranges[left];
                    left += 1;
                    range
                } else {
                    let range = ranges[right];
                    right += 1;
                    range
                };
                output += 1;
            }
            run_start = run_end;
        }
        std::mem::swap(&mut ranges, &mut scratch);
        width = run_width;
    }

    let mut merged = Vec::<(usize, usize)>::with_capacity(ranges.len());
    for (start, end) in ranges {
        ensure_running()?;
        if let Some((_, previous_end)) = merged.last_mut()
            && start <= *previous_end
        {
            *previous_end = (*previous_end).max(end);
        } else {
            merged.push((start, end));
        }
    }
    ensure_running()?;
    Ok(merged)
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

fn first_japanese_residual_with_cancellation<E>(
    text: &str,
    minimum: NonZeroUsize,
    allowed_terms: &BTreeSet<String>,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Option<String>, E> {
    let mut ranges = Vec::new();
    for term in allowed_terms {
        ensure_running()?;
        let mut cursor = 0_usize;
        while let Some(start) =
            find_language_substring_with_cancellation(text, cursor, term, &mut ensure_running)?
        {
            let end = start + term.len();
            ranges.push((start, end));
            cursor = end;
        }
    }
    let allowed_ranges = merge_byte_ranges_with_cancellation(ranges, &mut ensure_running)?;
    let mut range_index = 0_usize;
    let mut fragment_start = None;
    let mut fragment_end = 0_usize;
    let mut count = 0_usize;
    let mut next_check = 0_usize;
    for (byte_index, character) in text.char_indices() {
        ensure_language_text_progress(byte_index, &mut next_check, &mut ensure_running)?;
        while allowed_ranges
            .get(range_index)
            .is_some_and(|(_, end)| *end <= byte_index)
        {
            ensure_running()?;
            range_index += 1;
        }
        let allowed = allowed_ranges
            .get(range_index)
            .is_some_and(|(start, end)| *start <= byte_index && byte_index < *end);
        if !allowed && is_japanese_kana_letter(character) {
            fragment_start.get_or_insert(byte_index);
            fragment_end = byte_index + character.len_utf8();
            count += 1;
            continue;
        }
        if !allowed && fragment_start.is_some() && is_japanese_kana_continuation(character) {
            fragment_end = byte_index + character.len_utf8();
            continue;
        }
        if count >= minimum.get() {
            return Ok(Some(clone_language_text_with_cancellation(
                &text[fragment_start.expect("非零计数必须有起点")..fragment_end],
                &mut ensure_running,
            )?));
        }
        fragment_start = None;
        fragment_end = 0;
        count = 0;
    }
    ensure_running()?;
    if count >= minimum.get() {
        Ok(Some(clone_language_text_with_cancellation(
            &text[fragment_start.expect("非零计数必须有起点")..fragment_end],
            &mut ensure_running,
        )?))
    } else {
        Ok(None)
    }
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

fn parse_quote_structure_with_cancellation<E>(
    text: &LanguageText,
    pairs: &[QuotePair],
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Option<Vec<ParsedQuoteNode>>, E> {
    let Some(roles) = quote_roles_with_cancellation(pairs, &mut ensure_running)? else {
        return Ok(None);
    };

    let mut nodes = Vec::<PendingQuoteNode>::new();
    let mut stack = Vec::<usize>::new();
    for (segment_index, segment) in text.segments().iter().enumerate() {
        ensure_running()?;
        let LanguageTextSegment::NaturalText(text) = segment else {
            continue;
        };
        let mut next_check = 0_usize;
        for (byte_offset, character) in text.char_indices() {
            ensure_language_text_progress(byte_offset, &mut next_check, &mut ensure_running)?;
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
                let Some(node_index) = stack.pop() else {
                    return Ok(None);
                };
                if nodes[node_index].pair != pair {
                    return Ok(None);
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
        return Ok(None);
    }
    let mut parsed = Vec::with_capacity(nodes.len());
    for node in nodes {
        ensure_running()?;
        let Some(closing) = node.closing else {
            return Ok(None);
        };
        parsed.push(ParsedQuoteNode {
            pair: node.pair,
            parent: node.parent,
            opening: node.opening,
            closing,
        });
    }
    ensure_running()?;
    Ok(Some(parsed))
}

/// 按源文已经确认的拓扑解释译文引号。
///
/// 对称引号没有先验开闭方向，因此不能先用贪心 toggle 固定一种结构。这里先由
/// 源文拓扑生成唯一的开闭事件，再验证译文每个分隔符是否能够无冲突地承担对应
/// 事件；只有完整且唯一对应时才产生修复位置。
fn match_quote_structure_with_cancellation<E>(
    text: &LanguageText,
    pairs: &[QuotePair],
    source_nodes: &[JapaneseQuoteNode],
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Option<Vec<ParsedQuoteNode>>, E> {
    let Some(roles) = quote_roles_with_cancellation(pairs, &mut ensure_running)? else {
        return Ok(None);
    };
    let Some(events) = quote_events_with_cancellation(source_nodes, &mut ensure_running)? else {
        return Ok(None);
    };
    let mut occurrences = Vec::new();
    for (segment_index, segment) in text.segments().iter().enumerate() {
        ensure_running()?;
        let LanguageTextSegment::NaturalText(text) = segment else {
            continue;
        };
        let mut next_check = 0_usize;
        for (byte_offset, character) in text.char_indices() {
            ensure_language_text_progress(byte_offset, &mut next_check, &mut ensure_running)?;
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
        return Ok(None);
    }

    let mut matched = Vec::with_capacity(source_nodes.len());
    let mut target_pairs = Vec::with_capacity(source_nodes.len());
    for node in source_nodes {
        ensure_running()?;
        matched.push(PendingQuoteNode {
            pair: node.pair,
            parent: node.parent,
            opening: LanguageCharacterPosition {
                segment_index: usize::MAX,
                byte_offset: usize::MAX,
            },
            closing: None,
        });
        target_pairs.push(None);
    }
    for (event, (pair, role, position)) in events.into_iter().zip(occurrences) {
        ensure_running()?;
        match event {
            QuoteEvent::Open(node_index) => {
                if !matches!(
                    role,
                    QuoteCharacterRole::Opening | QuoteCharacterRole::Symmetric
                ) {
                    return Ok(None);
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
                    return Ok(None);
                }
                matched[node_index].closing = Some(position);
            }
        }
    }

    let mut parsed = Vec::with_capacity(matched.len());
    for node in matched {
        ensure_running()?;
        let Some(closing) = node.closing else {
            return Ok(None);
        };
        parsed.push(ParsedQuoteNode {
            pair: node.pair,
            parent: node.parent,
            opening: node.opening,
            closing,
        });
    }
    ensure_running()?;
    Ok(Some(parsed))
}

#[derive(Clone, Copy)]
enum QuoteEvent {
    Open(usize),
    Close(usize),
}

fn quote_events_with_cancellation<E>(
    nodes: &[JapaneseQuoteNode],
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Option<Vec<QuoteEvent>>, E> {
    let mut events = Vec::with_capacity(nodes.len().saturating_mul(2));
    let mut stack = Vec::<usize>::new();
    for (node_index, node) in nodes.iter().enumerate() {
        ensure_running()?;
        while stack.last().copied() != node.parent {
            ensure_running()?;
            let Some(parent) = stack.pop() else {
                return Ok(None);
            };
            events.push(QuoteEvent::Close(parent));
        }
        if node.parent.is_some_and(|parent| parent >= node_index) {
            return Ok(None);
        }
        events.push(QuoteEvent::Open(node_index));
        stack.push(node_index);
    }
    while let Some(node_index) = stack.pop() {
        ensure_running()?;
        events.push(QuoteEvent::Close(node_index));
    }
    ensure_running()?;
    Ok(Some(events))
}

fn quote_roles_with_cancellation<E>(
    pairs: &[QuotePair],
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Option<QuoteRoles>, E> {
    let mut roles = QuoteRoles::new();
    for (pair_index, pair) in pairs.iter().copied().enumerate() {
        ensure_running()?;
        let opening_role = if pair.opening() == pair.closing() {
            QuoteCharacterRole::Symmetric
        } else {
            QuoteCharacterRole::Opening
        };
        if roles
            .insert(pair.opening(), (pair_index, opening_role))
            .is_some()
        {
            return Ok(None);
        }
        if pair.opening() != pair.closing()
            && roles
                .insert(pair.closing(), (pair_index, QuoteCharacterRole::Closing))
                .is_some()
        {
            return Ok(None);
        }
    }
    ensure_running()?;
    Ok(Some(roles))
}

#[derive(Clone, Copy)]
enum QuoteCharacterRole {
    Opening,
    Closing,
    Symmetric,
}

type QuoteRoles = BTreeMap<char, (usize, QuoteCharacterRole)>;

#[derive(Clone)]
struct EnglishWord {
    normalized: String,
    start: usize,
    end: usize,
}

fn english_runs_with_cancellation<E>(
    text: &LanguageText,
    excluded_terms: &BTreeSet<String>,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Vec<Vec<EnglishWord>>, E> {
    let mut runs = Vec::new();
    for segment in natural_texts(text) {
        ensure_running()?;
        for run in
            english_runs_in_segment_with_cancellation(segment, excluded_terms, &mut ensure_running)?
        {
            ensure_running()?;
            runs.push(run);
        }
    }
    ensure_running()?;
    Ok(runs)
}

fn english_runs_in_segment_with_cancellation<E>(
    text: &str,
    excluded_terms: &BTreeSet<String>,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Vec<Vec<EnglishWord>>, E> {
    let excluded_ranges =
        ascii_insensitive_term_ranges_with_cancellation(text, excluded_terms, &mut ensure_running)?;
    let words = ascii_words_with_cancellation(text, &mut ensure_running)?;
    let mut runs = Vec::new();
    let mut current = Vec::new();
    let mut previous_end = None;
    for word in words {
        ensure_running()?;
        let mut excluded = false;
        for (start, end) in &excluded_ranges {
            ensure_running()?;
            if word.start < *end && *start < word.end {
                excluded = true;
                break;
            }
        }
        let disconnected = if let Some(previous_end) = previous_end {
            let mut connectors_only = true;
            let mut next_check = 0_usize;
            for (relative_offset, character) in text[previous_end..word.start].char_indices() {
                ensure_language_text_progress(
                    relative_offset,
                    &mut next_check,
                    &mut ensure_running,
                )?;
                if !is_english_run_connector(character) {
                    connectors_only = false;
                    break;
                }
            }
            let mut excluded_between = false;
            for (start, end) in &excluded_ranges {
                ensure_running()?;
                if previous_end < *end && *start < word.start {
                    excluded_between = true;
                    break;
                }
            }
            !connectors_only || excluded_between
        } else {
            false
        };
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
    ensure_running()?;
    Ok(runs)
}

fn is_english_run_connector(character: char) -> bool {
    character.is_whitespace() || character.is_ascii_punctuation()
}

fn ascii_insensitive_term_ranges_with_cancellation<E>(
    text: &str,
    excluded_terms: &BTreeSet<String>,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Vec<(usize, usize)>, E> {
    let normalized = ascii_lowercase_with_cancellation(text, &mut ensure_running)?;
    let mut ranges = Vec::new();
    for term in excluded_terms {
        ensure_running()?;
        let mut cursor = 0_usize;
        while let Some(start) = find_language_substring_with_cancellation(
            &normalized,
            cursor,
            term,
            &mut ensure_running,
        )? {
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
            cursor = end;
        }
    }
    merge_byte_ranges_with_cancellation(ranges, ensure_running)
}

fn ascii_words_with_cancellation<E>(
    text: &str,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Vec<EnglishWord>, E> {
    let mut words = Vec::new();
    let mut start = None;
    let mut next_check = 0_usize;
    for (byte_index, character) in text.char_indices() {
        ensure_language_text_progress(byte_index, &mut next_check, &mut ensure_running)?;
        if character.is_ascii_alphabetic() {
            start.get_or_insert(byte_index);
        } else if let Some(word_start) = start.take() {
            words.push(EnglishWord {
                normalized: ascii_lowercase_with_cancellation(
                    &text[word_start..byte_index],
                    &mut ensure_running,
                )?,
                start: word_start,
                end: byte_index,
            });
        }
    }
    if let Some(word_start) = start {
        words.push(EnglishWord {
            normalized: ascii_lowercase_with_cancellation(
                &text[word_start..],
                &mut ensure_running,
            )?,
            start: word_start,
            end: text.len(),
        });
    }
    ensure_running()?;
    Ok(words)
}

fn reaches_word_threshold_with_cancellation<E>(
    words: &[EnglishWord],
    minimum_words: NonZeroUsize,
    minimum_letters: NonZeroUsize,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<bool, E> {
    if words.len() < minimum_words.get() {
        ensure_running()?;
        return Ok(false);
    }
    let mut letters = 0_usize;
    for word in words {
        ensure_running()?;
        letters = letters.wrapping_add(word.normalized.len());
    }
    ensure_running()?;
    Ok(letters >= minimum_letters.get())
}

fn first_copied_english_fragment_with_cancellation<E>(
    text: &str,
    target_run: &[EnglishWord],
    source_runs: &[Vec<String>],
    minimum_words: NonZeroUsize,
    minimum_letters: NonZeroUsize,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Option<String>, E> {
    for start in 0..target_run.len() {
        ensure_running()?;
        for end in (start + 1..=target_run.len()).rev() {
            ensure_running()?;
            let candidate = &target_run[start..end];
            if !reaches_word_threshold_with_cancellation(
                candidate,
                minimum_words,
                minimum_letters,
                &mut ensure_running,
            )? {
                continue;
            }
            let mut copied = false;
            for source in source_runs {
                ensure_running()?;
                for window in source.windows(candidate.len()) {
                    ensure_running()?;
                    let mut equal = true;
                    for (left, right) in window.iter().zip(candidate) {
                        if !language_text_equal_with_cancellation(
                            left,
                            &right.normalized,
                            &mut ensure_running,
                        )? {
                            equal = false;
                            break;
                        }
                    }
                    if equal {
                        copied = true;
                        break;
                    }
                }
                if copied {
                    break;
                }
            }
            if copied {
                return Ok(Some(clone_language_text_with_cancellation(
                    &text[candidate[0].start..candidate[candidate.len() - 1].end],
                    &mut ensure_running,
                )?));
            }
        }
    }
    ensure_running()?;
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn language_id(value: &str) -> LanguageId {
        LanguageId::parse(value).expect("测试语言 ID 应该有效")
    }

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
    fn language_text_normalization_can_cancel_between_long_text_copies() {
        let long = "文".repeat(LANGUAGE_TEXT_CANCELLATION_CHECK_BYTES * 2);
        let mut polls = 0_usize;
        let result = LanguageText::new_with_cancellation(
            vec![
                LanguageTextSegment::NaturalText("前".to_owned()),
                LanguageTextSegment::NaturalText(long),
            ],
            || {
                polls += 1;
                if polls == 4 { Err("cancelled") } else { Ok(()) }
            },
        );

        assert!(matches!(result, Err("cancelled")));
        assert_eq!(polls, 4);
    }

    #[test]
    fn japanese_analysis_can_cancel_inside_a_long_source_segment() {
        let module = japanese_module();
        let source = LanguageText::natural("x".repeat(LANGUAGE_TEXT_CANCELLATION_CHECK_BYTES * 4));
        let mut polls = 0_usize;
        let result = module.analyze_source_with_cancellation(&source, &mut || {
            polls += 1;
            if polls == 4 {
                Err(LanguageOperationCancelled)
            } else {
                Ok(())
            }
        });

        assert_eq!(result, Err(LanguageOperationCancelled));
        assert_eq!(polls, 4);
    }

    #[test]
    fn residual_scan_can_cancel_inside_a_long_translation_segment() {
        let module = japanese_module();
        let analysis = module.analyze_source(&LanguageText::natural("勇者"));
        let translation =
            LanguageText::natural("x".repeat(LANGUAGE_TEXT_CANCELLATION_CHECK_BYTES * 4));
        let mut polls = 0_usize;
        let result =
            module.find_source_residual_with_cancellation(&analysis, &translation, &mut || {
                polls += 1;
                if polls == 6 {
                    Err(LanguageOperationCancelled)
                } else {
                    Ok(())
                }
            });

        assert_eq!(result, Err(LanguageOperationCancelled));
        assert_eq!(polls, 6);
    }

    #[test]
    fn repair_application_can_cancel_while_cloning_long_text() {
        let text = LanguageText::natural("x".repeat(LANGUAGE_TEXT_CANCELLATION_CHECK_BYTES * 4));
        let mut polls = 0_usize;
        let result = text.apply_repair_with_cancellation(&LanguageRepairPlan::unchanged(), || {
            polls += 1;
            if polls == 3 { Err("cancelled") } else { Ok(()) }
        });

        assert!(matches!(result, Err("cancelled")));
        assert_eq!(polls, 3);
    }

    #[test]
    fn semantic_fingerprint_can_cancel_inside_a_long_policy_term() {
        let long_term = "カ".repeat(LANGUAGE_TEXT_CANCELLATION_CHECK_BYTES * 2);
        let module = JapaneseLanguageModule::new(
            JapaneseResidualPolicy::new(non_zero(2), [long_term]).expect("测试策略应有效"),
            None,
        );
        let expected = module.semantic_fingerprint();
        let module: &dyn LanguageModule = &module;
        let mut successful_polls = 0_usize;
        let actual = module
            .semantic_fingerprint_with_cancellation(&mut || {
                successful_polls += 1;
                Ok(())
            })
            .expect("检查不会取消");
        assert_eq!(actual, expected);
        assert!(successful_polls > 2);

        let mut polls = 0_usize;
        let cancelled = module.semantic_fingerprint_with_cancellation(&mut || {
            polls += 1;
            if polls == 4 {
                Err(LanguageOperationCancelled)
            } else {
                Ok(())
            }
        });
        assert_eq!(cancelled, Err(LanguageOperationCancelled));
        assert_eq!(polls, 4);
    }

    #[test]
    fn language_id_validates_and_canonicalizes_rfc_5646_tags() {
        for (input, expected) in [
            ("ja", "ja"),
            ("EN-us", "en-US"),
            ("zh-hans", "zh-Hans"),
            ("en-Latn", "en"),
        ] {
            let language_id = LanguageId::parse(input).expect("语言标签应该有效");
            assert_eq!(language_id.as_str(), expected);
            assert_eq!(
                language_id
                    .to_string()
                    .parse::<LanguageId>()
                    .expect("规范标签应可重新解析"),
                language_id
            );
        }
    }

    #[test]
    fn language_id_rejects_noncanonical_external_forms() {
        for invalid in ["", "   "] {
            assert!(matches!(
                LanguageId::parse(invalid),
                Err(LanguageIdError::Blank)
            ));
        }
        assert!(matches!(
            LanguageId::parse(" ja"),
            Err(LanguageIdError::SurroundingWhitespace { .. })
        ));
        assert!(matches!(
            LanguageId::parse("en_US"),
            Err(LanguageIdError::Underscore { .. })
        ));
        assert!(matches!(
            LanguageId::parse("en--US"),
            Err(LanguageIdError::InvalidSyntax { .. })
        ));
        assert!(matches!(
            LanguageId::parse("zz"),
            Err(LanguageIdError::InvalidRegistryTag { .. })
        ));
        assert!(matches!(
            LanguageId::parse("und-Latn"),
            Err(LanguageIdError::UndefinedPrimaryLanguage { .. })
        ));
    }

    #[test]
    fn language_pair_preserves_typed_source_and_target() {
        let pair = LanguagePair::new(language_id("ja"), language_id("zh-Hans"));

        assert_eq!(pair.source().as_str(), "ja");
        assert_eq!(pair.target().as_str(), "zh-Hans");
        assert_eq!(pair.to_string(), "ja -> zh-Hans");
        assert_eq!(
            pair.into_parts(),
            (language_id("ja"), language_id("zh-Hans"))
        );
    }

    #[test]
    fn catalog_resolves_canonical_source_ids() {
        let module: Arc<dyn LanguageModule> = Arc::new(japanese_module());
        let catalog = LanguageModuleCatalog::new([
            (language_id("ja"), Arc::clone(&module)),
            (language_id("ja-JP"), module),
        ])
        .expect("显式精确绑定有效");

        assert!(catalog.resolve(&language_id("JA")).is_ok());
        assert!(matches!(
            catalog.resolve(&language_id("en")),
            Err(LanguageModuleCatalogError::UnknownLanguageId { .. })
        ));
    }

    #[test]
    fn catalog_rejects_missing_and_canonical_duplicate_ids() {
        assert!(matches!(
            LanguageModuleCatalog::new(std::iter::empty::<(LanguageId, Arc<dyn LanguageModule>,)>()),
            Err(LanguageModuleCatalogBuildError::MissingLanguageModule)
        ));

        let first: Arc<dyn LanguageModule> = Arc::new(japanese_module());
        let second: Arc<dyn LanguageModule> = Arc::new(japanese_module());
        assert!(matches!(
            LanguageModuleCatalog::new([
                (language_id("en-US"), first),
                (language_id("EN-us"), second),
            ]),
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
