use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use crate::diagnostic::{
    DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticImpact, DiagnosticReason,
    DiagnosticStage, DiagnosticSubject, SafeDiagnostic,
};
use crate::execution::OperationCompletion;
use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::rpg_maker::RpgMakerEngine;
use crate::rpg_maker::lua::runtime::{
    OwnedLuaProgram, TrustedLuaHostCallError, TrustedLuaPreparedTranslation,
    TrustedLuaPreparedTranslationAcceptance, TrustedLuaPreparedTranslationStatus,
    TrustedLuaTranslationSemantics, TrustedLuaTranslationTerm,
};
use crate::rpg_maker::lua::{
    LuaInvocation, LuaProjectContext, TrustedLuaExecutionHost, TrustedLuaExecutionOutcome,
};
use crate::rpg_maker::project::OpenedProject;
use crate::rpg_maker::text::TextGroupKind;

use super::semantics::{
    PreparedTranslationAcceptance, PreparedTranslationRejection, PreparedTranslationStatus,
    PreparedTranslationText, ResolvedTranslationSemanticError, ResolvedTranslationSemantics,
};
use super::standard::TranslationUnitRejectionReason;

/// 使用可信 Lua 程序翻译其自有数据的职责契约。
///
/// Lua 翻译完整拥有自己的数据协议、事务划分、重试和幂等语义。标准翻译和顶层
/// 翻译用例不解释 Lua 产物，也不回滚 Lua 或前序标准翻译已经提交的副作用。
pub(crate) trait LuaTranslation: Send + Sync {
    /// 配置边界已经选择且只供 Lua LLM 调用使用的 Client。
    type Client: Send + Sync + 'static;
    /// Lua 翻译失败。
    type Error: Error + Send + Sync + 'static;

    /// 使用本次执行配置运行调用方明确指定的可信 Lua 程序。
    fn run(
        &self,
        project: &OpenedProject,
        llm_client: Arc<Self::Client>,
        semantics: Arc<dyn TrustedLuaTranslationSemantics>,
        program: OwnedLuaProgram,
    ) -> impl Future<Output = Result<OperationCompletion<()>, Self::Error>> + Send;
}

/// 把 Translate 阶段已经建立的项目事实、公共 Client 和解析语义交给可信 Lua Host。
pub(crate) struct LuaTranslationService<H> {
    host: H,
}

impl<H> LuaTranslationService<H> {
    pub(crate) fn new(host: H) -> Self {
        Self { host }
    }
}

impl<H> LuaTranslation for LuaTranslationService<H>
where
    H: TrustedLuaExecutionHost,
{
    type Client = H::TranslationClient;
    type Error = LuaTranslationError<H::Error>;

    async fn run(
        &self,
        project: &OpenedProject,
        llm_client: Arc<Self::Client>,
        semantics: Arc<dyn TrustedLuaTranslationSemantics>,
        program: OwnedLuaProgram,
    ) -> Result<OperationCompletion<()>, Self::Error> {
        let error_path = program.main_script_path().to_path_buf();
        let invocation = LuaInvocation::translate(
            program,
            LuaProjectContext::for_frozen_source(
                project.name().as_str(),
                project.layout().rpg_maker_layout().engine(),
                project.source_content_root(),
                project.database_path().to_path_buf(),
                project.language_pair().clone(),
            ),
            llm_client,
            semantics,
        );

        let completion = self.host.execute(invocation).await.map_err(|source| {
            LuaTranslationError::ExecuteHost {
                script_path: error_path,
                source,
            }
        })?;
        let OperationCompletion::Completed(outcome) = completion else {
            return Ok(OperationCompletion::Cancelled);
        };
        match outcome {
            TrustedLuaExecutionOutcome::Empty => Ok(OperationCompletion::Completed(())),
            TrustedLuaExecutionOutcome::ExtractIntent(_) => {
                Err(LuaTranslationError::UnexpectedManagedOutcome)
            }
        }
    }
}

/// Lua Translate 阶段的 Host 执行失败。
#[derive(Debug)]
pub(crate) enum LuaTranslationError<E> {
    ExecuteHost { script_path: PathBuf, source: E },
    UnexpectedManagedOutcome,
}

