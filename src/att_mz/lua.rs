//! MZ 各业务阶段共享的可信 Lua 调用边界。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use self::runtime::TrustedLuaExtractIntent;
use self::runtime::TrustedLuaTranslationSemantics;
use self::runtime::TrustedLuaWriteBackHostCalls;
use super::ProjectName;
use super::project::OpenedProject;
use crate::execution::OperationCompletion;

pub(crate) mod hosting;
pub(crate) mod json;
pub(crate) mod mz;
pub(crate) mod runtime;
pub(crate) mod session;

/// Lua 只读来源门面能够访问的项目内相对路径。
///
/// 该类型只表达逻辑边界；普通文件、目录与 reparse point 的现实观测仍由文件系统
/// 根完成。进入 Host 后路径一定从 `data` 或 `js` 开始，且无法逃出冻结来源根。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct LuaSourcePath {
    path: PathBuf,
    display: String,
}

impl LuaSourcePath {
    pub(crate) fn parse(value: &str) -> Result<Self, LuaSourcePathError> {
        if value.is_empty() {
            return Err(LuaSourcePathError::Empty);
        }
        if value.contains(':') {
            return Err(LuaSourcePathError::AlternateDataStream);
        }

        let mut path = PathBuf::new();
        let mut display_components = Vec::new();
        for component in Path::new(value).components() {
            let Component::Normal(component) = component else {
                return Err(match component {
                    Component::ParentDir => LuaSourcePathError::ParentTraversal,
                    Component::Prefix(_) | Component::RootDir => LuaSourcePathError::Absolute,
                    Component::CurDir => LuaSourcePathError::NonCanonical,
                    Component::Normal(_) => unreachable!(),
                });
            };
            let component = component.to_str().ok_or(LuaSourcePathError::InvalidUtf8)?;
            if component.is_empty() {
                return Err(LuaSourcePathError::NonCanonical);
            }
            path.push(component);
            display_components.push(component.to_owned());
        }

        let Some(root) = display_components.first() else {
            return Err(LuaSourcePathError::Empty);
        };
        if root != "data" && root != "js" {
            return Err(LuaSourcePathError::OutsideSourceRoots);
        }

        Ok(Self {
            path,
            display: display_components.join("/"),
        })
    }

    pub(crate) fn join_to(&self, source_root: &Path) -> PathBuf {
        source_root.join(&self.path)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.display
    }

    pub(crate) fn child(&self, name: &str) -> Result<Self, LuaSourcePathError> {
        if name.is_empty()
            || name
                .chars()
                .any(|character| matches!(character, '/' | '\\' | ':'))
            || matches!(name, "." | "..")
        {
            return Err(LuaSourcePathError::InvalidChildName);
        }
        Self::parse(&format!("{}/{name}", self.display))
    }
}

/// Lua 来源路径没有满足冻结 `data/**`、`js/**` 边界。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LuaSourcePathError {
    Empty,
    Absolute,
    ParentTraversal,
    AlternateDataStream,
    NonCanonical,
    OutsideSourceRoots,
    InvalidUtf8,
    InvalidChildName,
}

impl fmt::Display for LuaSourcePathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("来源路径不能为空"),
            Self::Absolute => formatter.write_str("来源路径必须是相对路径"),
            Self::ParentTraversal => formatter.write_str("来源路径不允许父级逃逸"),
            Self::AlternateDataStream => formatter.write_str("来源路径不允许 NTFS ADS"),
            Self::NonCanonical => formatter.write_str("来源路径必须使用规范路径段"),
            Self::OutsideSourceRoots => formatter.write_str("来源路径只允许 data/** 或 js/**"),
            Self::InvalidUtf8 => formatter.write_str("来源路径必须是 UTF-8"),
            Self::InvalidChildName => formatter.write_str("目录项名称不是有效来源路径段"),
        }
    }
}

impl Error for LuaSourcePathError {}

/// 可信 Lua 在当前阶段能够访问的项目文件边界。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LuaProjectFileAccess {
    /// Extract 与 Translate 只接收 Init 冻结的 `source/data`、`source/js`。
    FrozenSource { source_root: PathBuf },
    /// WriteBack 同时接收冻结来源和本次尚未发布的候选目录。
    CandidateWriteBack {
        source_root: PathBuf,
        output_root: PathBuf,
    },
}

impl LuaProjectFileAccess {
    pub(crate) fn source_root(&self) -> &Path {
        match self {
            Self::FrozenSource { source_root } | Self::CandidateWriteBack { source_root, .. } => {
                source_root
            }
        }
    }

    pub(crate) fn output_root(&self) -> Option<&Path> {
        match self {
            Self::FrozenSource { .. } => None,
            Self::CandidateWriteBack { output_root, .. } => Some(output_root),
        }
    }
}

