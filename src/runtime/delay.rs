//! Tokio 时间驱动上的可取消延迟根。

use std::convert::Infallible;
use std::time::Duration;

use crate::att_mz::translate::executor::AsyncDelay;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TokioAsyncDelay;

impl AsyncDelay for TokioAsyncDelay {
    type Error = Infallible;

    async fn wait(&self, duration: Duration) -> Result<(), Self::Error> {
        tokio::time::sleep(duration).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[tokio::test]
    async fn waits_for_requested_duration() {
        let started = Instant::now();
        TokioAsyncDelay
            .wait(Duration::from_millis(5))
            .await
            .expect("延迟根不会失败");
        assert!(started.elapsed() >= Duration::from_millis(5));
    }
}
