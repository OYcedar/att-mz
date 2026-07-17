//! 跨领域复用的持久事件日志与运行身份根契约。
//!
//! 业务模块只提交已经建立的结构化事实。记录时间、物理顺序、轮转、刷盘和
//! 跨进程协调都由日志根拥有；运行身份由独立根在业务副作用开始前建立。

use std::error::Error;
use std::fmt;
use std::future::Future;

use uuid::Uuid;

/// 一次命令运行的全局唯一身份。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct RunId(Uuid);

impl RunId {
    pub(crate) const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    #[cfg(test)]
    pub(crate) const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// 在一次业务运行开始副作用前产生不可猜测的运行身份。
pub(crate) trait RunIdGenerator: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn generate(&self) -> Result<RunId, Self::Error>;
}

/// 按调用方定义的结构化事件类型追加持久日志。
///
/// 成功表示该条完整记录已经写入并通过 `sync_data` 确认。仅进入进程内队列、
/// 仅写入用户态缓冲或只完成 `write` 都不能返回成功。
pub(crate) trait PersistentEventLog<E>: Send + Sync
where
    E: Send + 'static,
{
    type Error: Error + Send + Sync + 'static;

    fn append(&self, event: E) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_id_uses_canonical_uuid_text() {
        let id = RunId::from_uuid(
            Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("测试 UUID 应合法"),
        );

        assert_eq!(id.to_string(), "550e8400-e29b-41d4-a716-446655440000");
    }
}
