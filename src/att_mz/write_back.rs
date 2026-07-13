use std::error::Error;
use std::future::Future;
use std::path::PathBuf;

use super::ProjectName;

/// 写回指定 MZ 游戏所需的输入。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteBackInput {
    pub name: ProjectName,
    /// 可选的可信 Lua 写回程序；具体执行能力由后续任务实现。
    pub lua_script: Option<PathBuf>,
}

/// 写回成功后交还给 CLI 的最小结果。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteBackOutput {
    pub name: ProjectName,
}

/// 完成一个 MZ 游戏文本写回用例。
pub trait WriteBackUseCase: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn execute(
        &self,
        input: WriteBackInput,
    ) -> impl Future<Output = Result<WriteBackOutput, Self::Error>> + Send;
}