/// 交给可信 Lua 程序的项目事实快照。
///
/// 冻结来源始终来自 Init 项目工作区；原游戏目录不是后续阶段的权威事实。只有
/// WriteBack 变体能够看到本次待修改的候选 `output_root`，Extract/Translate 不携带它。
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

    pub(crate) fn for_write_back_candidate(project: &OpenedProject, output_root: PathBuf) -> Self {
        Self {
            name: project.name().clone(),
            file_access: LuaProjectFileAccess::CandidateWriteBack {
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

/// 一次可信 Lua Host 成功执行后可交给阶段服务消费的结果。
///
/// 只有 Extract 可以产生托管标准快照意图；未声明意图的 Extract 以及
/// Translate/WriteBack 都产生 `Empty`，避免把 Extract 状态伪装成全阶段可选字段。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TrustedLuaExecutionOutcome {
    Empty,
    ExtractIntent(TrustedLuaExtractIntent),
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
        semantics: Arc<dyn TrustedLuaTranslationSemantics>,
    },
    WriteBack {
        script_path: PathBuf,
        project: LuaProjectContext,
        calls: Arc<dyn TrustedLuaWriteBackHostCalls>,
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
        semantics: Arc<dyn TrustedLuaTranslationSemantics>,
    ) -> Self {
        Self::Translate {
            script_path,
            project,
            profile,
            semantics,
        }
    }

    pub(crate) fn write_back(
        script_path: PathBuf,
        project: LuaProjectContext,
        calls: Arc<dyn TrustedLuaWriteBackHostCalls>,
    ) -> Self {
        Self::WriteBack {
            script_path,
            project,
            calls,
        }
    }
}

/// 完整拥有可信 Lua 程序生命周期与项目能力桥接的 Host。
///
/// Lua 是用户明确选择并完全信任的本机程序，不建立安全沙箱。Host 负责加载脚本、
/// 建立 VM、打开同一项目数据库，向全部阶段注入冻结来源 `ctx.source`、MZ 结构化
/// 只读门面 `ctx.mz` 与 `ctx.db`，在 Translate 阶段根据拥有的同一 `Arc` Profile 注入
/// `ctx.llm`，以及执行和关闭全部资源。Host 不把原始数据库连接、凭据或 Profile 暴露
/// 给脚本。
///
/// Lua 通过 `ctx.extract` 明确采用标准资产契约时，Host 只收集已校验的完整意图，
/// 并在 VM 与数据库会话都干净结束后交给 Extract 服务提交；Lua 通过开放 SQL 建立的
/// 自有数据仍由脚本拥有。Host 不会把整个脚本隐式包进长事务。Runtime 为每次脚本
/// 建立专用 OS 线程以隔离阻塞；公开 Future 不得阻塞异步执行器线程。Host 先打开
/// SQLite 会话，再通过 `start` 同步移交调用面与唯一终结器。启动期或运行期取消都必须回滚
/// 仍未提交的活动事务并关闭数据库连接和 VM；已经显式提交的结果不由 Host 回滚。
/// 成功返回的 Extract 意图进一步承诺 VM 正常结束、唯一终结器成功、没有未闭合事务，
/// 因而调用方可以在会话关闭后另起短事务提交托管标准快照。
pub(crate) trait TrustedLuaExecutionHost: Send + Sync {
    /// 与应用配置边界所选结果一致的执行配置。
    type TranslationProfile: Send + Sync + 'static;
    /// Host 接管或执行失败。
    type Error: Error + Send + Sync + 'static;

    fn execute(
        &self,
        invocation: LuaInvocation<Self::TranslationProfile>,
    ) -> impl Future<Output = Result<OperationCompletion<TrustedLuaExecutionOutcome>, Self::Error>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_paths_are_confined_to_data_and_js() {
        for valid in [
            "data",
            "js",
            "data/Items.json",
            "data/maps/副本.json",
            "js/plugins.js",
        ] {
            assert_eq!(LuaSourcePath::parse(valid).unwrap().as_str(), valid);
        }

        for invalid in [
            "",
            ".",
            "other/file",
            "../data/Items.json",
            "data/../js/plugins.js",
            "C:/game/data/Items.json",
            r"\\server\share\data\Items.json",
            "data/file:stream",
        ] {
            assert!(LuaSourcePath::parse(invalid).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn directory_children_preserve_the_validated_relative_identity() {
        let root = LuaSourcePath::parse("data/maps").unwrap();
        assert_eq!(
            root.child("Map001.json").unwrap().as_str(),
            "data/maps/Map001.json"
        );
        for invalid in ["", ".", "..", "a/b", r"a\b", "a:stream"] {
            assert!(root.child(invalid).is_err(), "{invalid:?}");
        }
    }
}
