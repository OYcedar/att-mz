//! RPG Maker MV/MZ 各业务阶段共享的可信 Lua 调用边界。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use self::runtime::OwnedLuaProgram;
use self::runtime::TrustedLuaExtractIntent;
use self::runtime::TrustedLuaTranslationSemantics;
use self::runtime::TrustedLuaWriteBackHostCalls;
use crate::execution::OperationCompletion;
use crate::language::{LanguageId, LanguagePair};
use crate::rpg_maker::RpgMakerEngine;

pub(crate) mod document;
pub(crate) mod hosting;
pub(crate) mod json;
pub(crate) mod lua54;
pub(crate) mod runtime;

/// Lua 只读来源门面能够访问的项目内相对路径。
///
/// 该类型只表达逻辑边界；普通文件、目录与 reparse point 的现实观测仍由文件系统
/// 根完成。进入 Host 后路径一定从 `data` 或 `js` 开始，且无法逃出冻结来源根。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct LuaSourcePath {
    display: String,
}

impl LuaSourcePath {
    pub(crate) fn parse(value: &str) -> Result<Self, LuaSourcePathError> {
        if value.is_empty() {
            return Err(LuaSourcePathError::Empty);
        }
        if value.starts_with('/') {
            return Err(LuaSourcePathError::Absolute);
        }
        if value.contains('\\')
            || value.ends_with('/')
            || value.contains("//")
            || value.chars().any(char::is_control)
        {
            return Err(LuaSourcePathError::NonCanonical);
        }
        if value.contains(':') {
            return Err(LuaSourcePathError::AlternateDataStream);
        }

        let mut display_components = Vec::new();
        for component in value.split('/') {
            if component == ".." {
                return Err(LuaSourcePathError::ParentTraversal);
            }
            if component.is_empty() || component == "." {
                return Err(LuaSourcePathError::NonCanonical);
            }
            display_components.push(component.to_owned());
        }

        let Some(root) = display_components.first() else {
            return Err(LuaSourcePathError::Empty);
        };
        if root != "data" && root != "js" {
            return Err(LuaSourcePathError::OutsideSourceRoots);
        }

        Ok(Self {
            display: display_components.join("/"),
        })
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.display
    }

    pub(crate) fn components(&self) -> impl Iterator<Item = &str> {
        self.display.split('/')
    }

    pub(crate) fn child(&self, name: &str) -> Result<Self, LuaSourcePathError> {
        if name.is_empty()
            || name
                .chars()
                .any(|character| character.is_control() || matches!(character, '/' | '\\' | ':'))
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
            Self::InvalidChildName => formatter.write_str("目录项名称不是有效来源路径段"),
        }
    }
}

impl Error for LuaSourcePathError {}

/// 可信 Lua 在当前阶段能够访问的项目文件边界。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LuaProjectFileAccess {
    /// Extract 与 Translate 只接收 Init 按当前引擎布局冻结的 `data`、`js`。
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
    name: String,
    engine: RpgMakerEngine,
    file_access: LuaProjectFileAccess,
    database_path: PathBuf,
    language_pair: LanguagePair,
}

impl LuaProjectContext {
    pub(crate) fn for_frozen_source(
        name: impl Into<String>,
        engine: RpgMakerEngine,
        source_root: PathBuf,
        database_path: PathBuf,
        language_pair: LanguagePair,
    ) -> Self {
        Self {
            name: name.into(),
            engine,
            file_access: LuaProjectFileAccess::FrozenSource { source_root },
            database_path,
            language_pair,
        }
    }

    pub(crate) fn for_write_back_candidate(
        name: impl Into<String>,
        engine: RpgMakerEngine,
        source_root: PathBuf,
        database_path: PathBuf,
        language_pair: LanguagePair,
        output_root: PathBuf,
    ) -> Self {
        Self {
            name: name.into(),
            engine,
            file_access: LuaProjectFileAccess::CandidateWriteBack {
                source_root,
                output_root,
            },
            database_path,
            language_pair,
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) const fn engine(&self) -> RpgMakerEngine {
        self.engine
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

    pub(crate) fn source_language(&self) -> &LanguageId {
        self.language_pair.source()
    }

    pub(crate) fn target_language(&self) -> &LanguageId {
        self.language_pair.target()
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
/// 枚举变体从类型上保证 Extract 不携带无关 LLM Client，而 Translate 一定拥有配置
/// 边界已经选择的 Client。调用拥有全部事实，便于专用 worker 在调用 Future 被取消
/// 后继续完成受控清理。
pub(crate) enum LuaInvocation<C> {
    Extract {
        program: OwnedLuaProgram,
        project: LuaProjectContext,
    },
    Translate {
        program: OwnedLuaProgram,
        project: LuaProjectContext,
        llm_client: Arc<C>,
        semantics: Arc<dyn TrustedLuaTranslationSemantics>,
    },
    WriteBack {
        program: OwnedLuaProgram,
        project: LuaProjectContext,
        calls: Arc<dyn TrustedLuaWriteBackHostCalls>,
    },
}

impl<C> LuaInvocation<C> {
    pub(crate) fn extract(program: OwnedLuaProgram, project: LuaProjectContext) -> Self {
        Self::Extract { program, project }
    }

    pub(crate) fn translate(
        program: OwnedLuaProgram,
        project: LuaProjectContext,
        llm_client: Arc<C>,
        semantics: Arc<dyn TrustedLuaTranslationSemantics>,
    ) -> Self {
        Self::Translate {
            program,
            project,
            llm_client,
            semantics,
        }
    }

    pub(crate) fn write_back(
        program: OwnedLuaProgram,
        project: LuaProjectContext,
        calls: Arc<dyn TrustedLuaWriteBackHostCalls>,
    ) -> Self {
        Self::WriteBack {
            program,
            project,
            calls,
        }
    }
}

/// 完整拥有可信 Lua 程序生命周期与项目能力桥接的 Host。
///
/// Lua 是用户明确选择并完全信任的本机程序，不建立安全沙箱。Host 接收已经冻结的
/// 主程序快照、建立 VM、打开同一项目数据库，向全部阶段注入冻结来源 `ctx.source`、
/// RPG Maker 结构化只读门面 `ctx.rpg_maker` 与 `ctx.db`，在 Translate 阶段根据拥有的 Client
/// 注入 `ctx.llm`，
/// 以及执行和关闭全部资源。Host 不把原始数据库连接或凭据暴露
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
    /// 与应用配置边界所选结果一致的 LLM Client。
    type TranslationClient: Send + Sync + 'static;
    /// Host 接管或执行失败。
    type Error: Error + Send + Sync + 'static;

    fn execute(
        &self,
        invocation: LuaInvocation<Self::TranslationClient>,
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
            "Data/Items.json",
            "JS/plugins.js",
            "other/file",
            "../data/Items.json",
            "data/../js/plugins.js",
            "C:/game/data/Items.json",
            r"\\server\share\data\Items.json",
            "data/file:stream",
            r"data\Items.json",
            "data//Items.json",
            "data/Items.json/",
            "data/./Items.json",
            "data/line\nfeed",
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
        for invalid in ["", ".", "..", "a/b", r"a\b", "a:stream", "line\nfeed"] {
            assert!(root.child(invalid).is_err(), "{invalid:?}");
        }
    }
}
