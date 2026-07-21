use std::sync::Arc;

use self::rules::RulesProgram;
use super::ProjectName;
use crate::progress::{NoopProgressObserver, ProgressObserver, ProgressSnapshot};

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

/// Extract 当前正在执行的 owner 或 owner 内部阶段。
///
/// `Builtin` / `Rules` / `Lua` 只表达 owner 的 `i/N`；其余变体拥有
/// 各自的真实分母或 spinner，避免用一个假全局百分比混合不同工作量。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtractProgressPhase {
    Builtin,
    BuiltinDocuments,
    BuiltinWorkUnits,
    BuiltinCommit,
    Rules,
    RulesDocuments,
    RulesMatches,
    RulesCommit,
    Lua,
    LuaExecution,
    LuaCommit,
}

/// 在 Extract 纵向切片内共享的不可失败进度入口。
///
/// 每个业务阶段只发布绝对快照；具体终端或日志呈现由应用边界决定。
#[derive(Clone)]
pub(crate) struct ExtractProgress {
    observer: Arc<dyn ProgressObserver<ExtractProgressPhase>>,
}

impl ExtractProgress {
    pub(crate) fn new<Q>(observer: Q) -> Self
    where
        Q: ProgressObserver<ExtractProgressPhase> + 'static,
    {
        Self {
            observer: Arc::new(observer),
        }
    }

    pub(crate) fn indeterminate(&self, phase: ExtractProgressPhase) {
        self.observer
            .observe(ProgressSnapshot::indeterminate(phase));
    }

    pub(crate) fn determinate(&self, phase: ExtractProgressPhase, completed: u64, total: u64) {
        self.observer
            .observe(ProgressSnapshot::determinate(phase, completed, total));
    }
}

impl Default for ExtractProgress {
    fn default() -> Self {
        Self::new(NoopProgressObserver)
    }
}

/// 把本次 Rules 文件与唯一 Rules 执行能力绑定为不可拆分的依赖。
pub(crate) struct SelectedRules<R> {
    program: RulesProgram,
    executor: R,
}

impl<R> SelectedRules<R> {
    pub(crate) fn new(program: RulesProgram, executor: R) -> Self {
        Self { program, executor }
    }

    pub(crate) fn program(&self) -> &RulesProgram {
        &self.program
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
    fn selected_rules_binds_program_and_executor() {
        let program = RulesProgram::from_toml("rules.toml".into(), b"rule = []".to_vec())
            .expect("测试规则应合法");
        let selected = SelectedRules::new(program, 7_u8);
        assert_eq!(
            selected.program().diagnostic_path(),
            std::path::Path::new("rules.toml")
        );
        assert_eq!(*selected.executor(), 7);
    }
}
