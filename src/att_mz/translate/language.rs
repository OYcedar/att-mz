#![allow(dead_code, reason = "语言模块等待 Translate 组合根接线")]

//! 翻译前后共同使用的语言事实。
//!
//! Catalog 只按外部明确绑定的语言 ID 选择实现，不猜测别名，也不提供默认语言。
//! 源语言实现同时负责译前判定和译后残留检查，避免建立两套互相矛盾的语言事实。

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;

/// 源语言对一段原文及译文残留的确定性判断。
pub(crate) trait SourceLanguage: Send + Sync {
    /// 原文是否包含需要交给模型翻译的本语言内容。
    fn needs_translation(&self, text: &str) -> bool;

    /// 返回译文中第一段达到本模块阈值的源语言残留。
    fn find_residual(&self, text_without_tokens: &str) -> Option<SourceResidual>;
}

/// 目标语言对模型结果执行的安全正规化与验收。
pub(crate) trait TargetLanguage: Send + Sync {
    fn normalize_and_validate(
        &self,
        text_without_tokens: &str,
    ) -> Result<String, TargetLanguageError>;
}

/// 一段足以判定译文仍残留源语言的诊断事实。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourceResidual {
    fragment: String,
}

impl SourceResidual {
    pub(crate) fn new(fragment: impl Into<String>) -> Self {
        Self {
            fragment: fragment.into(),
        }
    }

    pub(crate) fn fragment(&self) -> &str {
        &self.fragment
    }
}

/// 目标语言模块能够确定拒绝的模型内容。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TargetLanguageError {
    Blank,
    ContainsByteOrderMark,
}

impl fmt::Display for TargetLanguageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blank => formatter.write_str("译文不能为空白"),
            Self::ContainsByteOrderMark => formatter.write_str("译文包含不允许的 BOM 字符"),
        }
    }
}

impl Error for TargetLanguageError {}

/// 精确语言 ID 到语言实现的受信绑定集合。
#[derive(Clone)]
pub(crate) struct TranslationLanguageCatalog {
    sources: BTreeMap<String, Arc<dyn SourceLanguage>>,
    targets: BTreeMap<String, Arc<dyn TargetLanguage>>,
}

impl TranslationLanguageCatalog {
    pub(crate) fn new(
        source_bindings: impl IntoIterator<Item = (String, Arc<dyn SourceLanguage>)>,
        target_bindings: impl IntoIterator<Item = (String, Arc<dyn TargetLanguage>)>,
    ) -> Result<Self, TranslationLanguageCatalogBuildError> {
        let sources = collect_bindings(source_bindings, LanguageRole::Source)?;
        let targets = collect_bindings(target_bindings, LanguageRole::Target)?;

        if sources.is_empty() {
            return Err(TranslationLanguageCatalogBuildError::MissingSourceLanguage);
        }
        if targets.is_empty() {
            return Err(TranslationLanguageCatalogBuildError::MissingTargetLanguage);
        }

        Ok(Self { sources, targets })
    }

    pub(crate) fn source(
        &self,
        language_id: &str,
    ) -> Result<&dyn SourceLanguage, TranslationLanguageCatalogError> {
        self.sources
            .get(language_id)
            .map(Arc::as_ref)
            .ok_or_else(|| TranslationLanguageCatalogError::UnknownSourceLanguage {
                language_id: language_id.to_owned(),
                available_ids: self.sources.keys().cloned().collect(),
            })
    }

    pub(crate) fn source_arc(
        &self,
        language_id: &str,
    ) -> Result<Arc<dyn SourceLanguage>, TranslationLanguageCatalogError> {
        self.sources.get(language_id).cloned().ok_or_else(|| {
            TranslationLanguageCatalogError::UnknownSourceLanguage {
                language_id: language_id.to_owned(),
                available_ids: self.sources.keys().cloned().collect(),
            }
        })
    }

