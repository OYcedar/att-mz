//! 具有自然身份的独占文件租约契约。

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};

/// 在指定文件身份上取得跨进程排他租约的受检请求。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExclusiveFileLeaseRequest {
    lock_directory: PathBuf,
    identity: OsString,
}

impl ExclusiveFileLeaseRequest {
    pub(crate) fn new(
        lock_directory: PathBuf,
        identity: OsString,
    ) -> Result<Self, ExclusiveFileLeaseRequestError> {
        if lock_directory.as_os_str().is_empty() {
            return Err(ExclusiveFileLeaseRequestError::EmptyLockDirectory);
        }
        if identity.is_empty() {
            return Err(ExclusiveFileLeaseRequestError::EmptyIdentity);
        }
        Ok(Self {
            lock_directory,
            identity,
        })
    }

    pub(crate) fn lock_directory(&self) -> &Path {
        &self.lock_directory
    }

    pub(crate) fn identity(&self) -> &OsStr {
        &self.identity
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ExclusiveFileLeaseRequestError {
    EmptyLockDirectory,
    EmptyIdentity,
}

impl fmt::Display for ExclusiveFileLeaseRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyLockDirectory => formatter.write_str("排他文件租约目录不能为空"),
            Self::EmptyIdentity => formatter.write_str("排他文件租约身份不能为空"),
        }
    }
}

impl Error for ExclusiveFileLeaseRequestError {}

/// 持有一个跨进程排他文件租约直到本值被丢弃。
#[must_use = "排他文件租约必须存活到需要串行化的操作结束"]
pub(crate) struct ExclusiveFileLease<T> {
    _state: T,
}

impl<T> ExclusiveFileLease<T> {
    pub(crate) const fn new(state: T) -> Self {
        Self { _state: state }
    }
}

#[derive(Debug)]
pub(crate) enum ExclusiveFileLeaseError<E> {
    Unavailable { identity: OsString, source: E },
}

impl<E: fmt::Display> fmt::Display for ExclusiveFileLeaseError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { identity, source } => write!(
                formatter,
                "无法取得排他文件租约 {}：{source}",
                identity.to_string_lossy()
            ),
        }
    }
}

impl<E: Error + 'static> Error for ExclusiveFileLeaseError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Unavailable { source, .. } => Some(source),
        }
    }
}

/// 为一个调用方声明的文件身份提供跨进程排他租约。
pub(crate) trait ExclusiveFileLeaseProvider: Send + Sync {
    type Error: Error + Send + Sync + 'static;
    type LeaseState: Send + 'static;

    fn acquire_exclusive_file_lease(
        &self,
        request: ExclusiveFileLeaseRequest,
    ) -> impl Future<
        Output = Result<ExclusiveFileLease<Self::LeaseState>, ExclusiveFileLeaseError<Self::Error>>,
    > + Send;
}

#[cfg(test)]
mod tests;
