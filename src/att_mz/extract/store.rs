#![allow(dead_code, reason = "提取资产 Store 尚未接入生产数据库适配器")]

//! Rust 标准提取资产的快照替换契约。
//!
//! 两个窄接口可以由同一个生产 Store 实现，但调用边界始终明确本次替换的是
//! Builtin 还是 Rules 所拥有的数据。

use std::error::Error;
use std::future::Future;

use crate::att_mz::project::OpenedProject;

use super::model::{BuiltinSnapshot, RulesSnapshot};

pub(crate) mod asset_store;

/// 原子替换 Builtin 拥有的标准文本快照。
///
/// 实现保证：
///
/// - 首次调用可以建立 Rust 标准资产结构；
/// - 只替换 Builtin 叶子，不删除 Rules 叶子或 Lua 自建表；
/// - 同一精确地址且原文相同的叶子继承译文，原文变化只清除该叶子译文；
/// - 新叶子进入未翻译状态，消失叶子被删除；
/// - 与 Rules 已拥有的具体叶子冲突时整个替换失败；
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
/// - Rules-only 首次调用也可以建立 Rust 标准资产结构；
/// - 只替换 Rules 叶子，不删除 Builtin 叶子或 Lua 自建表；
/// - 同一精确地址且原文相同的叶子继承译文，原文变化只清除该叶子译文；
/// - 新叶子进入未翻译状态，消失叶子被删除；
/// - 与 Builtin 已拥有的具体叶子冲突时整个替换失败；
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
}
