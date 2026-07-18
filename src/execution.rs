//! 进程内业务执行的共享契约，包括取消状态、正常终态与受控 CPU 计算。

pub(crate) mod cpu;

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
}

/// 一次业务操作已经完成，或在继续派生工作前正常响应了合作式取消。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum OperationCompletion<T> {
    Completed(T),
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_is_shared_and_monotonic() {
        let first = CooperativeCancellation::default();
        let second = first.clone();
        assert!(!first.is_requested());

        second.request();

        assert!(first.is_requested());
        assert!(second.is_requested());
    }

    #[test]
    fn completion_expresses_cancellation_as_data() {
        let completion: OperationCompletion<usize> = OperationCompletion::Cancelled;
        assert_eq!(completion, OperationCompletion::Cancelled);
    }
}
