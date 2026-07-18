use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use crate::att_mz::lua::runtime::{
    TrustedLuaHostCallError, TrustedLuaPreparedTranslation,
    TrustedLuaPreparedTranslationAcceptance, TrustedLuaPreparedTranslationStatus,
    TrustedLuaTranslationSemantics, TrustedLuaTranslationTerm,
};
use crate::att_mz::lua::{
    LuaInvocation, LuaProjectContext, TrustedLuaExecutionHost, TrustedLuaExecutionOutcome,
};
use crate::att_mz::project::OpenedProject;
use crate::att_mz::text::TextGroupKind;

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
    /// 与配置解析器产物一致的执行配置。
    type Profile: Send + Sync + 'static;
    /// Lua 翻译失败。
    type Error: Error + Send + Sync + 'static;

    /// 使用本次执行配置运行调用方明确指定的可信 Lua 程序。
    fn run(
        &self,
        project: &OpenedProject,
        profile: &Self::Profile,
        semantics: Arc<dyn TrustedLuaTranslationSemantics>,
        script_path: PathBuf,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 把 Translate 阶段已经建立的项目事实和 Profile 交给可信 Lua Host。
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
    type Profile = Arc<H::TranslationProfile>;
    type Error = LuaTranslationError<H::Error>;

    async fn run(
        &self,
        project: &OpenedProject,
        profile: &Self::Profile,
        semantics: Arc<dyn TrustedLuaTranslationSemantics>,
        script_path: PathBuf,
    ) -> Result<(), Self::Error> {
        let error_path = script_path.clone();
        let invocation = LuaInvocation::translate(
            script_path,
            LuaProjectContext::from_opened_project(project),
            Arc::clone(profile),
            semantics,
        );

        let outcome = self.host.execute(invocation).await.map_err(|source| {
            LuaTranslationError::ExecuteHost {
                script_path: error_path,
                source,
            }
        })?;
        match outcome {
            TrustedLuaExecutionOutcome::Empty => Ok(()),
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
        self.language_pair().source_language()
    }

    fn target_language(&self) -> &str {
        self.language_pair().target_language()
    }

    fn prepare_translation(
        &self,
        kind: TextGroupKind,
        original: String,
    ) -> Result<Arc<dyn TrustedLuaPreparedTranslation>, TrustedLuaHostCallError> {
        let prepared = self
            .prepare(kind, &original)
            .map_err(|source| translation_semantic_error("prepare", source))?;
        let terms = prepared
            .terms()
            .iter()
            .map(|term| TrustedLuaTranslationTerm::new(term.term(), term.translation()))
            .collect();
        Ok(Arc::new(ResolvedPreparedTranslation { prepared, terms }))
    }
}

struct ResolvedPreparedTranslation {
    prepared: PreparedTranslationText,
    terms: Vec<TrustedLuaTranslationTerm>,
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

    fn accept(
        &self,
        candidate: String,
    ) -> Result<TrustedLuaPreparedTranslationAcceptance, TrustedLuaHostCallError> {
        match self
            .prepared
            .accept(candidate)
            .map_err(|source| translation_semantic_error("accept", source))?
        {
            PreparedTranslationAcceptance::Accepted(translation) => Ok(
                TrustedLuaPreparedTranslationAcceptance::accepted(translation),
            ),
            PreparedTranslationAcceptance::Rejected(reason) => Ok(
                TrustedLuaPreparedTranslationAcceptance::rejected(rejection_code(&reason)),
            ),
        }
    }
}

fn rejection_code(reason: &PreparedTranslationRejection) -> &'static str {
    match reason {
        PreparedTranslationRejection::NotActive(status) => status.storage_name(),
        PreparedTranslationRejection::Candidate(reason) => match reason {
            TranslationUnitRejectionReason::Missing => "missing",
            TranslationUnitRejectionReason::Duplicate => "duplicate",
            TranslationUnitRejectionReason::InvalidShape { .. } => "invalid_shape",
            TranslationUnitRejectionReason::BlankTranslation => "blank_translation",
            TranslationUnitRejectionReason::NoNaturalLanguageText => "no_natural_language_text",
            TranslationUnitRejectionReason::ContainsByteOrderMark => "contains_byte_order_mark",
            TranslationUnitRejectionReason::PlaceholderMismatch { .. } => "placeholder_mismatch",
            TranslationUnitRejectionReason::UnexpectedPlaceholderToken { .. } => {
                "unexpected_placeholder_token"
            }
            TranslationUnitRejectionReason::PlaceholderNormalizationAmbiguous { .. } => {
                "placeholder_normalization_ambiguous"
            }
            TranslationUnitRejectionReason::SourceResidual { .. } => "source_residual",
        },
    }
}

