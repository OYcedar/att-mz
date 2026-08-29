//! 进程参数、配置、命令装配与生命周期边界。

use crate::runtime::project_log::{TranslationEngineSummary, TranslationTaskCounters};

/// Translate 在失败或取消时仍需向终端呈现的当前事实。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TranslationTerminalSummary {
    pub(crate) tasks: TranslationTaskCounters,
    pub(crate) engine: TranslationEngineSummary,
}

pub(crate) mod arguments;
pub(crate) mod command;
pub(crate) mod config;
pub(crate) mod generic_command;
pub(crate) mod process;
pub(crate) mod project_log;
pub(crate) mod termination;
pub(crate) mod test_command;
pub(crate) mod translation_prompt;