    pub(crate) fn target(
        &self,
        language_id: &str,
    ) -> Result<&dyn TargetLanguage, TranslationLanguageCatalogError> {
        self.targets
            .get(language_id)
            .map(Arc::as_ref)
            .ok_or_else(|| TranslationLanguageCatalogError::UnknownTargetLanguage {
                language_id: language_id.to_owned(),
                available_ids: self.targets.keys().cloned().collect(),
            })
    }
}

impl fmt::Debug for TranslationLanguageCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TranslationLanguageCatalog")
            .field("source_ids", &self.sources.keys().collect::<Vec<_>>())
            .field("target_ids", &self.targets.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LanguageRole {
    Source,
    Target,
}

fn collect_bindings<T: ?Sized>(
    bindings: impl IntoIterator<Item = (String, Arc<T>)>,
    role: LanguageRole,
) -> Result<BTreeMap<String, Arc<T>>, TranslationLanguageCatalogBuildError> {
    let mut result = BTreeMap::new();
    for (language_id, implementation) in bindings {
        if language_id.trim().is_empty() {
            return Err(TranslationLanguageCatalogBuildError::BlankLanguageId { role });
        }
        if language_id.trim() != language_id {
            return Err(
                TranslationLanguageCatalogBuildError::SurroundingWhitespaceInLanguageId {
                    role,
                    language_id,
                },
            );
        }
        if result.insert(language_id.clone(), implementation).is_some() {
            return Err(TranslationLanguageCatalogBuildError::DuplicateLanguageId {
                role,
                language_id,
            });
        }
    }
    Ok(result)
}

/// 外部语言绑定不能建立为精确 Catalog。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationLanguageCatalogBuildError {
    MissingSourceLanguage,
    MissingTargetLanguage,
    BlankLanguageId {
        role: LanguageRole,
    },
    SurroundingWhitespaceInLanguageId {
        role: LanguageRole,
        language_id: String,
    },
    DuplicateLanguageId {
        role: LanguageRole,
        language_id: String,
    },
}

impl fmt::Display for TranslationLanguageCatalogBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSourceLanguage => formatter.write_str("没有绑定任何源语言实现"),
            Self::MissingTargetLanguage => formatter.write_str("没有绑定任何目标语言实现"),
            Self::BlankLanguageId { role } => write!(formatter, "{}语言 ID 为空", role.name()),
            Self::SurroundingWhitespaceInLanguageId { role, language_id } => write!(
                formatter,
                "{}语言 ID 含首尾空白：{language_id:?}",
                role.name()
            ),
            Self::DuplicateLanguageId { role, language_id } => {
                write!(formatter, "{}语言 ID 重复：{language_id}", role.name())
            }
        }
    }
}

impl Error for TranslationLanguageCatalogBuildError {}

impl LanguageRole {
    const fn name(self) -> &'static str {
        match self {
            Self::Source => "源",
            Self::Target => "目标",
        }
    }
}

/// 本次项目记录中的语言 ID 没有对应实现。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationLanguageCatalogError {
    UnknownSourceLanguage {
        language_id: String,
        available_ids: Vec<String>,
    },
    UnknownTargetLanguage {
        language_id: String,
        available_ids: Vec<String>,
    },
}

impl fmt::Display for TranslationLanguageCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSourceLanguage {
                language_id,
                available_ids,
            } => write!(
                formatter,
                "未知源语言 ID {language_id:?}；可用 ID：{}",
                available_ids.join(", ")
            ),
            Self::UnknownTargetLanguage {
                language_id,
                available_ids,
            } => write!(
                formatter,
                "未知目标语言 ID {language_id:?}；可用 ID：{}",
                available_ids.join(", ")
            ),
        }
    }
}

impl Error for TranslationLanguageCatalogError {}

