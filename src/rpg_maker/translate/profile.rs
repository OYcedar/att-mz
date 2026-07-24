use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use crate::language::{LanguageModule, LanguagePair};

/// RPG Maker 翻译响应必须遵循的受信外层协议。
///
/// 该模式与已装配的 system prompt 共同建立，Planner 与 Executor 必须通过同一份
/// 已解析资源消费它，避免提示词要求与响应解析发生偏离。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TranslationResponseEnvelope {
    JsonOnly,
    ThinkingThenJson,
}

/// 一个 RPG Maker system prompt 及其唯一适用的规范语言对。
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct RpgMakerSystemPrompt {
    language_pair: LanguagePair,
    markdown: String,
    response_envelope: TranslationResponseEnvelope,
}

impl fmt::Debug for RpgMakerSystemPrompt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RpgMakerSystemPrompt")
            .field("language_pair", &self.language_pair)
            .field("markdown", &"[REDACTED]")
            .field("response_envelope", &self.response_envelope)
            .finish()
    }
}

impl RpgMakerSystemPrompt {
    pub(crate) fn new(
        language_pair: LanguagePair,
        markdown: String,
        response_envelope: TranslationResponseEnvelope,
    ) -> Result<Self, RpgMakerSystemPromptError> {
        if markdown.trim().is_empty() {
            return Err(RpgMakerSystemPromptError::Blank);
        }
        Ok(Self {
            language_pair,
            markdown,
            response_envelope,
        })
    }

    pub(crate) fn language_pair(&self) -> &LanguagePair {
        &self.language_pair
    }

    pub(crate) fn markdown(&self) -> &str {
        &self.markdown
    }

    pub(crate) const fn response_envelope(&self) -> TranslationResponseEnvelope {
        self.response_envelope
    }
}

/// Prompt 文件内容无法建立为受信 RPG Maker system prompt。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RpgMakerSystemPromptError {
    Blank,
}

impl fmt::Display for RpgMakerSystemPromptError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blank => formatter.write_str("RPG Maker system prompt 为空"),
        }
    }
}

impl Error for RpgMakerSystemPromptError {}

/// 项目打开后为其精确语言对一次性解析出的 RPG Maker 翻译资源。
///
/// Planner 与 Executor 必须共享同一个实例；两者不得重新查询语言目录或选择 Prompt。
pub(crate) struct ResolvedRpgMakerTranslationResources {
    system_prompt: RpgMakerSystemPrompt,
    source_language: Arc<dyn LanguageModule>,
}

impl ResolvedRpgMakerTranslationResources {
    pub(crate) fn new(
        system_prompt: RpgMakerSystemPrompt,
        source_language: Arc<dyn LanguageModule>,
    ) -> Self {
        Self {
            system_prompt,
            source_language,
        }
    }

    pub(crate) fn language_pair(&self) -> &LanguagePair {
        self.system_prompt.language_pair()
    }

    pub(crate) fn system_prompt(&self) -> &RpgMakerSystemPrompt {
        &self.system_prompt
    }

    pub(crate) fn source_language(&self) -> Arc<dyn LanguageModule> {
        Arc::clone(&self.source_language)
    }
}

impl fmt::Debug for ResolvedRpgMakerTranslationResources {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedRpgMakerTranslationResources")
            .field("language_pair", self.language_pair())
            .field("system_prompt", &"[REDACTED]")
            .field("source_language", &"dyn LanguageModule")
            .finish()
    }
}

/// Profile 为 RPG Maker Planner 提供的单任务 user message 字符上限。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerTranslationPlanningConfiguration {
    max_user_message_characters: NonZeroUsize,
}

impl RpgMakerTranslationPlanningConfiguration {
    pub(crate) const fn new(max_user_message_characters: NonZeroUsize) -> Self {
        Self {
            max_user_message_characters,
        }
    }

    pub(crate) const fn max_user_message_characters(&self) -> NonZeroUsize {
        self.max_user_message_characters
    }
}

/// RPG Maker 单任务模型请求阶段全部由外部明确提供的重试策略。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerTranslationRequestConfiguration {
    network_retry_delays: Vec<Duration>,
    max_network_retry_after: Duration,
}

impl RpgMakerTranslationRequestConfiguration {
    pub(crate) fn new(
        network_retry_delays: Vec<Duration>,
        max_network_retry_after: Duration,
    ) -> Self {
        Self {
            network_retry_delays,
            max_network_retry_after,
        }
    }

    pub(crate) fn network_retry_delays(&self) -> &[Duration] {
        &self.network_retry_delays
    }

    pub(crate) const fn max_network_retry_after(&self) -> Duration {
        self.max_network_retry_after
    }
}

/// 一次 RPG Maker 翻译运行共享的不可变执行 Profile。
///
/// Prompt 与语言模块属于项目语言对解析结果，不属于 Profile。Debug 输出不会访问或
/// 展示 LLM Client，避免其中的凭据进入诊断。
pub(crate) struct RpgMakerTranslationProfile<L> {
    id: String,
    planning: RpgMakerTranslationPlanningConfiguration,
    request: RpgMakerTranslationRequestConfiguration,
    llm_client: Arc<L>,
}

