//! Standard 与可信 Lua 共同使用的一次翻译语义快照。

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::att_mz::text::TextGroupKind;
use crate::fingerprint::Sha256Fingerprint;
use crate::language::{LanguageAnalysis, LanguageModule};

use super::executor::accept_prepared_translation_candidate;
use super::language_projection::{LanguageTextProjectionError, project_protected_text};
use super::placeholder::{
    CompiledPlaceholderRules, Pcre2PlaceholderService, PlaceholderProtectionError,
};
use super::planning_resource::CompiledTerminology;
use super::standard::{
    AppliedPlaceholder, TerminologyDependency, TranslationLanguagePair,
    TranslationUnitRejectionReason,
};

/// 一轮 Standard 与 Lua 共享且不可变的当前翻译语义。
pub(crate) struct ResolvedTranslationSemantics {
    system_prompt: String,
    language_pair: TranslationLanguagePair,
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
        system_prompt: String,
        language_pair: TranslationLanguagePair,
        terminology: Arc<CompiledTerminology>,
        placeholder_service: Pcre2PlaceholderService,
        custom_placeholders: CompiledPlaceholderRules,
        source_language: Arc<dyn LanguageModule>,
        global_fingerprint: Sha256Fingerprint,
    ) -> Self {
        Self {
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

        use crate::language::{JapaneseLanguageModule, JapaneseResidualPolicy};

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
            "test system".to_owned(),
            TranslationLanguagePair::new("ja", "zh-Hans"),
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

    pub(crate) fn language_pair(&self) -> &TranslationLanguagePair {
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
        let (model_text, placeholders) = self
            .placeholder_service
            .protect(kind, original, &self.custom_placeholders)
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
        let terms = self
            .terminology
            .triggered_indices([original])
            .into_iter()
            .map(|index| self.terminology.entries()[index].dependency())
            .collect();
        Ok(PreparedTranslationText {
            status,
            model_text,
            terms,
            placeholders,
            language_analysis,
            source_language: Arc::clone(&self.source_language),
        })
    }
}

/// 当前叶子相对于源语言与保护规则的处理状态。
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

/// `prepare` 建立的不可伪造、可跨 worker 持有的叶子验收句柄。
#[derive(Clone)]
pub(crate) struct PreparedTranslationText {
    status: PreparedTranslationStatus,
    model_text: String,
    terms: Vec<TerminologyDependency>,
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

    pub(super) fn placeholders(&self) -> &[AppliedPlaceholder] {
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
