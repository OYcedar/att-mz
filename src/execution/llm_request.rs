//! 引擎无关的 LLM 请求与网络重试状态机。
//!
//! 根请求适配器只执行一次请求；本模块统一拥有重试预算、Retry-After、可取消等待、
//! attempt 编号与旁路证据。业务切片只把这里的明确终态转换成自己的 unavailable
//! 或技术错误语义。

use std::future::Future;
use std::num::NonZeroUsize;
use std::time::Duration;

use crate::diagnostic::{DiagnosticReport, StateEffect};
use crate::llm::{
    ChatMessage, LlmRequestError, LlmRequestExecutor, LlmRequestFailure, LlmResponse,
    LlmServiceStatus,
};

use super::CooperativeCancellation;

/// 可取消异步等待的根能力。
pub(crate) trait AsyncDelay: Send + Sync {
    fn wait(&self, duration: Duration) -> impl Future<Output = ()> + Send;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TokioAsyncDelay;

impl AsyncDelay for TokioAsyncDelay {
    async fn wait(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }
}

/// 一次请求执行所消费的外部重试策略。
#[derive(Clone, Copy, Debug)]
pub(crate) struct LlmRequestRetryPolicy<'a> {
    retry_delays: &'a [Duration],
    max_retry_after: Duration,
}

impl<'a> LlmRequestRetryPolicy<'a> {
    pub(crate) const fn new(retry_delays: &'a [Duration], max_retry_after: Duration) -> Self {
        Self {
            retry_delays,
            max_retry_after,
        }
    }
}

/// 请求状态机建立的实际 attempt 计数。
#[derive(Debug)]
pub(crate) struct LlmRequestAttemptEvidence {
    attempt_count: usize,
}

impl LlmRequestAttemptEvidence {
    pub(crate) const fn attempt_count(&self) -> usize {
        self.attempt_count
    }
}

/// 共享请求状态机的明确终态。
#[derive(Debug)]
pub(crate) enum LlmRequestExecutionOutcome<E> {
    Response {
        response: LlmResponse,
        attempt: NonZeroUsize,
    },
    RetryAfterExceedsMaximum {
        attempt: NonZeroUsize,
        diagnostic: DiagnosticReport,
        retry_after: Duration,
        maximum: Duration,
        service_status: LlmServiceStatus,
    },
    RetryBudgetExhausted {
        attempt: NonZeroUsize,
        diagnostic: DiagnosticReport,
        service_status: LlmServiceStatus,
    },
    Fatal {
        source: E,
        diagnostic: DiagnosticReport,
    },
    AdmissionStopped {
        diagnostic: DiagnosticReport,
    },
    Cancelled,
}

/// 共享请求状态机的终态与一次性证据。
#[derive(Debug)]
pub(crate) struct LlmRequestExecution<E> {
    outcome: LlmRequestExecutionOutcome<E>,
    evidence: LlmRequestAttemptEvidence,
}

impl<E> LlmRequestExecution<E> {
    pub(crate) fn into_parts(self) -> (LlmRequestExecutionOutcome<E>, LlmRequestAttemptEvidence) {
        (self.outcome, self.evidence)
    }
}

struct AttemptEvidenceBuilder {
    attempt_count: usize,
}

impl AttemptEvidenceBuilder {
    const fn new() -> Self {
        Self { attempt_count: 0 }
    }

    fn begin_attempt(&mut self, attempt: NonZeroUsize) {
        self.attempt_count = self.attempt_count.max(attempt.get());
    }

    fn finish(self) -> LlmRequestAttemptEvidence {
        LlmRequestAttemptEvidence {
            attempt_count: self.attempt_count,
        }
    }
}

fn finish<E>(
    outcome: LlmRequestExecutionOutcome<E>,
    evidence: AttemptEvidenceBuilder,
) -> LlmRequestExecution<E> {
    LlmRequestExecution {
        outcome,
        evidence: evidence.finish(),
    }
}

