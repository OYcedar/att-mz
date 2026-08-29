//! RPG Maker MV 与 MZ 共用的唯一纵向实现。
//!
//! 应用入口只选择受信引擎布局；项目、提取、翻译及写回均由本模块拥有。

pub(crate) mod asset;
pub(crate) mod asset_storage;
pub(crate) mod dialogue;
pub(crate) mod extract;
pub(crate) mod init;
pub(crate) mod location_codec;
pub(crate) mod model;
pub(crate) mod mutation_claim_summary;
pub(crate) mod plugin_document;
pub(crate) mod project;
pub(crate) mod project_database;
pub(crate) mod semantic_order;
pub(crate) mod structured_path;
pub(crate) mod text;
pub(crate) mod translate;
pub(crate) mod write_back;

use std::path::{Path, PathBuf};

use crate::diagnostic::RpgMakerComputeFailure;
use crate::execution::cpu::CpuTaskExecutionError;
use crate::runtime::cpu::CpuExecutorUnavailable;

pub(crate) fn compute_failure(
    source: &CpuTaskExecutionError<CpuExecutorUnavailable>,
) -> RpgMakerComputeFailure {
    match source {
        CpuTaskExecutionError::Cancelled => RpgMakerComputeFailure::Cancelled,
        CpuTaskExecutionError::Unavailable(CpuExecutorUnavailable::ShuttingDown) => {
            RpgMakerComputeFailure::ExecutorClosed
        }
        CpuTaskExecutionError::Unavailable(CpuExecutorUnavailable::StatePoisoned) => {
            RpgMakerComputeFailure::StatePoisoned
        }
        CpuTaskExecutionError::TaskPanicked => RpgMakerComputeFailure::WorkerPanicked,
    }
}

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