/// 日文源语言实现。
///
/// 译前把假名或 CJK 表意文字视为日文候选；译后只把假名作为确定残留，避免把
/// 合法简体中文汉字误判成日文残留。
#[derive(Clone, Copy, Debug)]
pub(crate) struct JapaneseSourceLanguage;

impl SourceLanguage for JapaneseSourceLanguage {
    fn needs_translation(&self, text: &str) -> bool {
        text.chars().any(is_japanese_source_character)
    }

    fn find_residual(&self, text_without_tokens: &str) -> Option<SourceResidual> {
        contiguous_fragment(text_without_tokens, is_japanese_kana).map(SourceResidual::new)
    }
}

fn is_japanese_source_character(character: char) -> bool {
    is_japanese_kana(character) || matches!(character as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF)
}

fn is_japanese_kana(character: char) -> bool {
    matches!(character as u32, 0x3040..=0x30FF | 0x31F0..=0x31FF | 0xFF65..=0xFF9F)
}

/// 英文源语言实现全部由外部阈值和允许项建立。
#[derive(Clone, Debug)]
pub(crate) struct EnglishSourceLanguage {
    minimum_word_count: NonZeroUsize,
    minimum_letter_count: NonZeroUsize,
    allowed_terms: BTreeSet<String>,
}

impl EnglishSourceLanguage {
    pub(crate) fn new(
        minimum_word_count: NonZeroUsize,
        minimum_letter_count: NonZeroUsize,
        allowed_terms: impl IntoIterator<Item = String>,
    ) -> Result<Self, EnglishSourceLanguageConfigurationError> {
        let mut terms = BTreeSet::new();
        for term in allowed_terms {
            if term.trim().is_empty() {
                return Err(EnglishSourceLanguageConfigurationError::Blank);
            }
            if term.trim() != term {
                return Err(
                    EnglishSourceLanguageConfigurationError::SurroundingWhitespace { term },
                );
            }
            if !terms.insert(term.clone()) {
                return Err(EnglishSourceLanguageConfigurationError::Duplicate { term });
            }
        }
        Ok(Self {
            minimum_word_count,
            minimum_letter_count,
            allowed_terms: terms,
        })
    }

    fn significant_words<'a>(&self, text: &'a str) -> Vec<&'a str> {
        ascii_words(text)
            .filter(|word| !self.allowed_terms.contains(*word))
            .collect()
    }

    fn reaches_threshold(&self, words: &[&str]) -> bool {
        words.len() >= self.minimum_word_count.get()
            && words.iter().map(|word| word.len()).sum::<usize>() >= self.minimum_letter_count.get()
    }
}

impl SourceLanguage for EnglishSourceLanguage {
    fn needs_translation(&self, text: &str) -> bool {
        self.reaches_threshold(&self.significant_words(text))
    }

    fn find_residual(&self, text_without_tokens: &str) -> Option<SourceResidual> {
        let words = self.significant_words(text_without_tokens);
        self.reaches_threshold(&words)
            .then(|| SourceResidual::new(words.join(" ")))
    }
}

fn ascii_words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|character: char| !character.is_ascii_alphabetic())
        .filter(|word| !word.is_empty())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum EnglishSourceLanguageConfigurationError {
    Blank,
    SurroundingWhitespace { term: String },
    Duplicate { term: String },
}

impl fmt::Display for EnglishSourceLanguageConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blank => formatter.write_str("英文残留允许项不能为空白"),
            Self::SurroundingWhitespace { term } => {
                write!(formatter, "英文残留允许项含首尾空白：{term:?}")
            }
            Self::Duplicate { term } => {
                write!(formatter, "英文残留允许项重复：{term}")
            }
        }
    }
}

impl Error for EnglishSourceLanguageConfigurationError {}

/// 简体中文目标语言实现，仅执行确定且不会改变措辞的正规化。
#[derive(Clone, Copy, Debug)]
pub(crate) struct SimplifiedChineseTargetLanguage;

