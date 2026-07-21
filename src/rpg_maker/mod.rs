//! RPG Maker MV 与 MZ 共用的唯一纵向实现。
//!
//! 应用入口只选择受信引擎布局；项目、提取、翻译、写回及 Lua 均由本模块拥有。

pub(crate) mod dialogue;
#[cfg(test)]
pub(crate) mod documentation_test;
pub(crate) mod extract;
pub(crate) mod init;
pub(crate) mod location_codec;
pub(crate) mod lua;
pub(crate) mod model;
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

use std::path::{Path, PathBuf};

pub(crate) use project::MaxFullwidthChars;
pub(crate) use project_name::ProjectName;

/// 当前支持的 RPG Maker 引擎。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum RpgMakerEngine {
    Mz,
    Mv,
}

impl RpgMakerEngine {
    pub(crate) const fn storage_name(self) -> &'static str {
        match self {
            Self::Mz => "mz",
            Self::Mv => "mv",
        }
    }
}

/// 一个引擎纵向切片提交给共享项目能力的已确认目录布局。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerLayout {
    engine: RpgMakerEngine,
    content_directory: Option<&'static str>,
    core_script: &'static str,
}

impl RpgMakerLayout {
    pub(crate) const MZ: Self = Self {
        engine: RpgMakerEngine::Mz,
        content_directory: None,
        core_script: "rmmz_core.js",
    };

    pub(crate) const MV: Self = Self {
        engine: RpgMakerEngine::Mv,
        content_directory: Some("www"),
        core_script: "rpg_core.js",
    };

    pub(crate) const fn engine(self) -> RpgMakerEngine {
        self.engine
    }

    pub(crate) const fn core_script(self) -> &'static str {
        self.core_script
    }

    pub(crate) const fn content_directory(self) -> Option<&'static str> {
        self.content_directory
    }

    pub(crate) fn game_content_root(self, game_root: &Path) -> PathBuf {
        self.content_directory.map_or_else(
            || game_root.to_path_buf(),
            |directory| game_root.join(directory),
        )
    }

    pub(crate) fn map_content_relative(self, logical_relative: &Path) -> PathBuf {
        self.content_directory.map_or_else(
            || logical_relative.to_path_buf(),
            |directory| PathBuf::from(directory).join(logical_relative),
        )
    }

    pub(crate) fn data_relative(self) -> PathBuf {
        self.content_directory.map_or_else(
            || PathBuf::from("data"),
            |directory| PathBuf::from(directory).join("data"),
        )
    }

    pub(crate) fn js_relative(self) -> PathBuf {
        self.content_directory.map_or_else(
            || PathBuf::from("js"),
            |directory| PathBuf::from(directory).join("js"),
        )
    }

    pub(crate) fn source_data_relative(self) -> PathBuf {
        PathBuf::from("source").join(self.data_relative())
    }

    pub(crate) fn source_js_relative(self) -> PathBuf {
        PathBuf::from("source").join(self.js_relative())
    }

    pub(crate) fn write_back_data_relative(self) -> PathBuf {
        PathBuf::from("write_back").join(self.data_relative())
    }

    pub(crate) fn write_back_js_relative(self) -> PathBuf {
        PathBuf::from("write_back").join(self.js_relative())
    }
}

/// 把一次命令选择的 Lua 脚本和唯一执行能力绑定为不可拆分的依赖。
pub(crate) struct SelectedLua<L> {
    program: lua::runtime::OwnedLuaProgram,
    executor: L,
}

impl<L> SelectedLua<L> {
    pub(crate) fn new(program: lua::runtime::OwnedLuaProgram, executor: L) -> Self {
        Self { program, executor }
    }

    pub(crate) fn script_path(&self) -> &Path {
        self.program.main_script_path()
    }

    pub(crate) const fn program(&self) -> &lua::runtime::OwnedLuaProgram {
        &self.program
    }

    pub(crate) fn executor(&self) -> &L {
        &self.executor
    }
}
