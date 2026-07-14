use std::error::Error;
use std::future::Future;
use std::path::PathBuf;

use super::ProjectName;

mod asset_reader;
mod deduplication;
pub(crate) mod executor;
mod language;
mod lua;
mod placeholder;
mod planner;
mod planning_resource;
pub(crate) mod profile;
mod result_store;
mod service;
pub(crate) mod standard;

#[cfg(test)]
mod full_tree_tests;

/// 翻译指定 MZ 游戏所需的输入。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranslateInput {
    pub name: ProjectName,
    pub llm_id: String,
    /// 本次标准翻译使用的外部术语表；`None` 不表示权威空术语表。
    pub terminology_path: Option<PathBuf>,
    /// 本次补充的占位符规则；`None` 不关闭标准翻译内置的 MZ 保护规格。
    pub placeholder_rules_path: Option<PathBuf>,
    /// 可选的可信 Lua 翻译程序；真实 Host 仍由后续根适配器提供。
    pub lua_script: Option<PathBuf>,
}

/// 翻译成功后交还给 CLI 的最小结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranslateOutput {
    pub name: ProjectName,
    pub llm_id: String,
}

/// 完成一个 MZ 游戏翻译用例。
pub trait TranslateUseCase: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn execute(
        &self,
        input: TranslateInput,
    ) -> impl Future<Output = Result<TranslateOutput, Self::Error>> + Send;
}