impl TargetLanguage for SimplifiedChineseTargetLanguage {
    fn normalize_and_validate(
        &self,
        text_without_tokens: &str,
    ) -> Result<String, TargetLanguageError> {
        if text_without_tokens.contains('\u{feff}') {
            return Err(TargetLanguageError::ContainsByteOrderMark);
        }
        let normalized = text_without_tokens
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        if normalized.trim().is_empty() {
            return Err(TargetLanguageError::Blank);
        }
        Ok(normalized)
    }
}

fn contiguous_fragment(text: &str, mut predicate: impl FnMut(char) -> bool) -> Option<String> {
    let mut result = String::new();
    let mut collecting = false;
    for character in text.chars() {
        if predicate(character) {
            collecting = true;
            result.push(character);
        } else if collecting {
            break;
        }
    }
    (!result.is_empty()).then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_uses_exact_ids_and_allows_aliases_to_share_implementation() {
        let japanese: Arc<dyn SourceLanguage> = Arc::new(JapaneseSourceLanguage);
        let chinese: Arc<dyn TargetLanguage> = Arc::new(SimplifiedChineseTargetLanguage);
        let catalog = TranslationLanguageCatalog::new(
            [
                ("ja".to_owned(), Arc::clone(&japanese)),
                ("ja-JP".to_owned(), japanese),
            ],
            [
                ("zh-Hans".to_owned(), Arc::clone(&chinese)),
                ("zh-CN".to_owned(), chinese),
            ],
        )
        .expect("精确别名绑定应该有效");

        assert!(catalog.source("ja").is_ok());
        assert!(catalog.target("zh-CN").is_ok());
        assert!(matches!(
            catalog.source("JA"),
            Err(TranslationLanguageCatalogError::UnknownSourceLanguage { .. })
        ));
    }

    #[test]
    fn japanese_preflight_accepts_kanji_but_residual_requires_kana() {
        let language = JapaneseSourceLanguage;

        assert!(language.needs_translation("魔法剣"));
        assert_eq!(
            language
                .find_residual("这是翻译です")
                .expect("假名应被视为确定残留")
                .fragment(),
            "です"
        );
        assert_eq!(language.find_residual("这是魔法剑"), None);
    }

    #[test]
    fn english_thresholds_and_allowlist_are_external_facts() {
        let language = EnglishSourceLanguage::new(
            NonZeroUsize::new(2).expect("常量非零"),
            NonZeroUsize::new(8).expect("常量非零"),
            ["HP".to_owned()],
        )
        .expect("英文配置应该有效");

        assert!(!language.needs_translation("HP recovered"));
        assert!(language.needs_translation("Magic Sword"));
        assert_eq!(
            language
                .find_residual("仍有 Magic Sword")
                .expect("达到阈值的英文应该被报告")
                .fragment(),
            "Magic Sword"
        );
    }

    #[test]
    fn simplified_chinese_only_normalizes_line_endings() {
        let language = SimplifiedChineseTargetLanguage;
        assert_eq!(
            language
                .normalize_and_validate("第一行\r\n第二行\r第三行")
                .expect("非空译文应该通过"),
            "第一行\n第二行\n第三行"
        );
        assert_eq!(
            language.normalize_and_validate(" \n "),
            Err(TargetLanguageError::Blank)
        );
    }

    #[test]
    fn invalid_catalog_bindings_fail_without_fallback() {
        let japanese: Arc<dyn SourceLanguage> = Arc::new(JapaneseSourceLanguage);
        let chinese: Arc<dyn TargetLanguage> = Arc::new(SimplifiedChineseTargetLanguage);

        assert!(matches!(
            TranslationLanguageCatalog::new(
                [(" ja".to_owned(), japanese)],
                [("zh-Hans".to_owned(), chinese)]
            ),
            Err(TranslationLanguageCatalogBuildError::SurroundingWhitespaceInLanguageId { .. })
        ));
    }
}
