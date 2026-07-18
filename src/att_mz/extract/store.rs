//! Rust 标准提取资产的快照替换契约。
//!
//! 三个窄接口由同一个生产 Store 内核实现，但调用边界始终明确本次替换的是
//! Builtin、Rules 还是 Lua 所拥有的数据。

use std::error::Error;
use std::future::Future;

use crate::att_mz::project::OpenedProject;

use super::model::{BuiltinSnapshot, RulesSnapshot};
pub(crate) use super::model::{
    ExtractedTextField, ExtractedTextGroup, LuaSnapshot, SnapshotModelError,
};

pub(crate) mod asset_store;

/// 原子替换 Builtin 拥有的标准文本快照。
///
/// 实现保证：
///
/// - 依赖 Init 已建立的完整当前 schema，不在提取阶段建表；
/// - 只替换 Builtin 叶子，不删除 Rules/Lua 叶子或 Lua 自建表；
/// - owner、表、unit、精确地址、字段名和原文均相同的叶子成对继承
///   `translation + translation_state`；group 与 sibling 变化不扩大失效范围；
/// - 新叶子进入未翻译状态，消失叶子被删除；
/// - 只与当前来源下新鲜的 Rules/Lua owner 检查精确地址冲突；
/// - 快照与 owner 指纹完全相同时直接成功且不发起写事务；
/// - 一个快照在单个事务中替换，不会出现部分快照；驱动确认未提交时旧快照保持，
///   提交结果未知时显式返回不确定终态。
pub(crate) trait BuiltinSnapshotStore: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn replace_builtin(
        &self,
        project: &OpenedProject,
        snapshot: BuiltinSnapshot,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 原子替换 Rules 拥有的标准文本快照。
///
/// 实现保证：
///
/// - 依赖 Init 已建立的完整当前 schema，不在提取阶段建表；
/// - 只替换 Rules 叶子，不删除 Builtin/Lua 叶子或 Lua 自建表；
/// - owner、表、unit、精确地址、字段名和原文均相同的叶子成对继承
///   `translation + translation_state`；group 与 sibling 变化不扩大失效范围；
/// - 新叶子进入未翻译状态，消失叶子被删除；
/// - 只与当前来源下新鲜的 Builtin/Lua owner 检查精确地址冲突；
/// - 非空 Rules 定义可提交 active 空快照；完整 `{}` 通过 `deactivate_rules`
///   移除 owner state 并级联清理资产；
/// - 快照与 owner 指纹完全相同时直接成功且不发起写事务；
/// - 一个快照在单个事务中替换，不会出现部分快照；驱动确认未提交时旧快照保持，
///   提交结果未知时显式返回不确定终态。
///
/// 译文继承与删除粒度始终是具体文本叶子，翻译上下文的复合分组不会扩大继承粒度。
pub(crate) trait RulesSnapshotStore: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn replace_rules(
        &self,
        project: &OpenedProject,
        snapshot: RulesSnapshot,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// 清空 Rules 资产并移除其 active owner state。
    fn deactivate_rules(
        &self,
        project: &OpenedProject,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 原子收敛 Lua 拥有的标准文本快照。
///
/// `replace_lua` 包括 active 空快照；`deactivate_lua` 则移除 owner state 并级联清理
/// Lua 标准资产。两者复用与 Builtin/Rules 相同的逐叶继承、冲突和事务不变量。
pub(crate) trait LuaSnapshotStore: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn replace_lua(
        &self,
        project: &OpenedProject,
        snapshot: LuaSnapshot,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    fn deactivate_lua(
        &self,
        project: &OpenedProject,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}
