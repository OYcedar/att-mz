//! Tokio 时间驱动上的可取消延迟根。

use std::time::Duration;

use crate::att_mz::translate::executor::AsyncDelay;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TokioAsyncDelay;

impl AsyncDelay for TokioAsyncDelay {
    async fn wait(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[tokio::test]
    async fn waits_for_requested_duration() {
        let started = Instant::now();
        TokioAsyncDelay.wait(Duration::from_millis(5)).await;
        assert!(started.elapsed() >= Duration::from_millis(5));
    }
}
