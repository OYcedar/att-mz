//! 进程内一次业务运行的合作式取消状态。

use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// 由进程边界发出、由业务阶段边界观察的单向取消事实。
///
/// 取消一旦请求便不会恢复。它只阻止尚未派生的工作；已经被根接管的副作用仍由
/// 相应根能力运行到明确终态。
#[derive(Clone, Default)]
pub(crate) struct CooperativeCancellation {
    requested: Arc<AtomicBool>,
}

impl CooperativeCancellation {
    pub(crate) fn request(&self) {
        self.requested.store(true, Ordering::Release);
    }

    pub(crate) fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    pub(crate) fn check(&self) -> Result<(), OperationCancelled> {
        if self.is_requested() {
            Err(OperationCancelled)
        } else {
            Ok(())
        }
    }
}

/// 当前运行已收到合作式取消请求。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OperationCancelled;

impl fmt::Display for OperationCancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("运行已收到合作式取消请求")
    }
}

impl Error for OperationCancelled {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_is_shared_and_monotonic() {
        let first = CooperativeCancellation::default();
        let second = first.clone();
        assert!(first.check().is_ok());

        second.request();

        assert_eq!(first.check(), Err(OperationCancelled));
        assert!(second.is_requested());
    }
}
