//! 跨领域复用的持久事件日志根契约。
//!
//! 本模块只定义业务模块提交结构化事件所需的最小能力。日志文件、轮转、队列、
//! 刷盘和保留策略属于未来根适配器，并且必须由外部配置显式提供。

use std::error::Error;
use std::future::Future;

/// 按调用方定义的结构化事件类型追加持久日志。
///
/// 业务模块只依赖本契约，不直接选择日志库、格式或存储位置。成功表示事件已经达到
/// 外部配置声明的持久化终态；仅进入进程内易失队列不能返回成功。失败表示无法履行
/// 持久日志承诺。
pub(crate) trait PersistentEventLog<E>: Send + Sync
where
    E: Send + 'static,
{
    type Error: Error + Send + Sync + 'static;

    fn append(&self, event: E) -> impl Future<Output = Result<(), Self::Error>> + Send;
}
