//! MZ 各业务阶段共享的可信 Lua 调用边界。

use std::error::Error;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::ProjectName;
use super::project::OpenedProject;
use crate::project_database::StoredProjectRecord;

pub(crate) mod hosting;
pub(crate) mod runtime;
pub(crate) mod session;

/// 可信 Lua 在当前阶段能够访问的项目文件边界。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LuaProjectFileAccess {
    /// Extract 与 Translate 只接收 Init 冻结的 `source/data`、`source/js`。
    FrozenSource { source_root: PathBuf },
    /// WriteBack 同时接收冻结来源和已经发布的固定最新输出。
    PublishedWriteBack {
        source_root: PathBuf,
        output_root: PathBuf,
    },
}

impl LuaProjectFileAccess {
    pub(crate) fn source_root(&self) -> &Path {
        match self {
            Self::FrozenSource { source_root } | Self::PublishedWriteBack { source_root, .. } => {
                source_root
            }
        }
    }

    pub(crate) fn output_root(&self) -> Option<&Path> {
        match self {
            Self::FrozenSource { .. } => None,
            Self::PublishedWriteBack { output_root, .. } => Some(output_root),
        }
    }
}

/// 交给可信 Lua 程序的项目事实快照。
///
/// 冻结来源始终来自 Init 项目工作区；原游戏目录不是后续阶段的权威事实。只有
/// WriteBack 变体能够看到已经发布的 `output_root`，Extract/Translate 不携带它。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LuaProjectContext {
    name: ProjectName,
    file_access: LuaProjectFileAccess,
    database_path: PathBuf,
    source_language: String,
    target_language: String,
}

impl LuaProjectContext {
    pub(crate) fn from_opened_project(project: &OpenedProject) -> Self {
        Self {
            name: project.name().clone(),
            file_access: LuaProjectFileAccess::FrozenSource {
                source_root: project.source_root().to_path_buf(),
            },
            database_path: project.database_path().to_path_buf(),
            source_language: project.source_language().to_owned(),
            target_language: project.target_language().to_owned(),
        }
    }

    pub(crate) fn from_stored_record(project: &StoredProjectRecord) -> Self {
        Self {
            name: project.name().clone(),
            file_access: LuaProjectFileAccess::FrozenSource {
                source_root: project.source_root().to_path_buf(),
            },
            database_path: project.database_path().to_path_buf(),
            source_language: project.source_language().to_owned(),
            target_language: project.target_language().to_owned(),
        }
    }

    pub(crate) fn for_published_write_back(project: &OpenedProject, output_root: PathBuf) -> Self {
        Self {
            name: project.name().clone(),
            file_access: LuaProjectFileAccess::PublishedWriteBack {
                source_root: project.source_root().to_path_buf(),
                output_root,
            },
            database_path: project.database_path().to_path_buf(),
            source_language: project.source_language().to_owned(),
            target_language: project.target_language().to_owned(),
        }
    }

    pub(crate) fn name(&self) -> &ProjectName {
        &self.name
    }

    pub(crate) fn source_root(&self) -> &Path {
        self.file_access.source_root()
    }

    pub(crate) fn output_root(&self) -> Option<&Path> {
        self.file_access.output_root()
    }

    pub(crate) fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub(crate) fn source_language(&self) -> &str {
        &self.source_language
    }

    pub(crate) fn target_language(&self) -> &str {
        &self.target_language
    }
}

/// 可信 Lua 调用所处的业务阶段，只用于诊断和建立 `ctx.phase`。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LuaPhase {
    Extract,
    Translate,
    WriteBack,
}

/// 交给可信 Lua Host 的一次完整调用。
///
/// 枚举变体从类型上保证 Extract 不携带无关 Profile，而 Translate 一定拥有顶层已经
/// 选择的同一 `Arc` Profile。调用拥有全部事实，便于专用 worker 在调用 Future 被
/// 取消后继续完成受控清理，且不要求 Profile 载荷实现 `Clone`。
pub(crate) enum LuaInvocation<P> {
    Extract {
        script_path: PathBuf,
        project: LuaProjectContext,
    },
    Translate {
        script_path: PathBuf,
        project: LuaProjectContext,
        profile: Arc<P>,
    },
    WriteBack {
        script_path: PathBuf,
        project: LuaProjectContext,
    },
}

impl<P> LuaInvocation<P> {
    pub(crate) fn extract(script_path: PathBuf, project: LuaProjectContext) -> Self {
        Self::Extract {
            script_path,
            project,
        }
    }

    pub(crate) fn translate(
        script_path: PathBuf,
        project: LuaProjectContext,
        profile: Arc<P>,
    ) -> Self {
        Self::Translate {
            script_path,
            project,
            profile,
        }
    }

    pub(crate) fn write_back(script_path: PathBuf, project: LuaProjectContext) -> Self {
        Self::WriteBack {
            script_path,
            project,
        }
    }
}

/// 完整拥有可信 Lua 程序生命周期与项目能力桥接的 Host。
///
/// Lua 是用户明确选择并完全信任的本机程序，不建立安全沙箱。Host 负责加载脚本、
/// 建立 VM、打开同一项目数据库并注入 `ctx.db`，在 Translate 阶段根据拥有的同一
/// `Arc` Profile 注入 `ctx.llm`，以及执行和关闭全部资源。Host 不把原始数据库连接、凭据
/// 或 Profile 暴露给脚本。
///
/// Lua 自己拥有 schema、数据身份、译文继承、业务事务划分、模型消息、响应处理、
/// 重试和跨阶段协议；Rust 不扫描、解释、转换或默认消费 Lua 产物，也不会把整个
/// 脚本隐式包进长事务。Host 负责 worker、背压和阻塞隔离；公开 Future 不得阻塞
/// 异步执行器线程。Host 先预留 Runtime 容量，再打开 SQLite 会话，最后通过
/// `reservation.start` 同步移交调用面与唯一终结器。排队期或运行期取消都必须回滚
/// 仍未提交的活动事务并关闭数据库连接和 VM；已经显式提交的结果不由 Host 回滚。
pub(crate) trait TrustedLuaExecutionHost: Send + Sync {
    /// 与翻译配置 Resolver 产物一致的执行配置。
    type TranslationProfile: Send + Sync + 'static;
    /// Host 接管或执行失败。
    type Error: Error + Send + Sync + 'static;

    fn execute(
        &self,
        invocation: LuaInvocation<Self::TranslationProfile>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}
