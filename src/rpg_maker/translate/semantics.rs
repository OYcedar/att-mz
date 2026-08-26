//! RPG Maker 翻译规划和结果验收共同使用的一次语义快照。

#[cfg(test)]
use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::language::{LanguageAnalysis, LanguageModule, LanguagePair, LanguageTextSegment};
use crate::rpg_maker::RpgMakerEngine;
use crate::rpg_maker::location_codec::{RpgMakerProjectionCodec, RpgMakerProjectionCodecError};
use crate::rpg_maker::model::TextUnitContent;
use crate::rpg_maker::semantic_order::{
    RpgMakerSemanticOrderKey, RpgMakerSemanticOrderKeyEncodeError,
};
use crate::rpg_maker::text::TextGroupKind;
use crate::translation::placeholder_projection::{
    LanguageTextProjectionError, SourceBoundPlaceholderError,
    project_protected_text_from_shared_with_cancellation, project_protected_text_with_cancellation,
};

#[cfg(test)]
use super::executor::accept_prepared_translation_candidate;
#[cfg(test)]
use super::pipeline::TranslationUnitRejectionReason;
use super::pipeline::{AppliedPlaceholder, GroupContextFingerprint, TranslationUnitIdentity};
use super::placeholder::{
    CompiledPlaceholderRules, Pcre2PlaceholderService, PlaceholderProtectionError,
    RpgMakerBuiltinPlaceholderProfile, RpgMakerSourceBoundPlaceholderError,
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
}

