//! 目录树内容身份与逻辑根范围契约。

use super::path_index::overlapping_later_paths;
use crate::fingerprint::Sha256Fingerprint;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::{Component, Path, PathBuf};

/// 一棵物理目录树在指纹中的稳定逻辑根。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectoryTreeRoot {
    physical_root: PathBuf,
    logical_root: PathBuf,
}

impl DirectoryTreeRoot {
    pub(crate) fn new(
        physical_root: PathBuf,
        logical_root: PathBuf,
    ) -> Result<Self, DirectoryTreeFingerprintRequestError> {
        if physical_root.as_os_str().is_empty() {
            return Err(DirectoryTreeFingerprintRequestError::EmptyPhysicalRoot);
        }
        validate_tree_logical_root(&logical_root)?;
        Ok(Self {
            physical_root,
            logical_root,
        })
    }

    pub(crate) fn physical_root(&self) -> &Path {
        &self.physical_root
    }

    pub(crate) fn logical_root(&self) -> &Path {
        &self.logical_root
    }
}

/// 多棵目录树共同构成一个精确内容身份的受检请求。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DirectoryTreeFingerprintRequest {
    roots: Vec<DirectoryTreeRoot>,
}

impl DirectoryTreeFingerprintRequest {
    pub(crate) fn new(
        roots: Vec<DirectoryTreeRoot>,
    ) -> Result<Self, DirectoryTreeFingerprintRequestError> {
        if roots.is_empty() {
            return Err(DirectoryTreeFingerprintRequestError::EmptyRoots);
        }
        let logical_roots = roots
            .iter()
            .map(DirectoryTreeRoot::logical_root)
            .collect::<Vec<_>>();
        if let Some((first, second)) = overlapping_later_paths(&logical_roots)
            .iter()
            .enumerate()
            .find_map(|(first, second)| second.map(|second| (first, second)))
        {
            return Err(
                DirectoryTreeFingerprintRequestError::OverlappingLogicalRoots {
                    first: roots[first].logical_root().to_path_buf(),
                    second: roots[second].logical_root().to_path_buf(),
                },
            );
        }
        Ok(Self { roots })
    }

    pub(crate) fn roots(&self) -> &[DirectoryTreeRoot] {
        &self.roots
    }
}

fn validate_tree_logical_root(path: &Path) -> Result<(), DirectoryTreeFingerprintRequestError> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(DirectoryTreeFingerprintRequestError::InvalidLogicalRoot {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DirectoryTreeFingerprintRequestError {
    EmptyRoots,
    EmptyPhysicalRoot,
    InvalidLogicalRoot { path: PathBuf },
    OverlappingLogicalRoots { first: PathBuf, second: PathBuf },
}

impl fmt::Display for DirectoryTreeFingerprintRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyRoots => formatter.write_str("目录树指纹至少需要一个逻辑根"),
            Self::EmptyPhysicalRoot => formatter.write_str("目录树物理根不能为空"),
            Self::InvalidLogicalRoot { path } => {
                write!(
                    formatter,
                    "目录树逻辑根必须是安全相对路径：{}",
                    path.display()
                )
            }
            Self::OverlappingLogicalRoots { first, second } => write!(
                formatter,
                "目录树逻辑根不能重叠：{} 与 {}",
                first.display(),
                second.display()
            ),
        }
    }
}

impl Error for DirectoryTreeFingerprintRequestError {}

#[derive(Debug)]
pub(crate) enum DirectoryTreeFingerprintError<E> {
    NotFound { path: PathBuf },
    NotDirectory { path: PathBuf },
    ChangedDuringObservation { path: PathBuf },
    Failed { path: PathBuf, source: E },
}

impl<E: fmt::Display> fmt::Display for DirectoryTreeFingerprintError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { path } => write!(formatter, "目录树不存在：{}", path.display()),
            Self::NotDirectory { path } => {
                write!(formatter, "目录树根不是目录：{}", path.display())
            }
            Self::ChangedDuringObservation { path } => write!(
                formatter,
                "目录树在建立指纹期间发生变化：{}",
                path.display()
            ),
            Self::Failed { path, source } => {
                write!(formatter, "无法建立目录树指纹 {}：{source}", path.display())
            }
        }
    }
}

impl<E: Error + 'static> Error for DirectoryTreeFingerprintError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Failed { source, .. } => Some(source),
            Self::NotFound { .. }
            | Self::NotDirectory { .. }
            | Self::ChangedDuringObservation { .. } => None,
        }
    }
}

/// 对一个或多个逻辑根建立与绝对路径无关的精确 SHA-256 内容指纹。
pub(crate) trait DirectoryTreeFingerprinter: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn fingerprint_directory_tree(
        &self,
        request: DirectoryTreeFingerprintRequest,
    ) -> impl Future<Output = Result<Sha256Fingerprint, DirectoryTreeFingerprintError<Self::Error>>> + Send;
}

#[cfg(test)]
mod tests;
