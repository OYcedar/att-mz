//! Rust 标准提取资产的快照替换契约。
//!
//! 三个窄接口由同一个生产 Store 内核实现，但调用边界始终明确本次替换的是
//! Builtin、Rules 还是 Lua 所拥有的数据。

use std::error::Error;
use std::future::Future;

use crate::rpg_maker::dialogue::MvDialogueDefinition;
use crate::rpg_maker::managed_translation::ManagedTranslationSnapshot;
use crate::rpg_maker::project::OpenedProject;

use super::model::{BuiltinSnapshot, RulesSnapshot};
pub(crate) use super::model::{
    ExtractedTextGroup, ExtractedTextUnit, LuaSnapshot, SnapshotModelError,
};

pub(crate) mod asset_store;
#[cfg(test)]
pub(crate) use asset_store::RpgMakerExtractionAssetStoreError;

/// Builtin 快照与 MV 对话定义在同一事务中的更新意图。
///
/// MZ 使用 `Reuse`；MV 提供新定义时使用 `Replace`。清空 MV 规则由一个空的受信
/// `MvDialogueDefinition` 表达，不建立第三种状态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum BuiltinProjectDefinitionUpdate {
    Reuse,
    Replace(MvDialogueDefinition),
}

/// 原子替换 Builtin 拥有的标准文本快照。
///
/// 实现保证：
///
/// - 依赖 Init 已建立的完整当前 schema，不在提取阶段建表；
/// - 只替换 Builtin 语义单元，不删除 Rules/Lua 单元或 Lua 自建表；
/// - owner、逻辑组、单元角色、完整源内容和源上下文均相同的单元成对继承
///   `translation_content_json + translation_state`；物化配方外壳与 sibling 变化不扩大失效范围；
/// - 新单元进入未翻译状态，消失单元被删除；
/// - 与其他 owner 的物理修改目标发生任何冲突时整次替换失败；
/// - `Replace` 的 MV 对话定义与组、单元、目标及 owner state 在同一事务中提交；
/// - 快照与 owner 指纹完全相同时直接成功且不发起写事务；
/// - 一个快照在单个事务中替换，不会出现部分快照；驱动确认未提交时旧快照保持，
///   提交结果未知时显式返回不确定终态。
pub(crate) trait BuiltinSnapshotStore: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn replace_builtin(
        &self,
        project: &OpenedProject,
        snapshot: BuiltinSnapshot,
        project_definition_update: BuiltinProjectDefinitionUpdate,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// 原子替换 Rules 拥有的标准文本快照。
///
/// 实现保证：
///
/// - 依赖 Init 已建立的完整当前 schema，不在提取阶段建表；
/// - 只替换 Rules 语义单元，不删除 Builtin/Lua 单元或 Lua 自建表；
/// - owner、逻辑组、单元角色、完整源内容和源上下文均相同的单元成对继承
///   `translation_content_json + translation_state`；物化配方外壳与 sibling 变化不扩大失效范围；
/// - 新单元进入未翻译状态，消失单元被删除；
/// - 与其他 owner 的物理修改目标发生任何冲突时整次替换失败；
/// - 当前 TOML 的 `rule = []` 通过 `deactivate_rules` 移除 owner state 并级联清理资产；
/// - 快照与 owner 指纹完全相同时直接成功且不发起写事务；
/// - 一个快照在单个事务中替换，不会出现部分快照；驱动确认未提交时旧快照保持，
///   提交结果未知时显式返回不确定终态。
///
/// 译文继承与删除粒度始终是具体语义单元，组内其他单元不会扩大继承粒度。
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
/// Standard 与托管翻译是两个独立语义域，但同一次干净脚本产生的意图必须共享事务；
/// active 空快照、逐单元继承、冲突检查和明确事务终态均由生产 Store 收敛。
pub(crate) trait LuaSnapshotStore: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    /// 在唯一事务中收敛一次干净 Lua Extract 同时产生的 Standard 与 Managed 意图。
    ///
    /// `None` 表示脚本未声明该域，存储必须完整保留其现状；两个域均为 `None`
    /// 时不得发起写事务。
    fn apply_lua(
        &self,
        project: &OpenedProject,
        standard: Option<LuaStandardSnapshotMutation>,
        managed: Option<ManagedTranslationSnapshot>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Extract Lua 被移除或以零字节程序停用时，在唯一事务中清除两个 Lua owner。
    fn deactivate_lua(
        &self,
        project: &OpenedProject,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// Lua Extract 对 Standard owner 的可选快照变更。
///
/// 外层 `Option` 表示未调用相关接口并保留现状；`Replace(empty)` 仍建立 active
/// 空快照，而 `Deactivate` 明确移除 Standard Lua owner。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LuaStandardSnapshotMutation {
    Replace(LuaSnapshot),
    Deactivate,
}
