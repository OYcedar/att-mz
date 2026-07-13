use std::error::Error;
use std::future::Future;
use std::path::PathBuf;

use super::ProjectName;

/// 翻译指定 MZ 游戏所需的输入。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranslateInput {
    pub name: ProjectName,
    pub llm_id: String,
    /// 可选的可信 Lua 翻译程序；具体执行能力由后续任务实现。
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
