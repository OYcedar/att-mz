//! 候选内声明范围的编辑句柄与失败契约。

use super::ScopedDirectoryPath;
use super::candidate::StagedDirectory;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::future::Future;
use std::path::{Component, Path, PathBuf};

/// 调用方为一个目录候选声明的可编辑顶层目录集合。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScopedDirectoryScope {
    roots: Vec<OsString>,
}

impl ScopedDirectoryScope {
    pub(crate) fn new(
        roots: impl IntoIterator<Item = OsString>,
    ) -> Result<Self, ScopedDirectoryScopeError> {
        let mut roots = roots.into_iter().collect::<Vec<_>>();
        if roots.is_empty() {
            return Err(ScopedDirectoryScopeError::Empty);
        }
        for root in &roots {
            let path = Path::new(root);
            let mut components = path.components();
            if !matches!(components.next(), Some(Component::Normal(name)) if !name.to_string_lossy().contains(':'))
                || components.next().is_some()
            {
                return Err(ScopedDirectoryScopeError::InvalidRoot { root: root.clone() });
            }
        }
        roots.sort();
        for pair in roots.windows(2) {
            if pair[0] == pair[1] {
                return Err(ScopedDirectoryScopeError::DuplicateRoot {
                    root: pair[0].clone(),
                });
            }
        }
        Ok(Self { roots })
    }

    pub(crate) fn roots(&self) -> &[OsString] {
        &self.roots
    }

    pub(crate) fn contains(&self, path: &ScopedDirectoryPath) -> bool {
        self.roots
            .binary_search_by(|root| root.as_os_str().cmp(path.first_component()))
            .is_ok()
    }

    pub(crate) fn is_scope_root(&self, path: &ScopedDirectoryPath) -> bool {
        path.is_top_level() && self.contains(path)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ScopedDirectoryScopeError {
    Empty,
    InvalidRoot { root: OsString },
    DuplicateRoot { root: OsString },
}

impl fmt::Display for ScopedDirectoryScopeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("候选编辑范围必须至少声明一个顶层目录"),
            Self::InvalidRoot { root } => write!(
                formatter,
                "候选编辑范围必须使用单个安全相对路径段：{}",
                root.to_string_lossy()
            ),
            Self::DuplicateRoot { root } => write!(
                formatter,
                "候选编辑范围重复声明顶层目录：{}",
                root.to_string_lossy()
            ),
        }
    }
}

impl Error for ScopedDirectoryScopeError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScopedDirectoryEntryKind {
    File,
    Directory,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScopedDirectoryEntry {
    name: OsString,
    kind: ScopedDirectoryEntryKind,
}

impl ScopedDirectoryEntry {
    pub(crate) fn new(name: OsString, kind: ScopedDirectoryEntryKind) -> Self {
        Self { name, kind }
    }

    pub(crate) fn name(&self) -> &OsStr {
        &self.name
    }

    pub(crate) const fn kind(&self) -> ScopedDirectoryEntryKind {
        self.kind
    }
}

/// 与一个仍未发布候选的物理根身份绑定的编辑令牌。
#[derive(Debug)]
pub(crate) struct BoundScopedDirectory<T> {
    root: PathBuf,
    scope: ScopedDirectoryScope,
    state: T,
}

impl<T> BoundScopedDirectory<T> {
    pub(crate) fn new(root: PathBuf, scope: ScopedDirectoryScope, state: T) -> Self {
        Self { root, scope, state }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn state(&self) -> &T {
        &self.state
    }

    pub(crate) fn scope(&self) -> &ScopedDirectoryScope {
        &self.scope
    }
}

#[derive(Debug)]
pub(crate) enum ScopedDirectoryBindError<E> {
    WrongEditorInstance,
    CandidateFinalized { root: PathBuf },
    CandidateIdentityChanged { root: PathBuf },
    Failed { root: PathBuf, source: E },
}

impl<E: fmt::Display> fmt::Display for ScopedDirectoryBindError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongEditorInstance => {
                formatter.write_str("目录候选不能绑定到另一个文件系统根实例")
            }
            Self::CandidateFinalized { root } => {
                write!(formatter, "目录候选已经终结：{}", root.display())
            }
            Self::CandidateIdentityChanged { root } => {
                write!(formatter, "目录候选物理身份已经变化：{}", root.display())
            }
            Self::Failed { root, source } => {
                write!(formatter, "无法绑定目录候选 {}：{source}", root.display())
            }
        }
    }
}

