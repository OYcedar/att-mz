use std::path::{Path, PathBuf};

use super::ProjectName;

pub(crate) mod builtin;
pub(crate) mod document;
pub(crate) mod lua;
mod model;
pub(crate) mod rules;
pub(crate) mod service;
pub(crate) mod store;

/// 提取指定 RPG Maker 游戏文本所需的输入。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtractInput {
    pub name: ProjectName,
}

/// 把本次 Rules 文件与唯一 Rules 执行能力绑定为不可拆分的依赖。
pub(crate) struct SelectedRules<R> {
    rules_path: PathBuf,
    executor: R,
}

impl<R> SelectedRules<R> {
    pub(crate) fn new(rules_path: PathBuf, executor: R) -> Self {
        Self {
            rules_path,
            executor,
        }
    }

    pub(crate) fn rules_path(&self) -> &Path {
        &self.rules_path
    }

    pub(crate) fn executor(&self) -> &R {
        &self.executor
    }
}

/// 提取成功后交还给 CLI 的最小结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtractOutput {
    pub name: ProjectName,
}

#[cfg(test)]
mod full_tree_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_rules_binds_path_and_executor() {
        let selected = SelectedRules::new(PathBuf::from("rules.toml"), 7_u8);
        assert_eq!(selected.rules_path(), Path::new("rules.toml"));
        assert_eq!(*selected.executor(), 7);
    }
}