fn translation_semantic_error(
    kind: &'static str,
    source: ResolvedTranslationSemanticError,
) -> TrustedLuaHostCallError {
    let message = source.to_string();
    TrustedLuaHostCallError::new("translation", kind, message, None, Some(Arc::new(source)))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::att_mz::ProjectName;
    use crate::att_mz::lua::LuaPhase;

    #[derive(Debug)]
    struct FakeProfile {
        name: &'static str,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct RecordedInvocation {
        phase: LuaPhase,
        script_path: PathBuf,
        project: LuaProjectContext,
        profile_address: usize,
        profile_name: &'static str,
        semantics_address: usize,
    }

    #[derive(Clone)]
    struct FakeHost {
        invocation: Arc<Mutex<Option<RecordedInvocation>>>,
        fail: bool,
        unexpected_outcome: bool,
    }

    impl TrustedLuaExecutionHost for FakeHost {
        type TranslationProfile = FakeProfile;
        type Error = FakeError;

        async fn execute(
            &self,
            invocation: LuaInvocation<Self::TranslationProfile>,
        ) -> Result<TrustedLuaExecutionOutcome, Self::Error> {
            let recorded = match invocation {
                LuaInvocation::Translate {
                    script_path,
                    project,
                    profile,
                    semantics,
                } => RecordedInvocation {
                    phase: LuaPhase::Translate,
                    script_path,
                    project,
                    profile_address: Arc::as_ptr(&profile).addr(),
                    profile_name: profile.name,
                    semantics_address: Arc::as_ptr(&semantics) as *const () as usize,
                },
                LuaInvocation::Extract { .. } => panic!("翻译服务不应提交 Extract 调用"),
                LuaInvocation::WriteBack { .. } => {
                    panic!("翻译服务不应提交 WriteBack 调用")
                }
            };
            *self.invocation.lock().expect("调用记录锁不应中毒") = Some(recorded);

            if self.fail {
                Err(FakeError)
            } else if self.unexpected_outcome {
                Ok(TrustedLuaExecutionOutcome::ExtractIntent(
                    crate::att_mz::lua::runtime::TrustedLuaExtractIntent::Deactivate,
                ))
            } else {
                Ok(TrustedLuaExecutionOutcome::Empty)
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
            crate::att_mz::project::test_layout_profile(),
        )
    }

    #[test]
    fn adapter_projects_the_shared_semantics_without_a_second_pipeline() {
        let semantics = ResolvedTranslationSemantics::for_test();

        let active = TrustedLuaTranslationSemantics::prepare_translation(
            &semantics,
            TextGroupKind::DatabaseEntry,
            r"\C[2]勇者".to_owned(),
        )
        .expect("共享语义应完成占位符保护");
        assert_eq!(active.status(), TrustedLuaPreparedTranslationStatus::Active);
        assert_eq!(active.model_text(), "⟦ATT_RMMZ_CONTROL_WHOLE_0000⟧勇者");
        assert!(active.terms().is_empty());
        assert_eq!(
            active
                .accept("英雄⟦ATT_RMMZ_CONTROL_WHOLE_0000⟧".to_owned())
                .expect("共享验收应可执行"),
            TrustedLuaPreparedTranslationAcceptance::accepted(r"英雄\C[2]")
        );

        let non_source = TrustedLuaTranslationSemantics::prepare_translation(
            &semantics,
            TextGroupKind::Map,
            "New Game".to_owned(),
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
        )
        .expect("全保护文本应是正常状态");
        assert_eq!(
            fully_protected.status(),
            TrustedLuaPreparedTranslationStatus::FullyProtected
        );
        assert_eq!(
            fully_protected
                .accept("⟦ATT_RMMZ_CONTROL_WHOLE_0000⟧".to_owned())
                .expect("非 active 状态应返回普通拒绝"),
            TrustedLuaPreparedTranslationAcceptance::rejected("fully_protected")
        );
    }

    #[tokio::test]
    async fn passes_complete_translate_context_and_the_same_profile_to_host_once() {
        let recorded = Arc::new(Mutex::new(None));
        let service = LuaTranslationService::new(FakeHost {
            invocation: Arc::clone(&recorded),
            fail: false,
            unexpected_outcome: false,
        });
        let profile = Arc::new(FakeProfile { name: "quality" });
        let profile_address = Arc::as_ptr(&profile).addr();
        let semantics = semantics();
        let semantics_address = Arc::as_ptr(&semantics) as *const () as usize;

        service
            .run(
                &project(),
                &profile,
                semantics,
                PathBuf::from("scripts/translate.lua"),
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
        assert_eq!(invocation.profile_address, profile_address);
        assert_eq!(invocation.profile_name, "quality");
        assert_eq!(invocation.semantics_address, semantics_address);
        assert_eq!(invocation.project.name().as_str(), "alice");
        assert_eq!(
            invocation.project.source_root(),
            Path::new("C:/projects/alice/source")
        );
        assert_eq!(
            invocation.project.database_path(),
            Path::new("C:/projects/alice/project.db")
        );
        assert_eq!(invocation.project.source_language(), "ja");
        assert_eq!(invocation.project.target_language(), "zh-Hans");
    }

    #[tokio::test]
    async fn preserves_script_path_and_host_source() {
        let service = LuaTranslationService::new(FakeHost {
            invocation: Arc::new(Mutex::new(None)),
            fail: true,
            unexpected_outcome: false,
        });

        let error = service
            .run(
                &project(),
                &Arc::new(FakeProfile { name: "quality" }),
                semantics(),
                PathBuf::from("broken translation.lua"),
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
            unexpected_outcome: true,
        });

        let error = service
            .run(
                &project(),
                &Arc::new(FakeProfile { name: "quality" }),
                semantics(),
                PathBuf::from("translation.lua"),
            )
            .await
            .expect_err("Translate 只能接受 Empty Host 结果");

        assert!(matches!(
            error,
            LuaTranslationError::UnexpectedManagedOutcome
        ));
    }

    #[test]
    fn execution_future_is_send() {
        fn assert_send<T: Send>(_: T) {}

        let service = LuaTranslationService::new(FakeHost {
            invocation: Arc::new(Mutex::new(None)),
            fail: false,
            unexpected_outcome: false,
        });
        let project = project();
        let profile = Arc::new(FakeProfile { name: "quality" });
        assert_send(service.run(
            &project,
            &profile,
            semantics(),
            PathBuf::from("translate.lua"),
        ));
    }
}
