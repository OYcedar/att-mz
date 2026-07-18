use std::error::Error;
use std::future::Future;
use std::path::PathBuf;

use super::ProjectName;

pub(crate) mod asset_reader;
mod deduplication;
pub(crate) mod executor;
mod language_projection;
pub(crate) mod lua;
pub(crate) mod placeholder;
pub(crate) mod planner;
pub(crate) mod planning_resource;
pub(crate) mod profile;
pub(crate) mod result_store;
pub(crate) mod semantics;
pub(crate) mod service;
pub(crate) mod standard;

#[cfg(test)]
mod full_tree_tests;

/// 翻译指定 MZ 游戏所需的输入。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranslateInput {
    pub name: ProjectName,
    pub profile_id: String,
    /// 本次标准翻译使用的外部术语表；`None` 不表示权威空术语表。
    pub terminology_path: Option<PathBuf>,
    /// 本次补充的占位符规则；`None` 不关闭标准翻译内置的 MZ 保护规格。
    pub placeholder_rules_path: Option<PathBuf>,
    /// 可选的可信 Lua 翻译程序；真实 Host 仍由后续根适配器提供。
    pub lua_script: Option<PathBuf>,
}

/// 一轮标准翻译的正常业务汇总。
///
/// 剩余译文大于零仍然表示命令正常完成；调用方不得把部分产出或未产出升级为错误。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StandardTranslationSummary {
    pub total_tasks: usize,
    pub complete_tasks: usize,
    pub partial_tasks: usize,
    pub unavailable_tasks: usize,
    pub accepted_decisions: usize,
    pub written_locations: usize,
    pub remaining_decisions: usize,
    pub remaining_locations: usize,
    pub protocol_diagnostics: usize,
    pub recoverable_request_exhaustions: usize,
    pub retained: usize,
    pub invalidated: usize,
    pub not_applicable: usize,
    pub reused: usize,
}

/// 翻译命令正常完成后交还给 CLI 的结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranslateOutput {
    pub name: ProjectName,
    pub profile_id: String,
    pub standard: StandardTranslationSummary,
    pub lua_executed: bool,
}

/// 完成一个 MZ 游戏翻译用例。
pub trait TranslateUseCase: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn execute(
        &self,
        input: TranslateInput,
    ) -> impl Future<Output = Result<TranslateOutput, Self::Error>> + Send;
}
