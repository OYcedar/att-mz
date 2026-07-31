use std::sync::Arc;

use self::rules::RulesProgram;
use crate::progress::{NoopProgressObserver, ProgressObserver, ProgressSnapshot};
use crate::project_name::ProjectName;

pub(crate) mod builtin;
pub(crate) mod document;
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
/// `Builtin` / `Rules` 只表达 owner 的 `i/N`；其余变体拥有
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
    /// Rules command 规则跳过的非字符串直接参数；未选择 Rules 时为空。
    pub rules_warnings: Vec<RulesCommandNonStringWarning>,
}

/// command Rule 直接选择的参数不是字符串时，可安全跳过的 JSON 类型。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum RulesCommandNonStringType {
    Null,
    Boolean,
    Number,
    Array,
    Object,
}

impl RulesCommandNonStringType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Boolean => "boolean",
            Self::Number => "number",
            Self::Array => "array",
            Self::Object => "object",
        }
    }
}

/// 一类被 Rules command 直接参数规则跳过的非字符串值。
///
/// 聚合键不包含原始值或命令位置，避免诊断泄露正文并控制警告数量。
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RulesCommandNonStringWarning {
    pub rule_number: usize,
    pub source_file: String,
    pub command_code: i64,
    pub parameter: usize,
    pub actual_type: RulesCommandNonStringType,
    pub skipped_count: u64,
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
