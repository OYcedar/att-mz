use std::path::PathBuf;

use crate::project_name::ProjectName;

pub(crate) mod asset_reader;
mod deduplication;
pub(crate) mod executor;
pub(crate) mod pipeline;
pub(crate) mod placeholder;
pub(crate) mod planner;
pub(crate) mod profile;
pub(crate) mod result_store;
pub(crate) mod semantics;
pub(crate) mod service;
pub(crate) mod task_record;

/// 翻译指定 RPG Maker 游戏所需的输入。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranslateInput {
    pub name: ProjectName,
    /// 本次翻译使用的外部术语表；`None` 不表示权威空术语表。
    pub terminology_path: Option<PathBuf>,
    /// 本次补充的占位符规则；`None` 不关闭 RPG Maker 内置保护规格。
    pub placeholder_rules_path: Option<PathBuf>,
}

/// 一轮 RPG Maker 翻译的正常业务汇总。
///
/// 剩余译文大于零仍然表示命令正常完成；调用方不得把部分产出或未产出升级为错误。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TranslationSummary {
    pub total_tasks: usize,
    pub started_tasks: usize,
    pub not_started_tasks: usize,
    pub complete_tasks: usize,
    pub partial_tasks: usize,
    pub unavailable_tasks: usize,
    pub accepted_decisions: usize,
    pub written_locations: usize,
    pub remaining_decisions: usize,
    pub remaining_locations: usize,
    pub protocol_diagnostics: usize,
    pub recoverable_request_exhaustions: usize,
    pub request_admission_stopped: bool,
    pub retained: usize,
    pub invalidated: usize,
    pub not_applicable: usize,
    pub reused: usize,
}

impl TranslationSummary {
    /// Task 协议问题或仍未解决的决策、位置都表示本次 Translate 尚未完整。
    ///
    /// `total_tasks == 0` 只说明没有发起模型任务，不能覆盖从项目状态观察到的剩余内容。
    pub(crate) const fn is_incomplete(self) -> bool {
        self.partial_tasks > 0
            || self.unavailable_tasks > 0
            || self.not_started_tasks > 0
            || self.remaining_decisions > 0
            || self.remaining_locations > 0
    }
}

/// 翻译命令正常完成后交还给 CLI 的结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranslateOutput {
    pub name: ProjectName,
    pub profile_id: String,
    pub summary: TranslationSummary,
}
