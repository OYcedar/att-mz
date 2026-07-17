use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

/// 用于精确选择系统提示词和语言实现的受信语言对。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct TranslationProfileLanguagePair {
    source_language: String,
    target_language: String,
}

impl TranslationProfileLanguagePair {
    pub(crate) fn new(
        source_language: impl Into<String>,
        target_language: impl Into<String>,
    ) -> Result<Self, TranslationProfileConfigurationError> {
        let source_language = source_language.into();
        let target_language = target_language.into();
        validate_language_id("源语言", &source_language)?;
        validate_language_id("目标语言", &target_language)?;
        Ok(Self {
            source_language,
            target_language,
        })
    }

    pub(crate) fn source_language(&self) -> &str {
        &self.source_language
    }

    pub(crate) fn target_language(&self) -> &str {
        &self.target_language
    }
}

/// 标准翻译规划阶段全部由外部明确提供的配置。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MzTranslationPlanningConfiguration {
    scope_concurrency: NonZeroUsize,
    max_message_characters: NonZeroUsize,
    system_markdown_by_language_pair: BTreeMap<TranslationProfileLanguagePair, String>,
}

impl MzTranslationPlanningConfiguration {
    pub(crate) fn new(
        scope_concurrency: NonZeroUsize,
        max_message_characters: NonZeroUsize,
        system_markdown_by_language_pair: impl IntoIterator<
            Item = (TranslationProfileLanguagePair, String),
        >,
    ) -> Result<Self, TranslationProfileConfigurationError> {
        let mut systems = BTreeMap::new();
        for (language_pair, system_markdown) in system_markdown_by_language_pair {
            if system_markdown.trim().is_empty() {
                return Err(TranslationProfileConfigurationError::BlankSystemMarkdown {
                    source_language: language_pair.source_language().to_owned(),
                    target_language: language_pair.target_language().to_owned(),
                });
            }
            if systems
                .insert(language_pair.clone(), system_markdown)
                .is_some()
            {
                return Err(
                    TranslationProfileConfigurationError::DuplicateLanguagePair {
                        source_language: language_pair.source_language().to_owned(),
                        target_language: language_pair.target_language().to_owned(),
                    },
                );
            }
        }
        if systems.is_empty() {
            return Err(TranslationProfileConfigurationError::MissingSystemMarkdown);
        }

        Ok(Self {
            scope_concurrency,
            max_message_characters,
            system_markdown_by_language_pair: systems,
        })
    }

    pub(crate) const fn scope_concurrency(&self) -> NonZeroUsize {
        self.scope_concurrency
    }

    pub(crate) const fn max_message_characters(&self) -> NonZeroUsize {
        self.max_message_characters
    }

    pub(crate) fn system_markdown(
        &self,
        language_pair: &TranslationProfileLanguagePair,
    ) -> Option<&str> {
        self.system_markdown_by_language_pair
            .get(language_pair)
            .map(String::as_str)
    }
}

/// 单任务模型执行阶段全部由外部明确提供的配置。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MzTranslationExecutionConfiguration {
    network_retry_delays: Vec<Duration>,
    max_network_retry_after: Duration,
}

impl MzTranslationExecutionConfiguration {
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

/// MZ 标准翻译与可信 Lua 翻译共享的完整受信载荷。
pub(crate) struct MzTranslationExecutionPayload<L> {
    planning: MzTranslationPlanningConfiguration,
    execution: MzTranslationExecutionConfiguration,
    llm_client: Arc<L>,
}

impl<L> MzTranslationExecutionPayload<L> {
    pub(crate) fn new(
        planning: MzTranslationPlanningConfiguration,
        execution: MzTranslationExecutionConfiguration,
        llm_client: Arc<L>,
    ) -> Self {
        Self {
            planning,
            execution,
            llm_client,
        }
    }

    pub(crate) fn planning(&self) -> &MzTranslationPlanningConfiguration {
        &self.planning
    }

    pub(crate) fn execution(&self) -> &MzTranslationExecutionConfiguration {
        &self.execution
    }

    pub(crate) fn llm_client(&self) -> &L {
        self.llm_client.as_ref()
    }
}

/// 外部配置无法建立为受信翻译 Profile 时的错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationProfileConfigurationError {
    BlankLanguageId {
        role: &'static str,
    },
    SurroundingWhitespaceInLanguageId {
        role: &'static str,
        value: String,
    },
    MissingSystemMarkdown,
    BlankSystemMarkdown {
        source_language: String,
        target_language: String,
    },
    DuplicateLanguagePair {
        source_language: String,
        target_language: String,
    },
}