impl fmt::Debug for ResolvedTranslationSemantics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedTranslationSemantics")
            .field("engine", &self.engine)
            .field("language_pair", &self.language_pair)
            .field("term_count", &self.terminology.entries().len())
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
    pub(crate) fn new(
        engine: RpgMakerEngine,
        language_pair: LanguagePair,
        terminology: Arc<CompiledTerminology>,
        placeholder_service: Pcre2PlaceholderService,
        custom_placeholders: CompiledPlaceholderRules,
        source_language: Arc<dyn LanguageModule>,
    ) -> Self {
        Self {
            engine,
            language_pair,
            terminology,
            placeholder_service,
            custom_placeholders,
            source_language,
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
        )
    }

    pub(crate) const fn language_pair(&self) -> &LanguagePair {
        &self.language_pair
    }

    /// 当前 Unit 的完整候选校验契约身份；源文零命中时也不同。
    pub(crate) fn candidate_contract_fingerprint(
        &self,
        identity: &TranslationUnitIdentity,
    ) -> Sha256Fingerprint {
        let target_id = identity.readable_id();
        let custom = self
            .custom_placeholders
            .applicable_contract_fingerprint(identity.kind().storage_name(), &target_id);
        let mut hasher = Sha256FramedHasher::new(b"att.rpg-maker-candidate-contract");
        hasher
            .frame(1, self.engine.storage_name().as_bytes())
            .frame(2, custom.as_bytes())
            .frame(
                3,
                RpgMakerBuiltinPlaceholderProfile::for_identity(self.engine, identity)
                    .fingerprint_name()
                    .as_bytes(),
            );
        hasher.finish()
    }

    /// 对已经恢复成 RPG Maker 原始文本的候选重新应用当前 Unit 的完整 consumer 与
    /// exact-id 自定义规则。源文没有命中控制符时也必须执行，这样候选新增控制符不会
    /// 绕过模型 token 阶段。
    pub(crate) fn candidate_placeholders_match_with_cancellation<E>(
        &self,
        identity: &TranslationUnitIdentity,
        candidate: &TextUnitContent,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<bool, ResolvedTranslationSemanticError>, E> {
        let target_id = identity.readable_id();
        let kind = identity.kind();
        let profile = RpgMakerBuiltinPlaceholderProfile::for_identity(self.engine, identity);
        let mut validate_slot = |source: &str, candidate: &str| {
            let source = match self.placeholder_service.protect_profile_with_cancellation(
                self.engine,
                kind,
                &target_id,
                profile,
                source,
                &[],
                &self.custom_placeholders,
                &mut ensure_running,
            )? {
                Ok(source) => source,
                Err(source) => {
                    return Ok(Err(ResolvedTranslationSemanticError::ProtectPlaceholder(
                        source,
                    )));
                }
            };
            match self
                .placeholder_service
                .bind_profile_candidate_with_cancellation(
                    &source,
                    self.engine,
                    kind,
                    &target_id,
                    profile,
                    candidate,
                    &self.custom_placeholders,
                    &mut ensure_running,
                )? {
                Ok(_) => Ok(Ok(true)),
                Err(RpgMakerSourceBoundPlaceholderError::Protection(source)) => Ok(Err(
                    ResolvedTranslationSemanticError::ProtectPlaceholder(source),
                )),
                Err(RpgMakerSourceBoundPlaceholderError::Binding(
                    SourceBoundPlaceholderError::Projection(source),
                )) => Ok(Err(ResolvedTranslationSemanticError::ProjectLanguageText(
                    source,
                ))),
                Err(RpgMakerSourceBoundPlaceholderError::Binding(
                    SourceBoundPlaceholderError::Multiset(_)
                    | SourceBoundPlaceholderError::AmbiguousOriginal { .. }
                    | SourceBoundPlaceholderError::UnexpectedPlaceholder,
                )) => Ok(Ok(false)),
            }
        };
        match (identity.source_content(), candidate) {
            (TextUnitContent::Value(source), TextUnitContent::Value(candidate)) => {
                validate_slot(source, candidate)
            }
            (TextUnitContent::Lines(source), TextUnitContent::Lines(candidate))
                if identity.role().preserves_placeholder_line_slots()
                    && source.len() == candidate.len() =>
            {
                for (source, candidate) in source.iter().zip(candidate) {
                    match validate_slot(source, candidate)? {
                        Ok(true) => {}
                        result => return Ok(result),
                    }
                }
                ensure_running()?;
                Ok(Ok(true))
            }
            (TextUnitContent::Lines(source), TextUnitContent::Lines(candidate))
                if !identity.role().preserves_placeholder_line_slots() =>
            {
                validate_slot(&source.join("\n"), &candidate.join("\n"))
            }
            _ => {
                ensure_running()?;
                Ok(Ok(false))
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn prepare(
        &self,
        kind: TextGroupKind,
        original: &str,
    ) -> Result<PreparedTranslationText, ResolvedTranslationSemanticError> {
        let facts = match prepare_translation_resource_text_with_cancellation(
            self.engine,
            kind,
            "test",
            test_profile(self.engine, kind),
            original,
            &[],
            self.terminology.as_ref(),
            &self.placeholder_service,
            &self.custom_placeholders,
            &mut || Ok::<_, Infallible>(()),
        ) {
            Ok(result) => result?,
            Err(unreachable) => match unreachable {},
        };
        self.finish_prepared_text(facts)
    }

    /// RPG Maker 的 `Lines` 保持完整原文参与 Placeholder 与语言分析，但 opaque 保护和
    /// 术语都不能跨越两个物理数组元素。`Value` 没有这层边界，其中的 LF 仍是普通内容。
    #[cfg(test)]
    pub(crate) fn prepare_content(
        &self,
        kind: TextGroupKind,
        content: &TextUnitContent,
    ) -> Result<PreparedTranslationText, ResolvedTranslationSemanticError> {
        match prepare_translation_resource_facts_for_contract_with_cancellation(
            self.engine,
            kind,
            "test",
            test_profile(self.engine, kind),
            content,
            self.terminology.as_ref(),
            &self.placeholder_service,
            &self.custom_placeholders,
            || Ok::<_, Infallible>(()),
        ) {
            Ok(Ok(facts)) => self.finish_prepared_text(facts),
            Ok(Err(source)) => Err(source),
            Err(unreachable) => match unreachable {},
        }
    }

    /// 保持生产译前语义，并在大文本复制、术语扫描和状态建立之间轮询取消。
    pub(crate) fn prepare_identity_content_with_cancellation<E>(
        &self,
        identity: &TranslationUnitIdentity,
        content: &TextUnitContent,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Result<PreparedTranslationText, ResolvedTranslationSemanticError>, E> {
        ensure_running()?;
        let facts = match prepare_translation_resource_facts_with_cancellation(
            self.engine,
            identity,
            content,
            self.terminology.as_ref(),
            &self.placeholder_service,
            &self.custom_placeholders,
            &mut ensure_running,
        )? {
            Ok(facts) => facts,
            Err(source) => return Ok(Err(source)),
        };
        ensure_running()?;
        let prepared = self.finish_prepared_text(facts);
        ensure_running()?;
        Ok(prepared)
    }

    fn finish_prepared_text(
        &self,
        facts: TranslationResourceFacts,
    ) -> Result<PreparedTranslationText, ResolvedTranslationSemanticError> {
        let TranslationResourceFacts {
            model_text,
            term_indices,
            placeholders,
            language_text,
        } = facts;
        let language_analysis = self.source_language.analyze_source(&language_text);
        let status = if !language_text.has_non_whitespace_natural_text() {
            PreparedTranslationStatus::FullyProtected
        } else if language_analysis.needs_translation() {
            PreparedTranslationStatus::Active
        } else {
            PreparedTranslationStatus::NonSourceLanguage
        };
        Ok(PreparedTranslationText {
            status,
            model_text,
            term_indices,
            placeholders,
            language_analysis,
            #[cfg(test)]
            source_language: Arc::clone(&self.source_language),
        })
    }
}

#[cfg(test)]
fn test_profile(engine: RpgMakerEngine, kind: TextGroupKind) -> RpgMakerBuiltinPlaceholderProfile {
    use crate::rpg_maker::asset::RpgMakerAssetOwner;
    use crate::rpg_maker::model::{ScalarFieldKey, TextUnitRole};
    use crate::rpg_maker::text::{RpgMakerLocation, RpgMakerSource, StandardDataFile};

    let (location, role) = match kind {
        TextGroupKind::DatabaseEntry => (
            RpgMakerLocation::value(
                RpgMakerSource::data(StandardDataFile::Items),
                vec![crate::rpg_maker::text::RpgMakerLocationStep::index(1)],
            ),
            TextUnitRole::Scalar(ScalarFieldKey::new("description").expect("测试 role 应合法")),
        ),
        TextGroupKind::System => (
            RpgMakerLocation::value(RpgMakerSource::data(StandardDataFile::System), Vec::new()),
            TextUnitRole::Scalar(ScalarFieldKey::new("gameTitle").expect("测试 role 应合法")),
        ),
        TextGroupKind::Map => (
            RpgMakerLocation::value(RpgMakerSource::map(1), Vec::new()),
            TextUnitRole::Scalar(ScalarFieldKey::new("displayName").expect("测试 role 应合法")),
        ),
        TextGroupKind::EventDialogue => (
            RpgMakerLocation::value(RpgMakerSource::map(1), Vec::new()),
            TextUnitRole::DialogueBody,
        ),
        TextGroupKind::EventChoices => (
            RpgMakerLocation::value(RpgMakerSource::map(1), Vec::new()),
            TextUnitRole::Choices,
        ),
        TextGroupKind::EventScrollingText => (
            RpgMakerLocation::value(RpgMakerSource::map(1), Vec::new()),
            TextUnitRole::ScrollingText,
        ),
        TextGroupKind::EventCommand => (
            RpgMakerLocation::value(RpgMakerSource::map(1), Vec::new()),
            TextUnitRole::Scalar(ScalarFieldKey::new("nickname").expect("测试 role 应合法")),
        ),
        TextGroupKind::PluginParameter => (
            RpgMakerLocation::value(
                RpgMakerSource::plugin_parameter(0, "Plugin", "Parameter"),
                Vec::new(),
            ),
            TextUnitRole::Scalar(ScalarFieldKey::new("value").expect("测试 role 应合法")),
        ),
    };
    RpgMakerBuiltinPlaceholderProfile::for_location(
        engine,
        RpgMakerAssetOwner::Builtin,
        kind,
        &location,
        &role,
    )
}

/// 已由当前编译后 Placeholder 与 Terminology 共同建立的 Unit 资源事实。
///
/// 字段保持私有，调用方不能用原始 JSON 假装已经完成规则编译和实际命中计算。
pub(crate) struct TranslationResourceFacts {
    model_text: String,
    term_indices: Vec<usize>,
    placeholders: Vec<AppliedPlaceholder>,
    language_text: crate::language::LanguageText,
}

/// 用已经编译的当前资源计算一个 Unit 实际命中的 Placeholder 与 Terminology。
///
/// 外层错误只表示取消；内层错误表示当前文本无法按已编译资源建立语义。
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_translation_resource_facts_with_cancellation<E>(
    engine: RpgMakerEngine,
    identity: &TranslationUnitIdentity,
    content: &TextUnitContent,
    terminology: &CompiledTerminology,
    placeholder_service: &Pcre2PlaceholderService,
    custom_placeholders: &CompiledPlaceholderRules,
    ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Result<TranslationResourceFacts, ResolvedTranslationSemanticError>, E> {
    let kind = identity.kind();
    let target_id = identity.readable_id();
    prepare_translation_resource_facts_for_contract_with_cancellation(
        engine,
        kind,
        &target_id,
        RpgMakerBuiltinPlaceholderProfile::for_identity(engine, identity),
        content,
        terminology,
        placeholder_service,
        custom_placeholders,
        ensure_running,
    )
}

#[allow(clippy::too_many_arguments)]
fn prepare_translation_resource_facts_for_contract_with_cancellation<E>(
    engine: RpgMakerEngine,
    kind: TextGroupKind,
    target_id: &str,
    profile: RpgMakerBuiltinPlaceholderProfile,
    content: &TextUnitContent,
    terminology: &CompiledTerminology,
    placeholder_service: &Pcre2PlaceholderService,
    custom_placeholders: &CompiledPlaceholderRules,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Result<TranslationResourceFacts, ResolvedTranslationSemanticError>, E> {
    ensure_running()?;
    match content {
        TextUnitContent::Value(original) => prepare_translation_resource_text_with_cancellation(
            engine,
            kind,
            target_id,
            profile,
            original,
            &[],
            terminology,
            placeholder_service,
            custom_placeholders,
            &mut ensure_running,
        ),
        TextUnitContent::Lines(lines) => {
            let (original, line_separators) =
                join_lines_with_cancellation(lines, &mut ensure_running)?;
            prepare_translation_resource_text_with_cancellation(
                engine,
                kind,
                target_id,
                profile,
                &original,
                &line_separators,
                terminology,
                placeholder_service,
                custom_placeholders,
                &mut ensure_running,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn prepare_translation_resource_text_with_cancellation<E>(
    engine: RpgMakerEngine,
    kind: TextGroupKind,
    target_id: &str,
    profile: RpgMakerBuiltinPlaceholderProfile,
    original: &str,
    line_separators: &[usize],
    terminology: &CompiledTerminology,
    placeholder_service: &Pcre2PlaceholderService,
    custom_placeholders: &CompiledPlaceholderRules,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Result<TranslationResourceFacts, ResolvedTranslationSemanticError>, E> {
    ensure_running()?;
    let (model_text, placeholders) = match placeholder_service.protect_profile_with_cancellation(
        engine,
        kind,
        target_id,
        profile,
        original,
        line_separators,
        custom_placeholders,
        &mut *ensure_running,
    )? {
        Ok(protected) => protected.into_parts(),
        Err(source) => {
            return Ok(Err(ResolvedTranslationSemanticError::ProtectPlaceholder(
                source,
            )));
        }
    };
    ensure_running()?;
    let language_text = match project_protected_text_with_cancellation(
        &model_text,
        &placeholders,
        &mut *ensure_running,
    )? {
        Ok(language_text) => language_text,
        Err(source) => {
            return Ok(Err(ResolvedTranslationSemanticError::ProjectLanguageText(
                source,
            )));
        }
    };
    ensure_running()?;

    let term_indices = if line_separators.is_empty() {
        terminology.triggered_indices_with_cancellation(
            natural_segments(&language_text),
            &mut *ensure_running,
        )?
    } else {
        let domains = terminology_line_domains_with_cancellation(
            original,
            &model_text,
            &placeholders,
            line_separators,
            ensure_running,
        )?;
        let mut matched = vec![false; terminology.entries().len()];
        for domain in domains {
            ensure_running()?;
            let mut domain_placeholders = Vec::new();
            for placeholder in &placeholders {
                ensure_running()?;
                if text_contains_with_cancellation(domain, placeholder.token(), ensure_running)? {
                    domain_placeholders.push(clone_placeholder_with_cancellation(
                        placeholder,
                        ensure_running,
                    )?);
                }
            }
            let projected = match project_protected_text_from_shared_with_cancellation(
                domain,
                Arc::new(domain_placeholders),
                &mut *ensure_running,
            )? {
                Ok(projected) => projected,
                Err(source) => {
                    return Ok(Err(ResolvedTranslationSemanticError::ProjectLanguageText(
                        source,
                    )));
                }
            };
            for index in terminology.triggered_indices_with_cancellation(
                natural_segments(&projected),
                &mut *ensure_running,
            )? {
                ensure_running()?;
                matched[index] = true;
            }
        }
        let mut indices = Vec::new();
        for (index, matched) in matched.into_iter().enumerate() {
            ensure_running()?;
            if matched {
                indices.push(index);
            }
        }
        indices
    };

    ensure_running()?;
    Ok(Ok(TranslationResourceFacts {
        model_text,
        term_indices,
        placeholders,
        language_text,
    }))
}

/// 建立一个完整 Group 的稳定来源语境指纹。
///
/// 调用方必须按完整自然顺序提供 Group 的全部 Unit。译文、Current、模型责任、
/// TaskBlock 邻居、Group 在外部 Scope 中的顺序和临时 ID 都不进入该指纹。
pub(crate) fn group_context_fingerprint_with_cancellation<'a, E>(
    kind: TextGroupKind,
    units: impl ExactSizeIterator<Item = (&'a RpgMakerSemanticOrderKey, &'a TranslationUnitIdentity)>,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Result<GroupContextFingerprint, GroupContextFingerprintError>, E> {
    ensure_running()?;
    let mut encoded_units = Vec::with_capacity(units.len());
    for (unit_order_key, identity) in units {
        ensure_running()?;
        let unit_order = match unit_order_key.encode() {
            Ok(unit_order) => unit_order,
            Err(source) => {
                return Ok(Err(GroupContextFingerprintError::SemanticOrder(source)));
            }
        };
        let role = match RpgMakerProjectionCodec::encode_role(identity.role()) {
            Ok(role) => role,
            Err(source) => return Ok(Err(GroupContextFingerprintError::Role(source))),
        };
        let source_content_json = serde_json::to_string(identity.source_content())
            .expect("受信 TextUnitContent 必须可序列化为规范 JSON");
        encoded_units.push((
            role,
            unit_order,
            source_content_json,
            identity.source_context_json().to_owned(),
        ));
    }
    let fingerprint = crate::translation::rpg_maker_group_source_context_v2_with_cancellation(
        kind.storage_name(),
        encoded_units.iter().map(|(role, order, source, context)| {
            (
                role.as_str(),
                order.as_slice(),
                source.as_str(),
                context.as_str(),
            )
        }),
        ensure_running,
    )?;
    Ok(Ok(GroupContextFingerprint::new(fingerprint)))
}

/// 完整 Group 语境指纹无法编码受信的顺序或 Unit 身份。
#[derive(Debug)]
pub(crate) enum GroupContextFingerprintError {
    SemanticOrder(RpgMakerSemanticOrderKeyEncodeError),
    Role(RpgMakerProjectionCodecError),
}

impl fmt::Display for GroupContextFingerprintError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SemanticOrder(source) => write!(formatter, "无法编码语义顺序：{source}"),
            Self::Role(source) => write!(formatter, "无法编码 Unit 角色：{source}"),
        }
    }
}

impl Error for GroupContextFingerprintError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SemanticOrder(source) => Some(source),
            Self::Role(source) => Some(source),
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

fn join_lines_with_cancellation<E>(
    lines: &[String],
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<(String, Vec<usize>), E> {
    ensure_running()?;
    let mut capacity = 0_usize;
    for line in lines {
        ensure_running()?;
        capacity = capacity.saturating_add(line.len()).saturating_add(1);
    }
    capacity = capacity.saturating_sub(usize::from(!lines.is_empty()));
    let mut original = String::with_capacity(capacity);
    let mut offsets = Vec::with_capacity(lines.len().saturating_sub(1));
    let mut cursor = 0;
    for (index, line) in lines.iter().enumerate() {
        append_text_with_cancellation(&mut original, line, ensure_running)?;
        cursor += line.len();
        if index + 1 < lines.len() {
            offsets.push(cursor);
            original.push('\n');
            cursor += 1;
        }
    }
    ensure_running()?;
    Ok((original, offsets))
}

/// 把 Lines 分隔 LF 映射到 Placeholder 投影后的模型文本，并据此切开术语扫描域。
///
/// 译前保护边界已经保证不透明跨度不会吞掉这些分隔符。
fn terminology_line_domains_with_cancellation<'a, E>(
    original: &str,
    model_text: &'a str,
    placeholders: &[AppliedPlaceholder],
    line_separators: &[usize],
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Vec<&'a str>, E> {
    ensure_running()?;
    let mut mapped = Vec::with_capacity(line_separators.len());
    let mut separator_index = 0;
    let mut source_cursor = 0;
    let mut model_cursor = 0;

    for placeholder in placeholders {
        ensure_running()?;
        let token_offset = find_text_with_cancellation(
            &model_text[model_cursor..],
            placeholder.token(),
            ensure_running,
        )?
        .expect("Placeholder 投影已经保证每个 token 在模型文本中恰好出现一次");
        let token_start = model_cursor + token_offset;
        let source_span_start = source_cursor + token_offset;
        let source_span_end = source_span_start + placeholder.original().len();

        debug_assert!(
            text_eq_with_cancellation(
                &original[source_cursor..source_span_start],
                &model_text[model_cursor..token_start],
                ensure_running,
            )?,
            "Placeholder 之前的自然文本必须逐字保持",
        );
        debug_assert!(
            text_eq_with_cancellation(
                &original[source_span_start..source_span_end],
                placeholder.original(),
                ensure_running,
            )?,
            "Placeholder 绑定必须对应原文中的当前源跨度",
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
        ensure_running()?;
        mapped.push(model_cursor + separator - source_cursor);
    }

    let mut domains = Vec::with_capacity(mapped.len() + 1);
    let mut start = 0;
    for separator in mapped {
        ensure_running()?;
        debug_assert_eq!(model_text.as_bytes().get(separator), Some(&b'\n'));
        domains.push(&model_text[start..separator]);
        start = separator + 1;
    }
    domains.push(&model_text[start..]);
    ensure_running()?;
    Ok(domains)
}

fn clone_text_with_cancellation<E>(
    source: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<String, E> {
    let mut output = String::with_capacity(source.len());
    append_text_with_cancellation(&mut output, source, ensure_running)?;
    Ok(output)
}

fn clone_placeholder_with_cancellation<E>(
    placeholder: &AppliedPlaceholder,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<AppliedPlaceholder, E> {
    Ok(AppliedPlaceholder::new_with_contract_and_identity(
        clone_text_with_cancellation(placeholder.token(), ensure_running)?,
        clone_text_with_cancellation(placeholder.original(), ensure_running)?,
        clone_text_with_cancellation(placeholder.semantic_identity(), ensure_running)?,
        placeholder.origin(),
        clone_text_with_cancellation(placeholder.label(), ensure_running)?,
        clone_text_with_cancellation(placeholder.scope(), ensure_running)?,
        placeholder.segment(),
        placeholder.order_policy(),
        placeholder.wrapper(),
    ))
}

fn append_text_with_cancellation<E>(
    output: &mut String,
    source: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    const CHUNK_BYTES: usize = 64 * 1024;

    ensure_running()?;
    let mut start = 0_usize;
    while start < source.len() {
        ensure_running()?;
        let mut end = start.saturating_add(CHUNK_BYTES).min(source.len());
        while end < source.len() && !source.is_char_boundary(end) {
            end += 1;
        }
        output.push_str(&source[start..end]);
        start = end;
    }
    ensure_running()
}

fn text_eq_with_cancellation<E>(
    left: &str,
    right: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<bool, E> {
    const CHUNK_BYTES: usize = 64 * 1024;

    ensure_running()?;
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left
        .as_bytes()
        .chunks(CHUNK_BYTES)
        .zip(right.as_bytes().chunks(CHUNK_BYTES))
    {
        ensure_running()?;
        if left != right {
            return Ok(false);
        }
    }
    ensure_running()?;
    Ok(true)
}

fn text_contains_with_cancellation<E>(
    haystack: &str,
    needle: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<bool, E> {
    Ok(find_text_with_cancellation(haystack, needle, ensure_running)?.is_some())
}

fn find_text_with_cancellation<E>(
    haystack: &str,
    needle: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Option<usize>, E> {
    const CHUNK_BYTES: usize = 64 * 1024;

    ensure_running()?;
    if needle.is_empty() {
        return Ok(Some(0));
    }
    if needle.len() > haystack.len() {
        return Ok(None);
    }
    let overlap = needle.len().saturating_sub(1);
    let mut start = 0_usize;
    while start < haystack.len() {
        ensure_running()?;
        let owned_end = start.saturating_add(CHUNK_BYTES).min(haystack.len());
        let search_end = owned_end.saturating_add(overlap).min(haystack.len());
        let mut search_start = start;
        while search_start > 0 && !haystack.is_char_boundary(search_start) {
            search_start -= 1;
        }
        let mut char_search_end = search_end;
        while char_search_end < haystack.len() && !haystack.is_char_boundary(char_search_end) {
            char_search_end += 1;
        }
        if let Some(offset) = haystack[search_start..char_search_end].find(needle) {
            let found = search_start + offset;
            if found < owned_end {
                return Ok(Some(found));
            }
        }
        start = owned_end;
    }
    ensure_running()?;
    Ok(None)
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
        assert!(hidden.term_indices().is_empty());

        let visible = semantics
            .prepare(TextGroupKind::PluginParameter, r"<code:x>勇者")
            .expect("自然正文应可准备");
        assert_eq!(visible.term_indices(), [0]);
    }

    #[test]
    fn overlapping_terminology_keeps_only_the_longest_trigger() {
        let semantics = semantics_with(
            RpgMakerEngine::Mz,
            vec![
                TerminologyEntry::new("プフクス", "普芙库丝", vec!["プフクス".to_owned()]),
                TerminologyEntry::new("プフクスッ", "噗呼咯", vec!["プフクスッ".to_owned()]),
            ],
            Vec::new(),
        );
        let prepared = semantics
            .prepare(TextGroupKind::DatabaseEntry, "プフクスッは笑った")
            .expect("重叠术语原文应可准备");
        assert_eq!(prepared.term_indices(), [1]);
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
            let ResolvedTranslationSemanticError::ProtectPlaceholder(
                PlaceholderProtectionError::CrossesLineBoundary {
                    matched,
                    source_line_index,
                },
            ) = error
            else {
                panic!("应保留跨 Lines 槽边界的 Placeholder 匹配定位")
            };
            assert_eq!(matched.rule().rule_number(), Some(1));
            assert_eq!(source_line_index, 0);
            assert!(matched.start_byte() < matched.end_byte());
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
                .expect("模型 token 已唯一标出 wrapper 边界"),
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
    fn custom_shell_candidate_binds_repeated_fragments_in_natural_order() {
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

        assert_eq!(
            prepared
                .accept("<x>第一项</x><x>第二项</x>")
                .expect("完整自然顺序应确定每个 wrapper binding"),
            PreparedTranslationAcceptance::Accepted("<x>第一项</x><x>第二项</x>".to_owned())
        );
    }

    #[test]
    fn candidate_source_binding_survives_a_translated_lookbehind_label() {
        let semantics = semantics_with(
            RpgMakerEngine::Mz,
            Vec::new(),
            vec![PlaceholderRuleDefinition::new(
                Some(vec!["database_entry".to_owned()]),
                r"(?<=Name: )[A-Za-z0-9-]+",
            )],
        );
        let prepared = semantics
            .prepare(TextGroupKind::DatabaseEntry, "Name: abc-123 翻訳")
            .expect("源文凭据应由 lookbehind 规则保护");

        assert_eq!(
            prepared
                .accept("名称：abc-123 译文")
                .expect("译文标签变化后仍应按源文 binding 验收"),
            PreparedTranslationAcceptance::Accepted("名称：abc-123 译文".to_owned())
        );
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
    fn candidate_binds_repeated_original_placeholders_in_natural_order() {
        let prepared = ResolvedTranslationSemantics::for_test()
            .prepare(TextGroupKind::EventDialogue, r"\C[2]翻訳\C[2]")
            .expect("重复控制符原文应可准备");

        assert_eq!(
            prepared
                .accept(r"\C[2]译文\C[2]")
                .expect("自然顺序应确定重复控制符 binding"),
            PreparedTranslationAcceptance::Accepted(r"\C[2]译文\C[2]".to_owned())
        );
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