impl<E> fmt::Display for LuaTranslationError<E>
where
    E: Error,
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExecuteHost {
                script_path,
                source,
            } => write!(
                formatter,
                "执行可信 Lua 翻译 Host 失败 {}：{source}",
                script_path.display()
            ),
            Self::UnexpectedManagedOutcome => {
                formatter.write_str("Lua Translate Host 返回了仅 Extract 可以产生的托管意图")
            }
        }
    }
}

impl<E> Error for LuaTranslationError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::ExecuteHost { source, .. } => Some(source),
            Self::UnexpectedManagedOutcome => None,
        }
    }
}

impl TrustedLuaTranslationSemantics for ResolvedTranslationSemantics {
    fn system_prompt(&self) -> &str {
        self.system_prompt()
    }

    fn source_language(&self) -> &str {
        self.language_pair().source().as_str()
    }

    fn target_language(&self) -> &str {
        self.language_pair().target().as_str()
    }

    fn prepare_translation(
        &self,
        kind: TextGroupKind,
        original: String,
        semantic_context: String,
    ) -> Result<Arc<dyn TrustedLuaPreparedTranslation>, TrustedLuaHostCallError> {
        let prepared = self
            .prepare(kind, &original)
            .map_err(|source| translation_semantic_error("prepare", source))?;
        let state_context = lua_translation_state_context(
            self.global_fingerprint(),
            self.engine(),
            kind,
            &original,
            &semantic_context,
            &prepared,
        );
        let terms = prepared
            .terms()
            .iter()
            .map(|term| TrustedLuaTranslationTerm::new(term.term(), term.translation()))
            .collect();
        Ok(Arc::new(ResolvedPreparedTranslation {
            prepared,
            terms,
            state_context,
        }))
    }
}

struct ResolvedPreparedTranslation {
    prepared: PreparedTranslationText,
    terms: Vec<TrustedLuaTranslationTerm>,
    state_context: LuaTranslationStateContext,
}

impl TrustedLuaPreparedTranslation for ResolvedPreparedTranslation {
    fn status(&self) -> TrustedLuaPreparedTranslationStatus {
        match self.prepared.status() {
            PreparedTranslationStatus::Active => TrustedLuaPreparedTranslationStatus::Active,
            PreparedTranslationStatus::NonSourceLanguage => {
                TrustedLuaPreparedTranslationStatus::NonSourceLanguage
            }
            PreparedTranslationStatus::FullyProtected => {
                TrustedLuaPreparedTranslationStatus::FullyProtected
            }
        }
    }

    fn model_text(&self) -> &str {
        self.prepared.model_text()
    }

    fn terms(&self) -> &[TrustedLuaTranslationTerm] {
        &self.terms
    }

    fn is_current(
        &self,
        translation: String,
        state: Sha256Fingerprint,
    ) -> Result<bool, TrustedLuaHostCallError> {
        Ok(self.state_context.finish(&translation) == state)
    }

    fn accept(
        &self,
        candidate: String,
    ) -> Result<TrustedLuaPreparedTranslationAcceptance, TrustedLuaHostCallError> {
        if candidate.contains('\r') {
            return Ok(TrustedLuaPreparedTranslationAcceptance::rejected(
                "contains_carriage_return",
            ));
        }
        if candidate.contains('\0') {
            return Ok(TrustedLuaPreparedTranslationAcceptance::rejected(
                "contains_nul",
            ));
        }
        if candidate.chars().all(char::is_whitespace) {
            return Ok(TrustedLuaPreparedTranslationAcceptance::rejected(
                "blank_translation",
            ));
        }
        match self
            .prepared
            .accept(candidate)
            .map_err(|source| translation_semantic_error("accept", source))?
        {
            PreparedTranslationAcceptance::Accepted(translation) => {
                let state = self.state_context.finish(&translation);
                Ok(TrustedLuaPreparedTranslationAcceptance::accepted(
                    translation,
                    state,
                ))
            }
            PreparedTranslationAcceptance::Rejected(reason) => Ok(
                TrustedLuaPreparedTranslationAcceptance::rejected(rejection_code(&reason)?),
            ),
        }
    }
}

