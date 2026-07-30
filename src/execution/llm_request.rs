//! 引擎无关的 LLM 请求与网络重试状态机。
//!
//! 根请求适配器只执行一次请求；本模块统一拥有重试预算、Retry-After、可取消等待、
//! attempt 编号与旁路证据。业务切片只把这里的明确终态转换成自己的 unavailable
//! 或技术错误语义。

use std::future::Future;
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use crate::diagnostic::{DiagnosticImpact, SafeDiagnostic};
use crate::llm::{
    ChatMessage, LlmFinishReason, LlmRequestDiagnosticSource, LlmRequestError, LlmRequestExecutor,
    LlmResponse, LlmUsage,
};

use super::CooperativeCancellation;

/// 可取消异步等待的根能力。
pub(crate) trait AsyncDelay: Send + Sync {
    fn wait(&self, duration: Duration) -> impl Future<Output = ()> + Send;
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

/// 一次逻辑请求尝试的结构化旁路证据。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LlmRequestAttemptRecord {
    pub(crate) attempt: NonZeroUsize,
    pub(crate) duration: Duration,
    pub(crate) outcome: LlmRequestAttemptOutcome,
}

impl LlmRequestAttemptRecord {
    pub(crate) fn succeeded(
        attempt: NonZeroUsize,
        duration: Duration,
        response: &LlmResponse,
    ) -> Self {
        Self {
            attempt,
            duration,
            outcome: LlmRequestAttemptOutcome::Succeeded {
                finish_reason: response.finish_reason().clone(),
                provider_request_id: response.provider_request_id().map(str::to_owned),
                provider_response_id: response.provider_response_id().map(str::to_owned),
                usage: response.usage(),
            },
        }
    }

    pub(crate) fn retryable(
        attempt: NonZeroUsize,
        duration: Duration,
        diagnostic: SafeDiagnostic,
        retry_after: Option<Duration>,
        retry_wait: Option<LlmRequestRetryWaitRecord>,
    ) -> Self {
        Self {
            attempt,
            duration,
            outcome: LlmRequestAttemptOutcome::Retryable {
                diagnostic,
                retry_after,
                retry_wait,
            },
        }
    }

    fn mark_retry_started(&mut self) {
        let outcome = std::mem::replace(&mut self.outcome, LlmRequestAttemptOutcome::Cancelled);
        self.outcome = match outcome {
            LlmRequestAttemptOutcome::Retryable {
                diagnostic,
                retry_after,
                retry_wait: Some(LlmRequestRetryWaitRecord::CompletedBeforeNextAttempt { duration }),
            } => LlmRequestAttemptOutcome::Retryable {
                diagnostic,
                retry_after,
                retry_wait: Some(LlmRequestRetryWaitRecord::Retried { duration }),
            },
            outcome => outcome,
        };
    }

    pub(crate) fn failed(
        attempt: NonZeroUsize,
        duration: Duration,
        diagnostic: SafeDiagnostic,
    ) -> Self {
        Self {
            attempt,
            duration,
            outcome: LlmRequestAttemptOutcome::Failed { diagnostic },
        }
    }

