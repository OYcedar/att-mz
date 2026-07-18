use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use crate::language::{LanguageModule, LanguagePair};

/// 一个 MZ system prompt 及其唯一适用的规范语言对。
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct MzSystemPrompt {
    language_pair: LanguagePair,
    markdown: String,
}

impl fmt::Debug for MzSystemPrompt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MzSystemPrompt")
            .field("language_pair", &self.language_pair)
            .field("markdown", &"[REDACTED]")
            .finish()
    }
}

impl MzSystemPrompt {
    pub(crate) fn new(
        language_pair: LanguagePair,
        markdown: String,
    ) -> Result<Self, MzSystemPromptError> {
        if markdown.trim().is_empty() {
            return Err(MzSystemPromptError::Blank { language_pair });
        }
        Ok(Self {
            language_pair,
            markdown,
        })
    }

    pub(crate) fn language_pair(&self) -> &LanguagePair {
        &self.language_pair
    }

    pub(crate) fn markdown(&self) -> &str {
        &self.markdown
    }
}

/// Prompt 文件内容无法建立为受信 MZ system prompt。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MzSystemPromptError {
    Blank { language_pair: LanguagePair },
}

impl fmt::Display for MzSystemPromptError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Blank { language_pair } => write!(
                formatter,
                "语言对 {} -> {} 的 MZ system prompt 为空",
                language_pair.source(),
                language_pair.target()
            ),
        }
    }
}

impl Error for MzSystemPromptError {}

/// 项目打开后为其精确语言对一次性解析出的 MZ 翻译资源。
///
/// Planner 与 Executor 必须共享同一个实例；两者不得重新查询语言目录或选择 Prompt。
pub(crate) struct ResolvedMzTranslationResources {
    system_prompt: MzSystemPrompt,
    source_language: Arc<dyn LanguageModule>,
}

impl ResolvedMzTranslationResources {
    pub(crate) fn new(
        system_prompt: MzSystemPrompt,
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

    pub(crate) fn system_prompt(&self) -> &MzSystemPrompt {
        &self.system_prompt
    }

    pub(crate) fn source_language(&self) -> Arc<dyn LanguageModule> {
        Arc::clone(&self.source_language)
    }
}

impl fmt::Debug for ResolvedMzTranslationResources {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedMzTranslationResources")
            .field("language_pair", self.language_pair())
            .field("system_prompt", &"[REDACTED]")
            .field("source_language", &"dyn LanguageModule")
            .finish()
    }
}

/// MZ 规划阶段全部由外部明确提供的资源策略。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MzTranslationPlanningConfiguration {
    scope_concurrency: NonZeroUsize,
    max_message_characters: NonZeroUsize,
}

impl MzTranslationPlanningConfiguration {
    pub(crate) const fn new(
        scope_concurrency: NonZeroUsize,
        max_message_characters: NonZeroUsize,
    ) -> Self {
        Self {
            scope_concurrency,
            max_message_characters,
        }
    }

    pub(crate) const fn scope_concurrency(&self) -> NonZeroUsize {
        self.scope_concurrency
    }

    pub(crate) const fn max_message_characters(&self) -> NonZeroUsize {
        self.max_message_characters
    }
}

/// MZ 单任务模型请求阶段全部由外部明确提供的重试策略。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MzTranslationRequestConfiguration {
    network_retry_delays: Vec<Duration>,
    max_network_retry_after: Duration,
}

impl MzTranslationRequestConfiguration {
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

/// 一次 MZ 翻译运行共享的不可变执行 Profile。
///
/// Prompt 与语言模块属于项目语言对解析结果，不属于 Profile。Debug 输出不会访问或
/// 展示 LLM Client，避免其中的凭据进入诊断。
pub(crate) struct MzTranslationProfile<L> {
    id: String,
    max_in_flight_tasks: NonZeroUsize,
    planning: MzTranslationPlanningConfiguration,
    request: MzTranslationRequestConfiguration,
    llm_client: Arc<L>,
}

impl<L> MzTranslationProfile<L> {
    pub(crate) fn new(
        id: impl Into<String>,
        max_in_flight_tasks: NonZeroUsize,
        planning: MzTranslationPlanningConfiguration,
        request: MzTranslationRequestConfiguration,
        llm_client: Arc<L>,
    ) -> Self {
        Self {
            id: id.into(),
            max_in_flight_tasks,
            planning,
            request,
            llm_client,
        }
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) const fn max_in_flight_tasks(&self) -> NonZeroUsize {
        self.max_in_flight_tasks
    }

    pub(crate) fn planning(&self) -> &MzTranslationPlanningConfiguration {
        &self.planning
    }

    pub(crate) fn request(&self) -> &MzTranslationRequestConfiguration {
        &self.request
    }

    pub(crate) fn llm_client(&self) -> &L {
        self.llm_client.as_ref()
    }

    pub(crate) fn shared_llm_client(&self) -> Arc<L> {
        Arc::clone(&self.llm_client)
    }
}

impl<L> fmt::Debug for MzTranslationProfile<L> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MzTranslationProfile")
            .field("id", &self.id)
            .field("max_in_flight_tasks", &self.max_in_flight_tasks)
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

    fn profile(secret: &'static str) -> MzTranslationProfile<SensitiveClient> {
        MzTranslationProfile::new(
            "primary",
            non_zero(3),
            MzTranslationPlanningConfiguration::new(non_zero(4), non_zero(24_000)),
            MzTranslationRequestConfiguration::new(
                vec![Duration::from_millis(250), Duration::from_secs(2)],
                Duration::from_secs(30),
            ),
            Arc::new(SensitiveClient(secret)),
        )
    }

    #[test]
    fn prompt_binds_non_blank_markdown_to_one_exact_language_pair() {
        let prompt = MzSystemPrompt::new(language_pair(), "# 完整提示词".to_owned())
            .expect("非空提示词合法");
        assert_eq!(prompt.language_pair(), &language_pair());
        assert_eq!(prompt.markdown(), "# 完整提示词");

        assert_eq!(
            MzSystemPrompt::new(language_pair(), " \n".to_owned()).expect_err("空白提示词必须失败"),
            MzSystemPromptError::Blank {
                language_pair: language_pair(),
            }
        );
    }

    #[test]
    fn profile_keeps_every_external_strategy_without_owning_prompt() {
        let profile = profile("secret");
        assert_eq!(profile.id(), "primary");
        assert_eq!(profile.max_in_flight_tasks(), non_zero(3));
        assert_eq!(profile.planning().scope_concurrency(), non_zero(4));
        assert_eq!(
            profile.planning().max_message_characters(),
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
        let prompt = MzSystemPrompt::new(language_pair(), "system".to_owned()).unwrap();
        let resources = ResolvedMzTranslationResources::new(prompt, Arc::clone(&module));

        assert_eq!(resources.language_pair(), &language_pair());
        assert_eq!(resources.system_prompt().markdown(), "system");
        assert!(Arc::ptr_eq(&resources.source_language(), &module));
    }
}