impl fmt::Display for TranslationProfileConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BlankLanguageId { role } => write!(formatter, "{role} ID 为空"),
            Self::SurroundingWhitespaceInLanguageId { role, value } => {
                write!(formatter, "{role} ID 含首尾空白：{value:?}")
            }
            Self::MissingSystemMarkdown => formatter.write_str("没有配置任何语言对的系统提示词"),
            Self::BlankSystemMarkdown {
                source_language,
                target_language,
            } => write!(
                formatter,
                "语言对 {source_language} -> {target_language} 的系统提示词为空"
            ),
            Self::DuplicateLanguagePair {
                source_language,
                target_language,
            } => write!(
                formatter,
                "语言对 {source_language} -> {target_language} 的系统提示词重复"
            ),
        }
    }
}

impl Error for TranslationProfileConfigurationError {}

fn validate_language_id(
    role: &'static str,
    value: &str,
) -> Result<(), TranslationProfileConfigurationError> {
    if value.trim().is_empty() {
        return Err(TranslationProfileConfigurationError::BlankLanguageId { role });
    }
    if value.trim() != value {
        return Err(
            TranslationProfileConfigurationError::SurroundingWhitespaceInLanguageId {
                role,
                value: value.to_owned(),
            },
        );
    }
    Ok(())
}

/// 根据 CLI 选择建立一次翻译执行所需的受信配置。
///
/// 选择只在已经由外部加载的配置集合中同步完成；实现不得读取文件、执行 I/O、
/// 补充默认值或根据运行环境自行选择配置。
pub(crate) trait TranslationExecutionProfileResolver: Send + Sync {
    /// 下层翻译能力共同消费的完整执行配置。
    type Profile: Send + Sync + 'static;
    /// 配置不存在、无效或无法建立时的失败。
    type Error: Error + Send + Sync + 'static;

    /// 按调用方明确指定的 Profile 标识选择执行配置。
    fn resolve(&self, profile_id: &str) -> Result<Self::Profile, Self::Error>;
}

/// 一次翻译运行共享的不可变执行配置。
///
/// `payload` 由外部配置边界建立并保持不透明；本类型只固定所有翻译阶段共同需要的
/// 配置身份和逻辑任务并发上限。Debug 输出绝不会访问或展示 `payload`。
pub(crate) struct TranslationExecutionProfile<P> {
    id: String,
    max_in_flight_tasks: NonZeroUsize,
    payload: P,
}

impl<P> TranslationExecutionProfile<P> {
    pub(crate) fn new(
        id: impl Into<String>,
        max_in_flight_tasks: NonZeroUsize,
        payload: P,
    ) -> Self {
        Self {
            id: id.into(),
            max_in_flight_tasks,
            payload,
        }
    }

    /// 返回配置文件中用于精确选择本 Profile 的 ID。
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    /// 返回本次运行最多同时执行的逻辑翻译任务数。
    pub(crate) const fn max_in_flight_tasks(&self) -> NonZeroUsize {
        self.max_in_flight_tasks
    }

    /// 借用外部配置边界建立的受信执行载荷。
    pub(crate) fn payload(&self) -> &P {
        &self.payload
    }
}

impl<P> fmt::Debug for TranslationExecutionProfile<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TranslationExecutionProfile")
            .field("id", &self.id)
            .field("max_in_flight_tasks", &self.max_in_flight_tasks)
            .field("payload", &"[REDACTED]")
            .finish()
    }
}

/// 已由外部配置边界建立的完整 Profile 集合。
///
/// 构造成功后，集合非空、每个 ID 至少包含一个非空白字符，并且 ID 按原值精确唯一。
/// 集合不修剪、不正规化，也不折叠大小写。
pub(crate) struct TranslationProfileCatalog<P> {
    profiles_by_id: BTreeMap<String, Arc<TranslationExecutionProfile<P>>>,
}

