//! RPG Maker MZ 纵向切片。
//!
//! 命令行解析、生产根构造和进程呈现属于 `application`；本模块只拥有 MZ
//! 业务输入、输出与用例实现。

use std::path::{Path, PathBuf};

pub(crate) mod audit;
pub(crate) mod extract;
pub(crate) mod init;
pub(crate) mod location_codec;
pub(crate) mod lua;
pub(crate) mod placeholder_token;
pub(crate) mod project;
pub(crate) mod project_database;
pub(crate) mod project_lease;
mod project_name;
pub(crate) mod standard_asset;
pub(crate) mod tag;
pub(crate) mod text;
pub(crate) mod translate;
pub(crate) mod write_back;

pub(crate) use project::MaxFullwidthChars;
pub(crate) use project_name::ProjectName;

/// MZ 在共享项目根与锁根下使用的固定命名空间。
pub(crate) const ENGINE_DIRECTORY_NAME: &str = "mz";

/// 把一次命令选择的 Lua 脚本和唯一执行能力绑定为不可拆分的依赖。
pub(crate) struct SelectedLua<L> {
    script_path: PathBuf,
    executor: L,
}

impl<L> SelectedLua<L> {
    pub(crate) fn new(script_path: PathBuf, executor: L) -> Self {
        Self {
            script_path,
            executor,
        }
    }

    pub(crate) fn script_path(&self) -> &Path {
        &self.script_path
    }

    pub(crate) fn executor(&self) -> &L {
        &self.executor
    }
}