/// 执行一次逻辑 LLM 请求，并在同一状态机内完成有限网络重试。
#[cfg(test)]
pub(crate) async fn execute_llm_request_with_retry<L, D>(
    llm: &L,
    client: &L::Client,
    messages: &[ChatMessage],
    policy: LlmRequestRetryPolicy<'_>,
    delay: &D,
    cancellation: &CooperativeCancellation,
) -> LlmRequestExecution<L::Error>
where
    L: LlmRequestExecutor,
    D: AsyncDelay,
{
    execute_llm_request_with_retry_observed(
        llm,
        client,
        messages,
        policy,
        delay,
        cancellation,
        || {},
    )
    .await
}

/// 执行共享请求状态机，并在第一次真实外部 attempt 开始时报告任务准入。
pub(crate) async fn execute_llm_request_with_retry_observed<L, D, H>(
    llm: &L,
    client: &L::Client,
    messages: &[ChatMessage],
    policy: LlmRequestRetryPolicy<'_>,
    delay: &D,
    cancellation: &CooperativeCancellation,
    on_first_attempt_started: H,
) -> LlmRequestExecution<L::Error>
where
    L: LlmRequestExecutor,
    D: AsyncDelay,
    H: FnOnce() + Send,
{
    execute_llm_request_with_retry_inner(
        llm,
        client,
        messages,
        policy,
        delay,
        cancellation,
        || {},
        on_first_attempt_started,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_llm_request_with_retry_inner<L, D, H, S>(
    llm: &L,
    client: &L::Client,
    messages: &[ChatMessage],
    policy: LlmRequestRetryPolicy<'_>,
    delay: &D,
    cancellation: &CooperativeCancellation,
    after_completed_wait_check: H,
    on_first_attempt_started: S,
) -> LlmRequestExecution<L::Error>
where
    L: LlmRequestExecutor,
    D: AsyncDelay,
    H: Fn(),
    S: FnOnce() + Send,
{
    let mut evidence = AttemptEvidenceBuilder::new();
    let mut attempt = NonZeroUsize::MIN;
    let mut retry_delays = policy.retry_delays.iter().copied();
    let mut on_first_attempt_started = Some(on_first_attempt_started);

    loop {
        if cancellation.is_requested() {
            return finish(LlmRequestExecutionOutcome::Cancelled, evidence);
        }

        let mut attempt_reported = false;
        let result = llm
            .request_with_attempt_observer(
                client,
                messages,
                Box::new(|| {
                    attempt_reported = true;
                    evidence.begin_attempt(attempt);
                    if let Some(on_started) = on_first_attempt_started.take() {
                        on_started();
                    }
                }),
            )
            .await;
        match result {
            Ok(response) => {
                if !attempt_reported {
                    evidence.begin_attempt(attempt);
                    if let Some(on_started) = on_first_attempt_started.take() {
                        on_started();
                    }
                }
                return finish(
                    LlmRequestExecutionOutcome::Response { response, attempt },
                    evidence,
                );
            }
            Err(LlmRequestError::Fatal(source)) => {
                if source.request_was_sent() && !attempt_reported {
                    evidence.begin_attempt(attempt);
                    if let Some(on_started) = on_first_attempt_started.take() {
                        on_started();
                    }
                }
                let cancelled = cancellation.is_requested() && source.is_cancelled_wait();
                if cancelled {
                    return finish(LlmRequestExecutionOutcome::Cancelled, evidence);
                }
                let diagnostic = {
                    DiagnosticReport::new(
                        StateEffect::ProgressPreserved,
                        llm.request_diagnostic(client, &source, None),
                    )
                };
                let service_status = source.service_status();
                if service_status.is_permanent() {
                    llm.stop_admission(service_status, &diagnostic);
                }
                return finish(
                    LlmRequestExecutionOutcome::Fatal { source, diagnostic },
                    evidence,
                );
            }
            Err(LlmRequestError::AdmissionStopped { diagnostic, .. }) => {
                return finish(
                    LlmRequestExecutionOutcome::AdmissionStopped { diagnostic },
                    evidence,
                );
            }
            Err(LlmRequestError::Retryable {
                source,
                retry_after,
            }) => {
                if source.request_was_sent() && !attempt_reported {
                    evidence.begin_attempt(attempt);
                    if let Some(on_started) = on_first_attempt_started.take() {
                        on_started();
                    }
                }
                if cancellation.is_requested() && source.is_cancelled_wait() {
                    return finish(LlmRequestExecutionOutcome::Cancelled, evidence);
                }

                let diagnostic = DiagnosticReport::new(
                    StateEffect::ProgressPreserved,
                    llm.request_diagnostic(client, &source, retry_after),
                );
                let service_status = source.service_status();
                if let Some(retry_after) = retry_after
                    && retry_after > policy.max_retry_after
                {
                    if service_status.stops_admission_after_unavailable() {
                        llm.stop_admission(service_status, &diagnostic);
                    }
                    return finish(
                        LlmRequestExecutionOutcome::RetryAfterExceedsMaximum {
                            attempt,
                            diagnostic,
                            retry_after,
                            maximum: policy.max_retry_after,
                            service_status,
                        },
                        evidence,
                    );
                }

                let Some(configured_delay) = retry_delays.next() else {
                    if service_status.stops_admission_after_unavailable() {
                        llm.stop_admission(service_status, &diagnostic);
                    }
                    return finish(
                        LlmRequestExecutionOutcome::RetryBudgetExhausted {
                            attempt,
                            diagnostic,
                            service_status,
                        },
                        evidence,
                    );
                };

                llm.continue_after_retryable(service_status);
                let retry_delay = configured_delay.max(retry_after.unwrap_or_default());
                let waiting = delay.wait(retry_delay);
                tokio::pin!(waiting);
                let cancelled = cancellation.cancelled();
                tokio::pin!(cancelled);
                tokio::select! {
                    biased;
                    () = &mut cancelled => {
                        return finish(
                            LlmRequestExecutionOutcome::Cancelled,
                            evidence,
                        );
                    }
                    () = &mut waiting => {}
                }
                if cancellation.is_requested() {
                    return finish(LlmRequestExecutionOutcome::Cancelled, evidence);
                }
                // 这条同步观察只供竞态测试在“等待已完成、下一请求尚未准入”的
                // 精确边界注入取消；生产调用传入零成本空闭包。
                after_completed_wait_check();
                if cancellation.is_requested() {
                    return finish(LlmRequestExecutionOutcome::Cancelled, evidence);
                }
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::error::Error;
    use std::fmt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier, Mutex};

    use crate::diagnostic::{Diagnostic, RuntimeComponent, RuntimeIssue, RuntimeOperation};
    use crate::llm::{LlmFinishReason, LlmRequestFailure};

    use super::*;

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeError {}

    impl LlmRequestFailure for FakeError {
        fn is_cancelled_wait(&self) -> bool {
            self.0 == "cancelled-wait"
        }

        fn request_was_sent(&self) -> bool {
            !matches!(self.0, "cancelled-wait" | "not-sent")
        }
    }

    type FakeResponse = Result<LlmResponse, LlmRequestError<FakeError>>;

    #[derive(Clone)]
    struct FakeLlm {
        responses: Arc<Mutex<VecDeque<FakeResponse>>>,
        calls: Arc<Mutex<usize>>,
    }

    impl LlmRequestExecutor for FakeLlm {
        type Client = ();
        type Error = FakeError;

        async fn request<'a>(
            &'a self,
            _client: &'a Self::Client,
            _messages: &'a [ChatMessage],
        ) -> Result<LlmResponse, LlmRequestError<Self::Error>> {
            *self.calls.lock().expect("请求计数锁不应中毒") += 1;
            self.responses
                .lock()
                .expect("响应队列锁不应中毒")
                .pop_front()
                .expect("测试必须准备足够响应")
        }

        async fn request_with_attempt_observer<'a>(
            &'a self,
            client: &'a Self::Client,
            messages: &'a [ChatMessage],
            on_attempt_started: Box<dyn FnOnce() + Send + 'a>,
        ) -> Result<LlmResponse, LlmRequestError<Self::Error>> {
            let result = self.request(client, messages).await;
            let was_sent = match &result {
                Ok(_) | Err(LlmRequestError::Retryable { .. }) => true,
                Err(LlmRequestError::Fatal(source)) => source.request_was_sent(),
                Err(LlmRequestError::AdmissionStopped { .. }) => false,
            };
            if was_sent {
                on_attempt_started();
            }
            result
        }

        fn request_diagnostic(
            &self,
            _client: &Self::Client,
            _source: &Self::Error,
            _retry_after: Option<Duration>,
        ) -> Diagnostic {
            Diagnostic::runtime(RuntimeIssue::ExecutorClosed {
                component: RuntimeComponent::Process,
                operation: RuntimeOperation::ExecuteTask,
            })
        }
    }

    #[derive(Clone, Default)]
    struct ImmediateDelay;

    impl AsyncDelay for ImmediateDelay {
        async fn wait(&self, _duration: Duration) {}
    }

    #[derive(Clone)]
    struct CancellingOnCompletionDelay {
        cancellation: CooperativeCancellation,
    }

    impl AsyncDelay for CancellingOnCompletionDelay {
        async fn wait(&self, _duration: Duration) {
            self.cancellation.request();
        }
    }

    fn retryable() -> Result<LlmResponse, LlmRequestError<FakeError>> {
        Err(LlmRequestError::Retryable {
            source: FakeError("busy"),
            retry_after: None,
        })
    }

    #[tokio::test]
    async fn completed_wait_cancelled_before_next_request_keeps_real_attempt_count() {
        let cancellation = CooperativeCancellation::default();
        let calls = Arc::new(Mutex::new(0));
        let llm = FakeLlm {
            responses: Arc::new(Mutex::new(VecDeque::from([
                retryable(),
                Ok(LlmResponse::new("{}", LlmFinishReason::Stop)),
            ]))),
            calls: Arc::clone(&calls),
        };

        let execution = execute_llm_request_with_retry(
            &llm,
            &(),
            &[],
            LlmRequestRetryPolicy::new(&[Duration::from_millis(1)], Duration::from_secs(1)),
            &CancellingOnCompletionDelay {
                cancellation: cancellation.clone(),
            },
            &cancellation,
        )
        .await;
        let (outcome, evidence) = execution.into_parts();

        assert!(matches!(outcome, LlmRequestExecutionOutcome::Cancelled));
        assert_eq!(*calls.lock().expect("请求计数锁不应中毒"), 1);
        assert_eq!(
            evidence.attempt_count(),
            1,
            "未开始的下一请求不得虚增 attempt"
        );
    }

    #[tokio::test]
    async fn cancellation_between_wait_checkpoint_and_next_loop_does_not_create_attempt() {
        let cancellation = CooperativeCancellation::default();
        let calls = Arc::new(Mutex::new(0));
        let llm = FakeLlm {
            responses: Arc::new(Mutex::new(VecDeque::from([
                retryable(),
                Ok(LlmResponse::new("{}", LlmFinishReason::Stop)),
            ]))),
            calls: Arc::clone(&calls),
        };
        let boundary = Arc::new(Barrier::new(2));
        let cancelling_boundary = Arc::clone(&boundary);
        let cancelling_token = cancellation.clone();
        let canceller = std::thread::spawn(move || {
            cancelling_boundary.wait();
            cancelling_token.request();
        });
        let observed_boundary = Arc::clone(&boundary);
        let observed_cancellation = cancellation.clone();

        let execution = execute_llm_request_with_retry_inner(
            &llm,
            &(),
            &[],
            LlmRequestRetryPolicy::new(&[Duration::ZERO], Duration::from_secs(1)),
            &ImmediateDelay,
            &cancellation,
            move || {
                observed_boundary.wait();
                while !observed_cancellation.is_requested() {
                    std::thread::yield_now();
                }
            },
            || {},
        )
        .await;
        canceller.join().expect("取消线程不应 panic");
        let (outcome, evidence) = execution.into_parts();

        assert!(matches!(outcome, LlmRequestExecutionOutcome::Cancelled));
        assert_eq!(*calls.lock().expect("请求计数锁不应中毒"), 1);
        assert_eq!(evidence.attempt_count(), 1);
    }

    #[tokio::test]
    async fn successful_retry_marks_wait_as_retried_and_reuses_messages() {
        let calls = Arc::new(Mutex::new(0));
        let llm = FakeLlm {
            responses: Arc::new(Mutex::new(VecDeque::from([
                retryable(),
                Ok(LlmResponse::new("{}", LlmFinishReason::Stop)),
            ]))),
            calls: Arc::clone(&calls),
        };
        let cancellation = CooperativeCancellation::default();

        let execution = execute_llm_request_with_retry(
            &llm,
            &(),
            &[],
            LlmRequestRetryPolicy::new(&[Duration::ZERO], Duration::from_secs(1)),
            &ImmediateDelay,
            &cancellation,
        )
        .await;
        let (outcome, evidence) = execution.into_parts();

        assert!(matches!(
            outcome,
            LlmRequestExecutionOutcome::Response { attempt, .. }
                if attempt.get() == 2
        ));
        assert_eq!(*calls.lock().expect("请求计数锁不应中毒"), 2);
        assert_eq!(evidence.attempt_count(), 2);
    }

    #[tokio::test]
    async fn admission_stop_without_http_has_zero_attempts() {
        let started = Arc::new(AtomicBool::new(false));
        let llm = FakeLlm {
            responses: Arc::new(Mutex::new(VecDeque::from([Err(
                LlmRequestError::AdmissionStopped {
                    diagnostic: DiagnosticReport::new(
                        StateEffect::ProgressPreserved,
                        Diagnostic::runtime(RuntimeIssue::ExecutorClosed {
                            component: RuntimeComponent::Process,
                            operation: RuntimeOperation::ExecuteTask,
                        }),
                    ),
                },
            )]))),
            calls: Arc::new(Mutex::new(0)),
        };

        let observed_started = Arc::clone(&started);
        let execution = execute_llm_request_with_retry_observed(
            &llm,
            &(),
            &[],
            LlmRequestRetryPolicy::new(&[], Duration::from_secs(1)),
            &ImmediateDelay,
            &CooperativeCancellation::default(),
            move || observed_started.store(true, Ordering::Release),
        )
        .await;
        let (outcome, evidence) = execution.into_parts();

        assert!(matches!(
            outcome,
            LlmRequestExecutionOutcome::AdmissionStopped { .. }
        ));
        assert_eq!(evidence.attempt_count(), 0);
        assert!(!started.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn failure_before_http_has_zero_attempts() {
        let llm = FakeLlm {
            responses: Arc::new(Mutex::new(VecDeque::from([Err(LlmRequestError::Fatal(
                FakeError("not-sent"),
            ))]))),
            calls: Arc::new(Mutex::new(0)),
        };

        let execution = execute_llm_request_with_retry(
            &llm,
            &(),
            &[],
            LlmRequestRetryPolicy::new(&[], Duration::from_secs(1)),
            &ImmediateDelay,
            &CooperativeCancellation::default(),
        )
        .await;
        let (_, evidence) = execution.into_parts();

        assert_eq!(evidence.attempt_count(), 0);
    }
}