    pub(crate) fn cancelled(attempt: NonZeroUsize, duration: Duration) -> Self {
        Self {
            attempt,
            duration,
            outcome: LlmRequestAttemptOutcome::Cancelled,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LlmRequestAttemptOutcome {
    Succeeded {
        finish_reason: LlmFinishReason,
        provider_request_id: Option<String>,
        provider_response_id: Option<String>,
        usage: Option<LlmUsage>,
    },
    Retryable {
        diagnostic: SafeDiagnostic,
        retry_after: Option<Duration>,
        retry_wait: Option<LlmRequestRetryWaitRecord>,
    },
    Failed {
        diagnostic: SafeDiagnostic,
    },
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LlmRequestRetryWaitRecord {
    Retried { duration: Duration },
    CompletedBeforeNextAttempt { duration: Duration },
    CancelledWhileWaiting { planned_duration: Duration },
}

/// 请求状态机建立的 attempt 计数与可选详细证据。
#[derive(Debug)]
pub(crate) struct LlmRequestAttemptEvidence {
    attempt_count: usize,
    attempts: Vec<LlmRequestAttemptRecord>,
}

impl LlmRequestAttemptEvidence {
    pub(crate) fn into_parts(self) -> (usize, Vec<LlmRequestAttemptRecord>) {
        (self.attempt_count, self.attempts)
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
        diagnostic: SafeDiagnostic,
        retry_after: Duration,
        maximum: Duration,
    },
    RetryBudgetExhausted {
        attempt: NonZeroUsize,
        diagnostic: SafeDiagnostic,
    },
    Fatal {
        attempt: NonZeroUsize,
        source: E,
        diagnostic: Option<SafeDiagnostic>,
        cancelled: bool,
    },
    Cancelled {
        attempt: NonZeroUsize,
    },
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
    recording: bool,
    attempt_count: usize,
    attempts: Vec<LlmRequestAttemptRecord>,
}

impl AttemptEvidenceBuilder {
    fn new(recording: bool) -> Self {
        Self {
            recording,
            attempt_count: 0,
            attempts: Vec::new(),
        }
    }

    fn begin_attempt(&mut self, attempt: NonZeroUsize) -> Option<Instant> {
        self.attempt_count = self.attempt_count.max(attempt.get());
        self.recording.then(Instant::now)
    }

    fn record(&mut self, build: impl FnOnce() -> LlmRequestAttemptRecord) {
        if self.recording {
            self.attempts.push(build());
        }
    }

    fn push(&mut self, record: Option<LlmRequestAttemptRecord>) {
        if let Some(record) = record {
            self.attempts.push(record);
        }
    }

    fn duration(started: Option<Instant>) -> Duration {
        started.map_or(Duration::ZERO, |started| started.elapsed())
    }

    fn finish(self) -> LlmRequestAttemptEvidence {
        LlmRequestAttemptEvidence {
            attempt_count: self.attempt_count,
            attempts: self.attempts,
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
pub(crate) async fn execute_llm_request_with_retry<L, D>(
    llm: &L,
    client: &L::Client,
    messages: &[ChatMessage],
    policy: LlmRequestRetryPolicy<'_>,
    delay: &D,
    cancellation: &CooperativeCancellation,
    record_evidence: bool,
) -> LlmRequestExecution<L::Error>
where
    L: LlmRequestExecutor,
    L::Error: LlmRequestDiagnosticSource,
    D: AsyncDelay,
{
    execute_llm_request_with_retry_inner(
        llm,
        client,
        messages,
        policy,
        delay,
        cancellation,
        record_evidence,
        || {},
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn execute_llm_request_with_retry_inner<L, D, H>(
    llm: &L,
    client: &L::Client,
    messages: &[ChatMessage],
    policy: LlmRequestRetryPolicy<'_>,
    delay: &D,
    cancellation: &CooperativeCancellation,
    record_evidence: bool,
    after_completed_wait_check: H,
) -> LlmRequestExecution<L::Error>
where
    L: LlmRequestExecutor,
    L::Error: LlmRequestDiagnosticSource,
    D: AsyncDelay,
    H: Fn(),
{
    let mut evidence = AttemptEvidenceBuilder::new(record_evidence);
    let mut attempt = NonZeroUsize::MIN;
    let mut retry_delays = policy.retry_delays.iter().copied();
    let mut completed_retry_wait = None;

    loop {
        if cancellation.is_requested() {
            if completed_retry_wait.is_some() {
                evidence.push(completed_retry_wait.take());
            } else {
                let _ = evidence.begin_attempt(attempt);
                evidence.record(|| LlmRequestAttemptRecord::cancelled(attempt, Duration::ZERO));
            }
            return finish(LlmRequestExecutionOutcome::Cancelled { attempt }, evidence);
        }
        if let Some(mut completed_retry_wait) = completed_retry_wait.take() {
            completed_retry_wait.mark_retry_started();
            evidence.push(Some(completed_retry_wait));
            attempt = attempt.saturating_add(1);
        }

        let attempt_started = evidence.begin_attempt(attempt);
        match llm.request(client, messages).await {
            Ok(response) => {
                evidence.record(|| {
                    LlmRequestAttemptRecord::succeeded(
                        attempt,
                        AttemptEvidenceBuilder::duration(attempt_started),
                        &response,
                    )
                });
                return finish(
                    LlmRequestExecutionOutcome::Response { response, attempt },
                    evidence,
                );
            }
            Err(LlmRequestError::Fatal(source)) => {
                let cancelled = cancellation.is_requested() && source.is_cancelled_wait();
                let diagnostic = (!cancelled)
                    .then(|| source.request_diagnostic(None, DiagnosticImpact::ProgressPreserved));
                if let Some(diagnostic) = &diagnostic {
                    evidence.record(|| {
                        LlmRequestAttemptRecord::failed(
                            attempt,
                            AttemptEvidenceBuilder::duration(attempt_started),
                            diagnostic.clone(),
                        )
                    });
                } else {
                    evidence.record(|| {
                        LlmRequestAttemptRecord::cancelled(
                            attempt,
                            AttemptEvidenceBuilder::duration(attempt_started),
                        )
                    });
                }
                return finish(
                    LlmRequestExecutionOutcome::Fatal {
                        attempt,
                        source,
                        diagnostic,
                        cancelled,
                    },
                    evidence,
                );
            }
            Err(LlmRequestError::Retryable {
                source,
                retry_after,
            }) => {
                if cancellation.is_requested() && source.is_cancelled_wait() {
                    evidence.record(|| {
                        LlmRequestAttemptRecord::cancelled(
                            attempt,
                            AttemptEvidenceBuilder::duration(attempt_started),
                        )
                    });
                    return finish(
                        LlmRequestExecutionOutcome::Fatal {
                            attempt,
                            source,
                            diagnostic: None,
                            cancelled: true,
                        },
                        evidence,
                    );
                }

                let diagnostic =
                    source.request_diagnostic(retry_after, DiagnosticImpact::ProgressPreserved);
                if let Some(retry_after) = retry_after
                    && retry_after > policy.max_retry_after
                {
                    evidence.record(|| {
                        LlmRequestAttemptRecord::retryable(
                            attempt,
                            AttemptEvidenceBuilder::duration(attempt_started),
                            diagnostic.clone(),
                            Some(retry_after),
                            None,
                        )
                    });
                    return finish(
                        LlmRequestExecutionOutcome::RetryAfterExceedsMaximum {
                            attempt,
                            diagnostic,
                            retry_after,
                            maximum: policy.max_retry_after,
                        },
                        evidence,
                    );
                }

                let Some(configured_delay) = retry_delays.next() else {
                    evidence.record(|| {
                        LlmRequestAttemptRecord::retryable(
                            attempt,
                            AttemptEvidenceBuilder::duration(attempt_started),
                            diagnostic.clone(),
                            retry_after,
                            None,
                        )
                    });
                    return finish(
                        LlmRequestExecutionOutcome::RetryBudgetExhausted {
                            attempt,
                            diagnostic,
                        },
                        evidence,
                    );
                };

                let retry_delay = configured_delay.max(retry_after.unwrap_or_default());
                let attempt_duration = AttemptEvidenceBuilder::duration(attempt_started);
                let mut diagnostic = Some(diagnostic);
                let waiting = delay.wait(retry_delay);
                tokio::pin!(waiting);
                let cancelled = cancellation.cancelled();
                tokio::pin!(cancelled);
                tokio::select! {
                    biased;
                    () = &mut cancelled => {
                        evidence.record(|| {
                            LlmRequestAttemptRecord::retryable(
                                attempt,
                                attempt_duration,
                                diagnostic.take().expect("可重试诊断必须只移动一次"),
                                retry_after,
                                Some(LlmRequestRetryWaitRecord::CancelledWhileWaiting {
                                    planned_duration: retry_delay,
                                }),
                            )
                        });
                        return finish(
                            LlmRequestExecutionOutcome::Cancelled { attempt },
                            evidence,
                        );
                    }
                    () = &mut waiting => {
                        completed_retry_wait = record_evidence.then(|| {
                            LlmRequestAttemptRecord::retryable(
                                attempt,
                                attempt_duration,
                                diagnostic.take().expect("可重试诊断必须只移动一次"),
                                retry_after,
                                Some(LlmRequestRetryWaitRecord::CompletedBeforeNextAttempt {
                                    duration: retry_delay,
                                }),
                            )
                        });
                    }
                }
                if cancellation.is_requested() {
                    evidence.push(completed_retry_wait.take());
                    return finish(LlmRequestExecutionOutcome::Cancelled { attempt }, evidence);
                }
                // 这条同步观察只供竞态测试在“等待已完成、下一请求尚未准入”的
                // 精确边界注入取消；生产调用传入零成本空闭包。
                after_completed_wait_check();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::error::Error;
    use std::fmt;
    use std::sync::{Arc, Barrier, Mutex};

    use crate::diagnostic::{
        DiagnosticAction, DiagnosticCode, DiagnosticFailureKind, DiagnosticReason, DiagnosticStage,
        DiagnosticSubject,
    };

    use super::*;

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct FakeError(&'static str);

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for FakeError {}

    impl LlmRequestDiagnosticSource for FakeError {
        fn request_diagnostic(
            &self,
            _retry_after: Option<Duration>,
            impact: DiagnosticImpact,
        ) -> SafeDiagnostic {
            SafeDiagnostic::new(
                DiagnosticCode::ModelRequest,
                DiagnosticStage::ModelRequest,
                DiagnosticSubject::component(self.0),
                DiagnosticReason::failure(DiagnosticFailureKind::TransportFailed),
                impact,
                DiagnosticAction::Retry,
            )
        }

        fn is_cancelled_wait(&self) -> bool {
            self.0 == "cancelled-wait"
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
                Ok(LlmResponse::new(
                    "{}",
                    LlmFinishReason::Stop,
                    None,
                    None,
                    None,
                )),
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
            true,
        )
        .await;
        let (outcome, evidence) = execution.into_parts();
        let (attempt_count, attempts) = evidence.into_parts();

        assert!(matches!(
            outcome,
            LlmRequestExecutionOutcome::Cancelled { attempt }
                if attempt == NonZeroUsize::MIN
        ));
        assert_eq!(*calls.lock().expect("请求计数锁不应中毒"), 1);
        assert_eq!(attempt_count, 1, "未开始的下一请求不得虚增 attempt");
        assert_eq!(attempts.len(), 1);
        assert!(matches!(
            attempts[0].outcome,
            LlmRequestAttemptOutcome::Retryable {
                retry_wait: Some(LlmRequestRetryWaitRecord::CompletedBeforeNextAttempt { .. }),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn cancellation_between_wait_checkpoint_and_next_loop_does_not_create_attempt() {
        let cancellation = CooperativeCancellation::default();
        let calls = Arc::new(Mutex::new(0));
        let llm = FakeLlm {
            responses: Arc::new(Mutex::new(VecDeque::from([
                retryable(),
                Ok(LlmResponse::new(
                    "{}",
                    LlmFinishReason::Stop,
                    None,
                    None,
                    None,
                )),
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
            true,
            move || {
                observed_boundary.wait();
                while !observed_cancellation.is_requested() {
                    std::thread::yield_now();
                }
            },
        )
        .await;
        canceller.join().expect("取消线程不应 panic");
        let (outcome, evidence) = execution.into_parts();
        let (attempt_count, attempts) = evidence.into_parts();

        assert!(matches!(
            outcome,
            LlmRequestExecutionOutcome::Cancelled { attempt }
                if attempt == NonZeroUsize::MIN
        ));
        assert_eq!(*calls.lock().expect("请求计数锁不应中毒"), 1);
        assert_eq!(attempt_count, 1);
        assert_eq!(attempts.len(), 1);
        assert!(matches!(
            attempts[0].outcome,
            LlmRequestAttemptOutcome::Retryable {
                retry_wait: Some(LlmRequestRetryWaitRecord::CompletedBeforeNextAttempt { .. }),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn successful_retry_marks_wait_as_retried_and_reuses_messages() {
        let calls = Arc::new(Mutex::new(0));
        let llm = FakeLlm {
            responses: Arc::new(Mutex::new(VecDeque::from([
                retryable(),
                Ok(LlmResponse::new(
                    "{}",
                    LlmFinishReason::Stop,
                    None,
                    None,
                    None,
                )),
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
            true,
        )
        .await;
        let (outcome, evidence) = execution.into_parts();
        let (attempt_count, attempts) = evidence.into_parts();

        assert!(matches!(
            outcome,
            LlmRequestExecutionOutcome::Response { attempt, .. }
                if attempt.get() == 2
        ));
        assert_eq!(*calls.lock().expect("请求计数锁不应中毒"), 2);
        assert_eq!(attempt_count, 2);
        assert_eq!(attempts.len(), 2);
        assert!(matches!(
            attempts[0].outcome,
            LlmRequestAttemptOutcome::Retryable {
                retry_wait: Some(LlmRequestRetryWaitRecord::Retried { .. }),
                ..
            }
        ));
        assert!(matches!(
            attempts[1].outcome,
            LlmRequestAttemptOutcome::Succeeded { .. }
        ));
    }
}