impl<P> TranslationProfileCatalog<P> {
    pub(crate) fn new(
        profiles: impl IntoIterator<Item = TranslationExecutionProfile<P>>,
    ) -> Result<Self, TranslationProfileCatalogError> {
        let mut profiles_by_id = BTreeMap::new();

        for (index, profile) in profiles.into_iter().enumerate() {
            if profile.id().trim().is_empty() {
                return Err(TranslationProfileCatalogError::BlankId { index });
            }

            let id = profile.id().to_owned();
            if profiles_by_id.contains_key(&id) {
                return Err(TranslationProfileCatalogError::DuplicateId { id });
            }
            profiles_by_id.insert(id, Arc::new(profile));
        }

        if profiles_by_id.is_empty() {
            return Err(TranslationProfileCatalogError::Empty);
        }

        Ok(Self { profiles_by_id })
    }

    fn get(&self, id: &str) -> Option<Arc<TranslationExecutionProfile<P>>> {
        self.profiles_by_id.get(id).cloned()
    }

    fn available_ids(&self) -> Vec<String> {
        self.profiles_by_id.keys().cloned().collect()
    }
}

impl<P> fmt::Debug for TranslationProfileCatalog<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TranslationProfileCatalog")
            .field(
                "profile_ids",
                &self.profiles_by_id.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// 外部 Profile 集合无法建立时的配置错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationProfileCatalogError {
    /// 没有提供任何可选择的 Profile。
    Empty,
    /// 第 `index` 个 Profile 的 ID 不包含非空白字符。
    BlankId { index: usize },
    /// 两个 Profile 使用了完全相同的 ID。
    DuplicateId { id: String },
}

impl fmt::Display for TranslationProfileCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("没有配置任何翻译 Profile"),
            Self::BlankId { index } => {
                write!(formatter, "第 {} 个翻译 Profile 的 ID 为空", index + 1)
            }
            Self::DuplicateId { id } => write!(formatter, "翻译 Profile ID 重复：{id}"),
        }
    }
}

impl Error for TranslationProfileCatalogError {}

/// 在一个受信、不可变的 Profile 集合中执行精确 ID 选择。
pub(crate) struct InMemoryTranslationExecutionProfileResolver<P> {
    catalog: TranslationProfileCatalog<P>,
}

impl<P> InMemoryTranslationExecutionProfileResolver<P> {
    pub(crate) const fn new(catalog: TranslationProfileCatalog<P>) -> Self {
        Self { catalog }
    }
}

impl<P> fmt::Debug for InMemoryTranslationExecutionProfileResolver<P> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InMemoryTranslationExecutionProfileResolver")
            .field("catalog", &self.catalog)
            .finish()
    }
}

impl<P> TranslationExecutionProfileResolver for InMemoryTranslationExecutionProfileResolver<P>
where
    P: Send + Sync + 'static,
{
    type Profile = Arc<TranslationExecutionProfile<P>>;
    type Error = TranslationExecutionProfileResolveError;

    fn resolve(&self, profile_id: &str) -> Result<Self::Profile, Self::Error> {
        self.catalog.get(profile_id).ok_or_else(|| {
            TranslationExecutionProfileResolveError::UnknownProfile {
                requested_id: profile_id.to_owned(),
                available_ids: self.catalog.available_ids(),
            }
        })
    }
}

/// 调用方指定的 Profile 无法在受信集合中找到。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TranslationExecutionProfileResolveError {
    UnknownProfile {
        requested_id: String,
        available_ids: Vec<String>,
    },
}

impl fmt::Display for TranslationExecutionProfileResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownProfile {
                requested_id,
                available_ids,
            } => write!(
                formatter,
                "找不到翻译 Profile {requested_id}；可用 Profile：{}",
                available_ids.join("、")
            ),
        }
    }
}

