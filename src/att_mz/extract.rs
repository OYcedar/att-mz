use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};

use super::ProjectName;

mod builtin;
pub(crate) mod document;
mod lua;
mod model;
mod rules;
mod service;
mod store;

/// 提取指定 MZ 游戏文本所需的输入。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtractInput {
    pub name: ProjectName,
    pub selection: ExtractionSelection,
}

/// 一次提取调用中被选择的能力。
///
/// 字段保持私有，使空选择、重复阶段和任意阶段顺序无法进入用例内部。实际执行顺序
/// 永远由提取用例确定为 Builtin、Rules、Lua。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtractionSelection {
    builtin: bool,
    rules_path: Option<PathBuf>,
    lua_script: Option<PathBuf>,
}

impl ExtractionSelection {
    /// 建立一个至少选择一项能力的提取请求。
    pub fn new(
        builtin: bool,
        rules_path: Option<PathBuf>,
        lua_script: Option<PathBuf>,
    ) -> Result<Self, EmptyExtractionSelection> {
        if !builtin && rules_path.is_none() && lua_script.is_none() {
            return Err(EmptyExtractionSelection);
        }

        Ok(Self {
            builtin,
            rules_path,
            lua_script,
        })
    }

    /// 是否刷新 MZ 固定位置文本。
    pub fn builtin(&self) -> bool {
        self.builtin
    }

    /// 本次使用的 Rules JSON 文件。
    pub fn rules_path(&self) -> Option<&Path> {
        self.rules_path.as_deref()
    }

    /// 本次使用的自由 Lua 提取脚本。
    pub fn lua_script(&self) -> Option<&Path> {
        self.lua_script.as_deref()
    }

    pub(crate) fn into_parts(self) -> (bool, Option<PathBuf>, Option<PathBuf>) {
        (self.builtin, self.rules_path, self.lua_script)
    }
}

/// 调用方没有选择任何提取能力。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmptyExtractionSelection;

impl fmt::Display for EmptyExtractionSelection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("至少需要选择 builtin、rules 或 lua 中的一项")
    }
}

impl Error for EmptyExtractionSelection {}

/// 提取成功后交还给 CLI 的最小结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtractOutput {
    pub name: ProjectName,
}

/// 完成一个 MZ 游戏文本提取用例。
///
/// 一次调用可以组合一种或多种提取能力；成功表示本次选择的全部阶段均已完成。
pub trait ExtractUseCase: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn execute(
        &self,
        input: ExtractInput,
    ) -> impl Future<Output = Result<ExtractOutput, Self::Error>> + Send;
}

#[cfg(test)]
mod full_tree_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_rejects_the_only_invalid_state() {
        assert_eq!(
            ExtractionSelection::new(false, None, None),
            Err(EmptyExtractionSelection)
        );
    }

    #[test]
    fn selection_keeps_each_intent_without_exposing_an_ordered_task_list() {
        let selection = ExtractionSelection::new(
            true,
            Some(PathBuf::from("rules.json")),
            Some(PathBuf::from("extract.lua")),
        )
        .expect("非空选择应该合法");

        assert!(selection.builtin());
        assert_eq!(selection.rules_path(), Some(Path::new("rules.json")));
        assert_eq!(selection.lua_script(), Some(Path::new("extract.lua")));
    }
}