impl<L> RpgMakerTranslationProfile<L> {
    pub(crate) fn new(
        id: impl Into<String>,
        planning: RpgMakerTranslationPlanningConfiguration,
        request: RpgMakerTranslationRequestConfiguration,
        llm_client: Arc<L>,
    ) -> Self {
        Self {
            id: id.into(),
            planning,
            request,
            llm_client,
        }
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn planning(&self) -> &RpgMakerTranslationPlanningConfiguration {
        &self.planning
    }

    pub(crate) fn request(&self) -> &RpgMakerTranslationRequestConfiguration {
        &self.request
    }

    pub(crate) fn llm_client(&self) -> &L {
        self.llm_client.as_ref()
    }

    pub(crate) fn shared_llm_client(&self) -> Arc<L> {
        Arc::clone(&self.llm_client)
    }
}

impl<L> fmt::Debug for RpgMakerTranslationProfile<L> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RpgMakerTranslationProfile")
            .field("id", &self.id)
            .field("planning", &self.planning)
            .field("request", &self.request)
            .field("llm_client", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use crate::language::{JapaneseLanguageModule, JapaneseResidualPolicy, LanguageId};

    use super::*;

    #[derive(Debug, Eq, PartialEq)]
    struct SensitiveClient(&'static str);

    fn non_zero(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).expect("测试配置必须非零")
    }

    fn language_pair() -> LanguagePair {
        LanguagePair::new(
            LanguageId::parse("ja").expect("测试源语言合法"),
            LanguageId::parse("zh-Hans").expect("测试目标语言合法"),
        )
    }

    fn profile(secret: &'static str) -> RpgMakerTranslationProfile<SensitiveClient> {
        RpgMakerTranslationProfile::new(
            "primary",
            RpgMakerTranslationPlanningConfiguration::new(non_zero(24_000)),
            RpgMakerTranslationRequestConfiguration::new(
                vec![Duration::from_millis(250), Duration::from_secs(2)],
                Duration::from_secs(30),
            ),
            Arc::new(SensitiveClient(secret)),
        )
    }

    #[test]
    fn prompt_binds_non_blank_markdown_to_one_exact_language_pair() {
        let prompt = RpgMakerSystemPrompt::new(
            language_pair(),
            "# 完整提示词".to_owned(),
            TranslationResponseEnvelope::ThinkingThenJson,
        )
        .expect("非空提示词合法");
        assert_eq!(prompt.language_pair(), &language_pair());
        assert_eq!(prompt.markdown(), "# 完整提示词");
        assert_eq!(
            prompt.response_envelope(),
            TranslationResponseEnvelope::ThinkingThenJson
        );
        let debug = format!("{prompt:?}");
        assert!(debug.contains("ThinkingThenJson"));
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("# 完整提示词"));

        let error = RpgMakerSystemPrompt::new(
            language_pair(),
            " \n".to_owned(),
            TranslationResponseEnvelope::JsonOnly,
        )
        .expect_err("空白提示词必须失败");
        assert_eq!(error, RpgMakerSystemPromptError::Blank);
        assert_eq!(error.to_string(), "RPG Maker system prompt 为空");
    }

    #[test]
    fn profile_keeps_every_external_strategy_without_owning_prompt() {
        let profile = profile("secret");
        assert_eq!(profile.id(), "primary");
        assert_eq!(
            profile.planning().max_user_message_characters(),
            non_zero(24_000)
        );
        assert_eq!(
            profile.request().network_retry_delays(),
            [Duration::from_millis(250), Duration::from_secs(2)]
        );
        assert_eq!(
            profile.request().max_network_retry_after(),
            Duration::from_secs(30)
        );
        assert!(profile.llm_client() == &SensitiveClient("secret"));
    }

    #[test]
    fn debug_output_redacts_llm_client() {
        let debug = format!("{:?}", profile("never-print-this-secret"));
        assert!(debug.contains("primary"));
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("never-print-this-secret"));
    }

    #[test]
    fn resolved_resources_share_the_exact_prompt_and_language_module() {
        let module: Arc<dyn LanguageModule> = Arc::new(JapaneseLanguageModule::new(
            JapaneseResidualPolicy::new(non_zero(1), Vec::new()).expect("测试日文策略合法"),
            None,
        ));
        let prompt = RpgMakerSystemPrompt::new(
            language_pair(),
            "system".to_owned(),
            TranslationResponseEnvelope::JsonOnly,
        )
        .unwrap();
        let resources = ResolvedRpgMakerTranslationResources::new(prompt, Arc::clone(&module));

        assert_eq!(resources.language_pair(), &language_pair());
        assert_eq!(resources.system_prompt().markdown(), "system");
        assert_eq!(
            resources.system_prompt().response_envelope(),
            TranslationResponseEnvelope::JsonOnly
        );
        assert!(Arc::ptr_eq(&resources.source_language(), &module));
    }
}