impl Error for TranslationExecutionProfileResolveError {}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    use super::super::standard::StandardTranslationProfile;
    use super::*;

    #[derive(Eq, PartialEq)]
    struct SensitivePayload(&'static str);

    fn non_zero(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).expect("测试并发数必须非零")
    }

    fn profile(id: &str, secret: &'static str) -> TranslationExecutionProfile<SensitivePayload> {
        TranslationExecutionProfile::new(id, non_zero(3), SensitivePayload(secret))
    }

    fn catalog() -> TranslationProfileCatalog<SensitivePayload> {
        TranslationProfileCatalog::new([
            profile("alpha", "alpha-secret"),
            profile("beta", "beta-secret"),
        ])
        .expect("测试 Profile 集合应该合法")
    }

    #[test]
    fn profile_exposes_only_trusted_read_only_values() {
        let profile = profile("alpha", "alpha-secret");

        assert_eq!(profile.id(), "alpha");
        assert_eq!(profile.max_in_flight_tasks(), non_zero(3));
        assert!(profile.payload() == &SensitivePayload("alpha-secret"));
    }

    #[test]
    fn catalog_rejects_empty_blank_and_duplicate_profiles() {
        assert_eq!(
            TranslationProfileCatalog::<SensitivePayload>::new([]).expect_err("空集合应该失败"),
            TranslationProfileCatalogError::Empty
        );

        for blank_id in ["", "   ", "\t\r\n"] {
            assert_eq!(
                TranslationProfileCatalog::new([profile(blank_id, "secret")])
                    .expect_err("空白 ID 应该失败"),
                TranslationProfileCatalogError::BlankId { index: 0 }
            );
        }

        assert_eq!(
            TranslationProfileCatalog::new([
                profile("same", "first-secret"),
                profile("same", "second-secret"),
            ])
            .expect_err("重复 ID 应该失败"),
            TranslationProfileCatalogError::DuplicateId {
                id: "same".to_owned(),
            }
        );
    }

    #[test]
    fn exact_ids_are_not_trimmed_normalized_or_case_folded() {
        let resolver = InMemoryTranslationExecutionProfileResolver::new(
            TranslationProfileCatalog::new([
                profile("alpha", "lower-secret"),
                profile("Alpha", "upper-secret"),
                profile(" alpha ", "spaced-secret"),
            ])
            .expect("三个精确 ID 应该互不重复"),
        );

        assert_eq!(
            resolver.resolve("alpha").expect("小写 ID 应该存在").id(),
            "alpha"
        );
        assert_eq!(
            resolver.resolve("Alpha").expect("大写 ID 应该存在").id(),
            "Alpha"
        );
        assert_eq!(
            resolver
                .resolve(" alpha ")
                .expect("带空格的原始 ID 应该存在")
                .id(),
            " alpha "
        );
        assert!(matches!(
            resolver.resolve("ALPHA"),
            Err(TranslationExecutionProfileResolveError::UnknownProfile { .. })
        ));
    }

    #[test]
    fn repeated_resolution_returns_the_same_arc_snapshot() {
        let resolver = InMemoryTranslationExecutionProfileResolver::new(catalog());

        let first = resolver.resolve("alpha").expect("Profile 应该存在");
        let second = resolver.resolve("alpha").expect("Profile 应该存在");

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn unknown_profile_reports_requested_and_sorted_available_ids() {
        let resolver = InMemoryTranslationExecutionProfileResolver::new(catalog());

        let error = resolver
            .resolve("missing")
            .expect_err("未知 Profile 不得回退到其它配置");

        assert_eq!(
            error,
            TranslationExecutionProfileResolveError::UnknownProfile {
                requested_id: "missing".to_owned(),
                available_ids: vec!["alpha".to_owned(), "beta".to_owned()],
            }
        );
        assert!(error.source().is_none());
    }

    #[test]
    fn concurrent_resolution_has_no_shared_active_selection() {
        let resolver = Arc::new(InMemoryTranslationExecutionProfileResolver::new(catalog()));
        let expected_alpha = resolver.resolve("alpha").expect("alpha 应该存在");
        let expected_beta = resolver.resolve("beta").expect("beta 应该存在");
        let barrier = Arc::new(Barrier::new(3));

        let alpha_resolver = Arc::clone(&resolver);
        let alpha_barrier = Arc::clone(&barrier);
        let alpha = thread::spawn(move || {
            alpha_barrier.wait();
            alpha_resolver.resolve("alpha").expect("alpha 应该存在")
        });

        let beta_resolver = Arc::clone(&resolver);
        let beta_barrier = Arc::clone(&barrier);
        let beta = thread::spawn(move || {
            beta_barrier.wait();
            beta_resolver.resolve("beta").expect("beta 应该存在")
        });

        barrier.wait();
        let resolved_alpha = alpha.join().expect("alpha 选择线程不应 panic");
        let resolved_beta = beta.join().expect("beta 选择线程不应 panic");

        assert!(Arc::ptr_eq(&resolved_alpha, &expected_alpha));
        assert!(Arc::ptr_eq(&resolved_beta, &expected_beta));
        assert!(!Arc::ptr_eq(&resolved_alpha, &resolved_beta));
        assert!(Arc::ptr_eq(
            &resolver.resolve("alpha").expect("alpha 应该保持可选"),
            &expected_alpha
        ));
    }

    #[test]
    fn debug_output_never_reads_or_exposes_payload() {
        let profile = profile("private", "never-print-this-secret");
        let profile_debug = format!("{profile:?}");
        assert!(profile_debug.contains("private"));
        assert!(profile_debug.contains("[REDACTED]"));
        assert!(!profile_debug.contains("never-print-this-secret"));

        let resolver = InMemoryTranslationExecutionProfileResolver::new(
            TranslationProfileCatalog::new([profile]).expect("Profile 集合应该合法"),
        );
        let resolver_debug = format!("{resolver:?}");
        assert!(resolver_debug.contains("private"));
        assert!(!resolver_debug.contains("never-print-this-secret"));
    }

    #[test]
    fn resolver_and_selected_profile_are_send_and_sync_without_clone_payload() {
        fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<InMemoryTranslationExecutionProfileResolver<SensitivePayload>>();
        assert_send_sync::<Arc<TranslationExecutionProfile<SensitivePayload>>>();
    }

    #[test]
    fn mz_payload_keeps_every_external_planning_and_execution_choice_exact() {
        let language_pair =
            TranslationProfileLanguagePair::new("ja", "zh-Hans").expect("语言对应合法");
        let planning = MzTranslationPlanningConfiguration::new(
            non_zero(4),
            non_zero(24_000),
            [(language_pair.clone(), "# 完整系统提示词".to_owned())],
        )
        .expect("规划配置应合法");
        let execution = MzTranslationExecutionConfiguration::new(
            vec![Duration::from_millis(250), Duration::from_secs(2)],
            Duration::from_secs(30),
        );
        let client = Arc::new(SensitivePayload("secret"));
        let payload = MzTranslationExecutionPayload::new(planning, execution, Arc::clone(&client));

        assert_eq!(payload.planning().scope_concurrency(), non_zero(4));
        assert_eq!(
            payload.planning().max_message_characters(),
            non_zero(24_000)
        );
        assert_eq!(
            payload.planning().system_markdown(&language_pair),
            Some("# 完整系统提示词")
        );
        assert_eq!(
            payload.execution().network_retry_delays(),
            [Duration::from_millis(250), Duration::from_secs(2)]
        );
        assert_eq!(
            payload.execution().max_network_retry_after(),
            Duration::from_secs(30)
        );
        assert!(std::ptr::eq(payload.llm_client(), client.as_ref()));
    }

    #[test]
    fn profile_configuration_rejects_ambiguous_language_pairs_and_prompts() {
        assert_eq!(
            TranslationProfileLanguagePair::new(" ja", "zh-Hans")
                .expect_err("语言 ID 首尾空白应失败"),
            TranslationProfileConfigurationError::SurroundingWhitespaceInLanguageId {
                role: "源语言",
                value: " ja".to_owned(),
            }
        );

        assert_eq!(
            MzTranslationPlanningConfiguration::new(non_zero(1), non_zero(1), [])
                .expect_err("缺少系统提示词应失败"),
            TranslationProfileConfigurationError::MissingSystemMarkdown
        );

        let pair = TranslationProfileLanguagePair::new("en", "zh-Hans").expect("语言对应合法");
        assert_eq!(
            MzTranslationPlanningConfiguration::new(
                non_zero(1),
                non_zero(1),
                [(pair, " \n".to_owned())],
            )
            .expect_err("空白系统提示词应失败"),
            TranslationProfileConfigurationError::BlankSystemMarkdown {
                source_language: "en".to_owned(),
                target_language: "zh-Hans".to_owned(),
            }
        );
    }

    #[test]
    fn concrete_and_arc_profiles_satisfy_the_standard_profile_contract() {
        let concrete = profile("alpha", "alpha-secret");
        assert_eq!(
            StandardTranslationProfile::max_in_flight_tasks(&concrete),
            non_zero(3)
        );

        let selected = InMemoryTranslationExecutionProfileResolver::new(catalog())
            .resolve("alpha")
            .expect("alpha 应该存在");
        assert_eq!(
            StandardTranslationProfile::max_in_flight_tasks(&selected),
            non_zero(3)
        );
    }
}