#[derive(Clone, Copy)]
struct LuaTranslationStateContext(Sha256Fingerprint);

impl LuaTranslationStateContext {
    fn finish(self, translation: &str) -> Sha256Fingerprint {
        let mut hasher = Sha256FramedHasher::new(b"att.rpg_maker.lua-translation-state");
        hasher
            .frame(1, self.0.as_bytes())
            .frame(2, translation.as_bytes());
        hasher.finish()
    }
}

fn lua_translation_state_context(
    global_semantics: Sha256Fingerprint,
    engine: RpgMakerEngine,
    kind: TextGroupKind,
    original: &str,
    semantic_context: &str,
    prepared: &PreparedTranslationText,
) -> LuaTranslationStateContext {
    let mut hasher = Sha256FramedHasher::new(b"att.rpg_maker.lua-translation-context");
    hasher
        .frame(1, global_semantics.as_bytes())
        .frame(2, engine.storage_name().as_bytes())
        .frame(3, group_kind_name(kind))
        .frame(4, original.as_bytes())
        .frame(5, semantic_context.as_bytes())
        .frame(6, prepared.status().storage_name().as_bytes())
        .frame(7, prepared.model_text().as_bytes());
    for placeholder in prepared.placeholders() {
        let origin = match placeholder.origin() {
            super::standard::PlaceholderRuleOrigin::BuiltIn => b"builtin".as_slice(),
            super::standard::PlaceholderRuleOrigin::Custom => b"custom".as_slice(),
        };
        let segment = match placeholder.segment() {
            super::standard::PlaceholderSegment::Whole => b"whole".as_slice(),
            super::standard::PlaceholderSegment::Begin => b"begin".as_slice(),
            super::standard::PlaceholderSegment::End => b"end".as_slice(),
        };
        hasher
            .frame(20, placeholder.token().as_bytes())
            .frame(21, placeholder.original().as_bytes())
            .frame(22, origin)
            .frame(23, placeholder.label().as_bytes())
            .frame(24, placeholder.scope().as_bytes())
            .frame(25, segment);
    }
    for term in prepared.terms() {
        hasher
            .frame(30, term.term().as_bytes())
            .frame(31, term.translation().as_bytes());
    }
    LuaTranslationStateContext(hasher.finish())
}

const fn group_kind_name(kind: TextGroupKind) -> &'static [u8] {
    match kind {
        TextGroupKind::DatabaseEntry => b"database_entry",
        TextGroupKind::System => b"system",
        TextGroupKind::Map => b"map",
        TextGroupKind::EventDialogue => b"dialogue",
        TextGroupKind::EventChoices => b"choices",
        TextGroupKind::EventScrollingText => b"scrolling_text",
        TextGroupKind::EventCommand => b"event_command",
        TextGroupKind::PluginParameter => b"plugin_parameter",
    }
}

fn rejection_code(
    reason: &PreparedTranslationRejection,
) -> Result<&'static str, TrustedLuaHostCallError> {
    match reason {
        PreparedTranslationRejection::NotActive(status) => Ok(status.storage_name()),
        PreparedTranslationRejection::Candidate(reason) => match reason {
            TranslationUnitRejectionReason::BlankTranslation => Ok("blank_translation"),
            TranslationUnitRejectionReason::NoNaturalLanguageText => Ok("no_natural_language_text"),
            TranslationUnitRejectionReason::ContainsByteOrderMark => Ok("contains_byte_order_mark"),
            TranslationUnitRejectionReason::PlaceholderMismatch { .. } => {
                Ok("placeholder_mismatch")
            }
            TranslationUnitRejectionReason::UnexpectedPlaceholderToken { .. } => {
                Ok("unexpected_placeholder_token")
            }
            TranslationUnitRejectionReason::PlaceholderNormalizationAmbiguous { .. } => {
                Ok("placeholder_normalization_ambiguous")
            }
            TranslationUnitRejectionReason::SourceResidual { .. } => Ok("source_residual"),
            TranslationUnitRejectionReason::Missing
            | TranslationUnitRejectionReason::Duplicate
            | TranslationUnitRejectionReason::InvalidShape { .. }
            | TranslationUnitRejectionReason::LineCountMismatch { .. }
            | TranslationUnitRejectionReason::InvalidLineText { .. }
            | TranslationUnitRejectionReason::BlankLineMismatch { .. } => {
                Err(TrustedLuaHostCallError::new(
                    "translation",
                    "internal_invariant",
                    "Lua 标量验收产生了 Standard 响应结构专用拒绝原因",
                    None,
                    None,
                ))
            }
        },
    }
}

