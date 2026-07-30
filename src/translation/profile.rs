use std::time::Duration;

/// 单个模型请求使用的网络重试策略。
///
/// 该策略来自 LLM Client 配置，与具体翻译引擎无关。各引擎只读取已经解析完成的
/// 延迟序列和服务端 `Retry-After` 的最大接受时间。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TranslationRequestConfiguration {
    network_retry_delays: Vec<Duration>,
    max_network_retry_after: Duration,
}

impl TranslationRequestConfiguration {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_configuration_preserves_the_selected_retry_policy() {
        let configuration = TranslationRequestConfiguration::new(
            vec![Duration::from_millis(250), Duration::from_secs(2)],
            Duration::from_secs(30),
        );

        assert_eq!(
            configuration.network_retry_delays(),
            [Duration::from_millis(250), Duration::from_secs(2)]
        );
        assert_eq!(
            configuration.max_network_retry_after(),
            Duration::from_secs(30)
        );
    }
}