impl<E: Error + 'static> Error for ScopedDirectoryBindError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Failed { source, .. } => Some(source),
            Self::WrongEditorInstance
            | Self::CandidateFinalized { .. }
            | Self::CandidateIdentityChanged { .. } => None,
        }
    }
}

#[derive(Debug)]
pub(crate) enum ScopedDirectoryEditError<E> {
    WrongEditorInstance,
    OutsideScope { path: PathBuf },
    ScopeRootMutation { path: PathBuf },
    NotFound { path: PathBuf },
    NotFile { path: PathBuf },
    NotDirectory { path: PathBuf },
    CandidateIdentityChanged { root: PathBuf },
    Failed { path: PathBuf, source: E },
}

impl<E: fmt::Display> fmt::Display for ScopedDirectoryEditError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongEditorInstance => {
                formatter.write_str("候选编辑令牌不能交给另一个文件系统根实例")
            }
            Self::OutsideScope { path } => {
                write!(
                    formatter,
                    "候选路径不在调用方声明的编辑范围内：{}",
                    path.display()
                )
            }
            Self::ScopeRootMutation { path } => {
                write!(formatter, "不能修改候选编辑子树根：{}", path.display())
            }
            Self::NotFound { path } => write!(formatter, "候选路径不存在：{}", path.display()),
            Self::NotFile { path } => write!(formatter, "候选路径不是文件：{}", path.display()),
            Self::NotDirectory { path } => {
                write!(formatter, "候选路径不是目录：{}", path.display())
            }
            Self::CandidateIdentityChanged { root } => {
                write!(formatter, "目录候选物理身份已经变化：{}", root.display())
            }
            Self::Failed { path, source } => {
                write!(formatter, "候选目录操作失败 {}：{source}", path.display())
            }
        }
    }
}

impl<E: Error + 'static> Error for ScopedDirectoryEditError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Failed { source, .. } => Some(source),
            Self::WrongEditorInstance
            | Self::OutsideScope { .. }
            | Self::ScopeRootMutation { .. }
            | Self::NotFound { .. }
            | Self::NotFile { .. }
            | Self::NotDirectory { .. }
            | Self::CandidateIdentityChanged { .. } => None,
        }
    }
}

/// 在一个未发布目录候选的调用方声明子树中执行受限文件操作。
///
/// `bind_scoped_directory` 只绑定候选根物理身份并验证声明范围根，不得为绑定重复枚举
/// 完整候选树。后续操作只重验当前目标、祖先和根身份，必须拒绝 reparse point 与
/// 硬链接；调用返回前该次操作已经终结。完整候选树只由发布根在整体交换前验证一次。
pub(crate) trait ScopedDirectoryEditor: Send + Sync {
    type CandidateState: Send + 'static;
    type ScopeState: Send + Sync + 'static;
    type Error: Error + Send + Sync + 'static;

    fn bind_scoped_directory(
        &self,
        candidate: &StagedDirectory<Self::CandidateState>,
        scope: ScopedDirectoryScope,
    ) -> impl Future<
        Output = Result<
            BoundScopedDirectory<Self::ScopeState>,
            ScopedDirectoryBindError<Self::Error>,
        >,
    > + Send
    + use<Self>;

    fn list_scoped_directory(
        &self,
        scope: &BoundScopedDirectory<Self::ScopeState>,
        path: ScopedDirectoryPath,
    ) -> impl Future<
        Output = Result<Vec<ScopedDirectoryEntry>, ScopedDirectoryEditError<Self::Error>>,
    > + Send;

    /// 列举候选根的全部直接子项；调用方据此拥有自身的顶层结构语义。
    fn list_scoped_root(
        &self,
        scope: &BoundScopedDirectory<Self::ScopeState>,
    ) -> impl Future<
        Output = Result<Vec<ScopedDirectoryEntry>, ScopedDirectoryEditError<Self::Error>>,
    > + Send;

    fn create_scoped_directory(
        &self,
        scope: &BoundScopedDirectory<Self::ScopeState>,
        path: ScopedDirectoryPath,
    ) -> impl Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send;

    fn write_scoped_file(
        &self,
        scope: &BoundScopedDirectory<Self::ScopeState>,
        path: ScopedDirectoryPath,
        bytes: Vec<u8>,
    ) -> impl Future<Output = Result<(), ScopedDirectoryEditError<Self::Error>>> + Send;
}

#[cfg(test)]
mod tests;