fn translation_semantic_error(
    kind: &'static str,
    source: ResolvedTranslationSemanticError,
) -> TrustedLuaHostCallError {
    let operation = match kind {
        "prepare" => "translation.prepare",
        "accept" => "translation.accept",
        _ => "translation.semantic",
    };
    let detail = source.safe_detail();
    let diagnostic = SafeDiagnostic::new(
        DiagnosticCode::LuaExecution,
        DiagnosticStage::Translate,
        DiagnosticSubject::operation(operation),
        DiagnosticReason::failure_with_detail(DiagnosticFailureKind::LuaHostCallFailed, detail),
        DiagnosticImpact::Unchanged,
        DiagnosticAction::CheckProjectState,
    );
    TrustedLuaHostCallError::new(
        "translation",
        kind,
        "Lua 翻译语义处理失败",
        None,
        Some(Arc::new(source)),
    )
    .with_operation(operation)
    .with_safe_diagnostic(diagnostic)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::rpg_maker::ProjectName;
    use crate::rpg_maker::lua::LuaPhase;
    use crate::rpg_maker::translate::executor::TranslationCandidateTechnicalError;
    use crate::rpg_maker::translate::language_projection::LanguageTextProjectionError;
    use crate::rpg_maker::translate::placeholder::PlaceholderProtectionError;

    #[derive(Debug)]
    struct FakeClient {
        name: &'static str,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct RecordedInvocation {
        phase: LuaPhase,
        script_path: PathBuf,
        project: LuaProjectContext,
        client_address: usize,
        client_name: &'static str,
        semantics_address: usize,
    }

    #[derive(Clone)]
    struct FakeHost {
        invocation: Arc<Mutex<Option<RecordedInvocation>>>,
        fail: bool,
        cancelled: bool,
        unexpected_outcome: bool,
    }

    impl TrustedLuaExecutionHost for FakeHost {
        type TranslationClient = FakeClient;
        type Error = FakeError;

        async fn execute(
            &self,
            invocation: LuaInvocation<Self::TranslationClient>,
        ) -> Result<OperationCompletion<TrustedLuaExecutionOutcome>, Self::Error> {
            let recorded = match invocation {
                LuaInvocation::Translate {
                    program,
                    project,
                    llm_client,
                    semantics,
                } => RecordedInvocation {
                    phase: LuaPhase::Translate,
                    script_path: program.main_script_path().to_path_buf(),
                    project,
                    client_address: Arc::as_ptr(&llm_client).addr(),
                    client_name: llm_client.name,
                    semantics_address: Arc::as_ptr(&semantics) as *const () as usize,
                },
                LuaInvocation::Extract { .. } => panic!("翻译服务不应提交 Extract 调用"),
                LuaInvocation::WriteBack { .. } => {
                    panic!("翻译服务不应提交 WriteBack 调用")
                }
                LuaInvocation::Project { .. } => {
                    panic!("翻译服务不应提交独立项目 Lua 调用")
                }
            };
            *self.invocation.lock().expect("调用记录锁不应中毒") = Some(recorded);

            if self.fail {
                Err(FakeError)
            } else if self.cancelled {
                Ok(OperationCompletion::Cancelled)
            } else if self.unexpected_outcome {
                Ok(OperationCompletion::Completed(
                    TrustedLuaExecutionOutcome::ExtractIntent(
                        crate::rpg_maker::lua::runtime::TrustedLuaExtractIntent::Deactivate,
                    ),
                ))
            } else {
                Ok(OperationCompletion::Completed(
                    TrustedLuaExecutionOutcome::Empty,
                ))
            }
        }
    }

    struct FakeSemantics;

    impl TrustedLuaTranslationSemantics for FakeSemantics {
        fn system_prompt(&self) -> &str {
            "system"
        }

        fn source_language(&self) -> &str {
            "ja"
        }

        fn target_language(&self) -> &str {
            "zh-Hans"
        }

        fn prepare_translation(
            &self,
            _kind: TextGroupKind,
            _original: String,
            _semantic_context: String,
        ) -> Result<Arc<dyn TrustedLuaPreparedTranslation>, TrustedLuaHostCallError> {
            Err(TrustedLuaHostCallError::new(
                "test",
                "unused",
                "测试不应预处理文本",
                None,
                None,
            ))
        }
    }

    fn semantics() -> Arc<dyn TrustedLuaTranslationSemantics> {
        Arc::new(FakeSemantics)
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeError;

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("host failed")
        }
    }

    impl Error for FakeError {}

    fn project() -> OpenedProject {
        OpenedProject::new(
            "alice".parse::<ProjectName>().expect("项目名应合法"),
            PathBuf::from("C:/projects/alice"),
            PathBuf::from("C:/projects/alice/project.db"),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            crate::rpg_maker::project::test_layout_profile(),
        )
    }

    fn program(path: &str) -> OwnedLuaProgram {
        OwnedLuaProgram::new(PathBuf::from(path), b"return nil".to_vec())
    }

    #[test]
    fn candidate_rejection_codes_only_expose_scalar_protocol_reasons() {
        let cases = [
            (
                TranslationUnitRejectionReason::BlankTranslation,
                "blank_translation",
            ),
            (
                TranslationUnitRejectionReason::NoNaturalLanguageText,
                "no_natural_language_text",
            ),
            (
                TranslationUnitRejectionReason::ContainsByteOrderMark,
                "contains_byte_order_mark",
            ),
            (
                TranslationUnitRejectionReason::PlaceholderMismatch {
                    token: "TOKEN".to_owned(),
                },
                "placeholder_mismatch",
            ),
            (
                TranslationUnitRejectionReason::UnexpectedPlaceholderToken {
                    token: "TOKEN".to_owned(),
                },
                "unexpected_placeholder_token",
            ),
            (
                TranslationUnitRejectionReason::PlaceholderNormalizationAmbiguous {
                    original: "原文".to_owned(),
                },
                "placeholder_normalization_ambiguous",
            ),
            (
                TranslationUnitRejectionReason::SourceResidual {
                    fragment: "残留".to_owned(),
                },
                "source_residual",
            ),
        ];

        for (reason, expected) in cases {
            assert_eq!(
                rejection_code(&PreparedTranslationRejection::Candidate(reason))
                    .expect("标量拒绝原因应可公开"),
                expected,
            );
        }

        for reason in [
            TranslationUnitRejectionReason::Missing,
            TranslationUnitRejectionReason::Duplicate,
            TranslationUnitRejectionReason::InvalidShape {
                message: "测试".to_owned(),
            },
            TranslationUnitRejectionReason::LineCountMismatch {
                expected: 2,
                actual: 1,
            },
            TranslationUnitRejectionReason::InvalidLineText { line_index: 1 },
            TranslationUnitRejectionReason::BlankLineMismatch {
                line_index: 1,
                expected_blank: true,
            },
        ] {
            let error = rejection_code(&PreparedTranslationRejection::Candidate(reason))
                .expect_err("Standard 响应结构错误不属于 Lua 标量协议");
            assert_eq!(error.kind(), "internal_invariant");
        }
    }

    #[test]
    fn semantic_error_publishes_stable_operation_without_source_text() {
        const SOURCE_SENTINEL: &str = "TRANSLATION_SEMANTIC_SOURCE_SENTINEL";
        let error = translation_semantic_error(
            "prepare",
            ResolvedTranslationSemanticError::ProjectLanguageText(
                LanguageTextProjectionError::MissingToken {
                    token: SOURCE_SENTINEL.to_owned(),
                },
            ),
        );

        assert_eq!(error.message(), "Lua 翻译语义处理失败");
        assert_eq!(error.operation(), Some("translation.prepare"));
        let diagnostic = error.safe_diagnostic().expect("语义错误必须携带安全投影");
        let serialized = serde_json::to_string(diagnostic).expect("安全诊断应可序列化");
        assert!(serialized.contains("translation.prepare"));
        assert!(serialized.contains("semantic=language_projection"));
        assert!(serialized.contains("missing_required_placeholder_token"));
        assert!(!serialized.contains(SOURCE_SENTINEL));
        assert!(!error.to_string().contains(SOURCE_SENTINEL));

        for source in [
            PlaceholderProtectionError::EmptyMatch {
                label: SOURCE_SENTINEL.to_owned(),
            },
            PlaceholderProtectionError::OverlappingMatches {
                first: SOURCE_SENTINEL.to_owned(),
                second: SOURCE_SENTINEL.to_owned(),
            },
        ] {
            let error = translation_semantic_error(
                "prepare",
                ResolvedTranslationSemanticError::ProtectPlaceholder(source),
            );
            let diagnostic = error.safe_diagnostic().expect("语义错误必须携带安全投影");
            let serialized = serde_json::to_string(diagnostic).expect("安全诊断应可序列化");
            assert!(serialized.contains("semantic=placeholder_protection"));
            assert!(!serialized.contains(SOURCE_SENTINEL));
            assert!(!error.to_string().contains(SOURCE_SENTINEL));
        }
    }

    #[test]
    fn semantic_error_keeps_typed_rule_count_and_segment_facts() {
        let cases = [
            (
                "prepare",
                ResolvedTranslationSemanticError::ProtectPlaceholder(
                    PlaceholderProtectionError::MissingTextCapture { rule_number: 17 },
                ),
                ["semantic=placeholder_protection", "rule=17"].as_slice(),
            ),
            (
                "prepare",
                ResolvedTranslationSemanticError::ProjectLanguageText(
                    LanguageTextProjectionError::ChangedSegmentCount {
                        expected: 9,
                        actual: 7,
                    },
                ),
                [
                    "language_repair_changed_segment_count",
                    "expected=9",
                    "actual=7",
                ]
                .as_slice(),
            ),
            (
                "accept",
                ResolvedTranslationSemanticError::AcceptCandidate(
                    TranslationCandidateTechnicalError::LanguageProjection(
                        LanguageTextProjectionError::ChangedSegmentKind { segment_index: 4 },
                    ),
                ),
                ["translation.accept", "segment_index=4"].as_slice(),
            ),
        ];

        for (kind, source, expected_facts) in cases {
            let error = translation_semantic_error(kind, source);
            let diagnostic = error.safe_diagnostic().expect("语义错误必须携带安全投影");
            let serialized = serde_json::to_string(diagnostic).expect("安全诊断应可序列化");
            for fact in expected_facts {
                assert!(
                    serialized.contains(fact),
                    "缺少安全事实 {fact}: {serialized}"
                );
            }
        }
    }

    #[test]
    fn adapter_projects_the_shared_semantics_without_a_second_pipeline() {
        let semantics = ResolvedTranslationSemantics::for_test();

        let active = TrustedLuaTranslationSemantics::prepare_translation(
            &semantics,
            TextGroupKind::DatabaseEntry,
            r"\C[2]勇者".to_owned(),
            "speaker=Harold".to_owned(),
        )
        .expect("共享语义应完成占位符保护");
        assert_eq!(active.status(), TrustedLuaPreparedTranslationStatus::Active);
        assert_eq!(
            active.model_text(),
            "⟦ATT_RPG_MAKER_CONTROL_WHOLE_0000⟧勇者"
        );
        assert!(active.terms().is_empty());
        let acceptance = active
            .accept("英雄⟦ATT_RPG_MAKER_CONTROL_WHOLE_0000⟧".to_owned())
            .expect("共享验收应可执行");
        let TrustedLuaPreparedTranslationAcceptance::Accepted { translation, state } = acceptance
        else {
            panic!("合法标量候选应被接受")
        };
        assert_eq!(translation, r"英雄\C[2]");
        assert!(
            active
                .is_current(translation.clone(), state)
                .expect("Current 比较应可执行")
        );
        assert!(
            !active
                .is_current("不同译文".to_owned(), state)
                .expect("Current 比较应可执行")
        );

        let non_source = TrustedLuaTranslationSemantics::prepare_translation(
            &semantics,
            TextGroupKind::Map,
            "New Game".to_owned(),
            String::new(),
        )
        .expect("非源语言文本应是正常状态");
        assert_eq!(
            non_source.status(),
            TrustedLuaPreparedTranslationStatus::NonSourceLanguage
        );
        assert_eq!(
            non_source
                .accept("新游戏".to_owned())
                .expect("非 active 状态应返回普通拒绝"),
            TrustedLuaPreparedTranslationAcceptance::rejected("non_source_language")
        );

        let fully_protected = TrustedLuaTranslationSemantics::prepare_translation(
            &semantics,
            TextGroupKind::EventDialogue,
            r"\C[2]".to_owned(),
            String::new(),
        )
        .expect("全保护文本应是正常状态");
        assert_eq!(
            fully_protected.status(),
            TrustedLuaPreparedTranslationStatus::FullyProtected
        );
        assert_eq!(
            fully_protected
                .accept("⟦ATT_RPG_MAKER_CONTROL_WHOLE_0000⟧".to_owned())
                .expect("非 active 状态应返回普通拒绝"),
            TrustedLuaPreparedTranslationAcceptance::rejected("fully_protected")
        );
    }

    #[test]
    fn scalar_accept_allows_lf_and_rejects_cr_nul_and_blank_text() {
        let semantics = ResolvedTranslationSemantics::for_test();
        let prepared = TrustedLuaTranslationSemantics::prepare_translation(
            &semantics,
            TextGroupKind::Map,
            "勇者".to_owned(),
            String::new(),
        )
        .expect("测试文本应可准备");

        let accepted = prepared
            .accept("英雄\n再次出发".to_owned())
            .expect("LF 标量应可验收");
        assert!(matches!(
            accepted,
            TrustedLuaPreparedTranslationAcceptance::Accepted { ref translation, .. }
                if translation == "英雄\n再次出发"
        ));
        for (candidate, expected) in [
            ("英雄\r返回", "contains_carriage_return"),
            ("英雄\0返回", "contains_nul"),
            (" \n\t", "blank_translation"),
        ] {
            assert_eq!(
                prepared
                    .accept(candidate.to_owned())
                    .expect("内容拒绝应是普通结果"),
                TrustedLuaPreparedTranslationAcceptance::rejected(expected),
            );
        }
    }

    #[test]
    fn opaque_state_binds_script_context_and_final_translation_deterministically() {
        let semantics = ResolvedTranslationSemantics::for_test();
        let prepare = |context: &str| {
            TrustedLuaTranslationSemantics::prepare_translation(
                &semantics,
                TextGroupKind::Map,
                "勇者".to_owned(),
                context.to_owned(),
            )
            .expect("测试文本应可准备")
        };
        let accept = |prepared: &Arc<dyn TrustedLuaPreparedTranslation>| {
            let TrustedLuaPreparedTranslationAcceptance::Accepted { translation, state } = prepared
                .accept("英雄".to_owned())
                .expect("候选验收应可执行")
            else {
                panic!("合法候选应被接受")
            };
            (translation, state)
        };

        let first = prepare("speaker=Harold");
        let same = prepare("speaker=Harold");
        let changed = prepare("speaker=Therese");
        let (translation, first_state) = accept(&first);
        let (_, same_state) = accept(&same);
        let (_, changed_state) = accept(&changed);

        assert_eq!(first_state, same_state);
        assert_ne!(first_state, changed_state);
        assert!(
            first
                .is_current(translation, first_state)
                .expect("Current 比较应可执行")
        );
    }

    #[tokio::test]
    async fn passes_complete_translate_context_and_the_same_client_to_host_once() {
        let recorded = Arc::new(Mutex::new(None));
        let service = LuaTranslationService::new(FakeHost {
            invocation: Arc::clone(&recorded),
            fail: false,
            cancelled: false,
            unexpected_outcome: false,
        });
        let client = Arc::new(FakeClient { name: "quality" });
        let client_address = Arc::as_ptr(&client).addr();
        let semantics = semantics();
        let semantics_address = Arc::as_ptr(&semantics) as *const () as usize;

        service
            .run(
                &project(),
                Arc::clone(&client),
                semantics,
                program("scripts/translate.lua"),
            )
            .await
            .expect("Lua 翻译应该成功");

        let invocation = recorded
            .lock()
            .expect("调用记录锁不应中毒")
            .clone()
            .expect("Host 应该收到一次调用");
        assert_eq!(invocation.phase, LuaPhase::Translate);
        assert_eq!(
            invocation.script_path,
            PathBuf::from("scripts/translate.lua")
        );
        assert_eq!(invocation.client_address, client_address);
        assert_eq!(invocation.client_name, "quality");
        assert_eq!(invocation.semantics_address, semantics_address);
        assert_eq!(invocation.project.name(), "alice");
        assert_eq!(
            invocation.project.source_root(),
            Path::new("C:/projects/alice/source")
        );
        assert_eq!(
            invocation.project.database_path(),
            Path::new("C:/projects/alice/project.db")
        );
        assert_eq!(invocation.project.source_language().as_str(), "ja");
        assert_eq!(invocation.project.target_language().as_str(), "zh-Hans");
    }

    #[tokio::test]
    async fn preserves_script_path_and_host_source() {
        let service = LuaTranslationService::new(FakeHost {
            invocation: Arc::new(Mutex::new(None)),
            fail: true,
            cancelled: false,
            unexpected_outcome: false,
        });

        let error = service
            .run(
                &project(),
                Arc::new(FakeClient { name: "quality" }),
                semantics(),
                program("broken translation.lua"),
            )
            .await
            .expect_err("Host 失败应该传播");

        assert!(matches!(
            &error,
            LuaTranslationError::ExecuteHost {
                script_path,
                source: FakeError
            } if script_path == &PathBuf::from("broken translation.lua")
        ));
        assert_eq!(
            error.source().and_then(|source| source.downcast_ref()),
            Some(&FakeError)
        );
        assert!(error.to_string().contains("broken translation.lua"));
    }

    #[tokio::test]
    async fn rejects_extract_managed_intent_from_translate_host() {
        let service = LuaTranslationService::new(FakeHost {
            invocation: Arc::new(Mutex::new(None)),
            fail: false,
            cancelled: false,
            unexpected_outcome: true,
        });

        let error = service
            .run(
                &project(),
                Arc::new(FakeClient { name: "quality" }),
                semantics(),
                program("translation.lua"),
            )
            .await
            .expect_err("Translate 只能接受 Empty Host 结果");

        assert!(matches!(
            error,
            LuaTranslationError::UnexpectedManagedOutcome
        ));
    }

    #[tokio::test]
    async fn cancellation_is_propagated_as_a_normal_translation_result() {
        let service = LuaTranslationService::new(FakeHost {
            invocation: Arc::new(Mutex::new(None)),
            fail: false,
            cancelled: true,
            unexpected_outcome: false,
        });

        let completion = service
            .run(
                &project(),
                Arc::new(FakeClient { name: "quality" }),
                semantics(),
                program("translation.lua"),
            )
            .await
            .expect("Lua 取消应是正常结果");

        assert_eq!(completion, OperationCompletion::Cancelled);
    }

    #[test]
    fn execution_future_is_send() {
        fn assert_send<T: Send>(_: T) {}

        let service = LuaTranslationService::new(FakeHost {
            invocation: Arc::new(Mutex::new(None)),
            fail: false,
            cancelled: false,
            unexpected_outcome: false,
        });
        let project = project();
        let client = Arc::new(FakeClient { name: "quality" });
        assert_send(service.run(&project, client, semantics(), program("translate.lua")));
    }
}
