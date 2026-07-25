//! 可选的敏感 LLM HTTP 调用记录。
//!
//! 本模块只保存发送边界已经拥有的最终请求与 Provider 结果。记录是非权威
//! 可观测性旁路，任何文件故障都只进入既有日志降级提示。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use time::OffsetDateTime;
use url::Url;

use crate::diagnostic::{DiagnosticAction, DiagnosticImpact, DiagnosticStage};

use super::filesystem::SystemFileSystem;
use super::project_log::ProjectLogger;

#[derive(Clone)]
pub(crate) struct LlmCallRecorder {
    inner: Arc<LlmCallRecorderInner>,
}

struct LlmCallRecorderInner {
    directory: PathBuf,
    run_id: String,
    next_call: AtomicU64,
    file_system: SystemFileSystem,
    warnings: ProjectLogger,
}

pub(crate) struct PendingLlmCall {
    number: u64,
    started_at: OffsetDateTime,
    started: Instant,
    endpoint: String,
    request_body: Vec<u8>,
}

pub(crate) enum LlmCallOutcome<'a> {
    ResponseParsed { status: u16, body: &'a [u8] },
    HttpError { status: u16, body: &'a [u8] },
    ResponseParseFailed { status: u16, body: &'a [u8] },
    ResponseNotReceived,
    BodyReadFailed { status: u16 },
}

impl LlmCallRecorder {
    pub(crate) fn new(
        directory: PathBuf,
        run_id: String,
        file_system: SystemFileSystem,
        warnings: ProjectLogger,
    ) -> Self {
        Self {
            inner: Arc::new(LlmCallRecorderInner {
                directory,
                run_id,
                next_call: AtomicU64::new(0),
                file_system,
                warnings,
            }),
        }
    }

    /// 为一个即将进入 `request.send()` 的实际 HTTP 尝试分配运行内编号。
    ///
    /// 此处只复制已经序列化的请求，不建立目录或写文件。
    pub(crate) fn begin(&self, endpoint: &Url, request_body: &[u8]) -> PendingLlmCall {
        let number = self.inner.next_call.fetch_add(1, Ordering::Relaxed) + 1;
        let mut endpoint = endpoint.clone();
        let _ = endpoint.set_username("");
        let _ = endpoint.set_password(None);
        endpoint.set_query(None);
        endpoint.set_fragment(None);
        PendingLlmCall {
            number,
            started_at: OffsetDateTime::now_utc(),
            started: Instant::now(),
            endpoint: endpoint.to_string(),
            request_body: request_body.to_vec(),
        }
    }

    /// 一次性写出已经结束的 HTTP 尝试；失败只登记非致命日志降级。
    pub(crate) async fn record(&self, call: PendingLlmCall, outcome: LlmCallOutcome<'_>) {
        let path = self
            .inner
            .directory
            .join(format!("call-{:06}.md", call.number));
        let markdown = render_call(&self.inner.run_id, &call, call.started.elapsed(), outcome);
        if let Err(error) = self
            .inner
            .file_system
            .write_new_observation_file(path, markdown.into_bytes())
            .await
        {
            self.inner
                .warnings
                .record_observability_failure(error.safe_diagnostic(
                    DiagnosticStage::Logging,
                    DiagnosticImpact::Unchanged,
                    DiagnosticAction::CheckPathAndPermissions,
                ));
        }
    }
}

fn render_call(
    run_id: &str,
    call: &PendingLlmCall,
    duration: Duration,
    outcome: LlmCallOutcome<'_>,
) -> String {
    let (result, status, response) = match outcome {
        LlmCallOutcome::ResponseParsed { status, body } => {
            ("response_parsed", Some(status), render_response_body(body))
        }
        LlmCallOutcome::HttpError { status, body } => {
            ("http_error", Some(status), render_response_body(body))
        }
        LlmCallOutcome::ResponseParseFailed { status, body } => (
            "response_parse_failed",
            Some(status),
            render_response_body(body),
        ),
        LlmCallOutcome::ResponseNotReceived => (
            "response_not_received",
            None,
            "_No response body was received._\n".to_owned(),
        ),
        LlmCallOutcome::BodyReadFailed { status } => (
            "body_read_failed",
            Some(status),
            "_The response body could not be read._\n".to_owned(),
        ),
    };

    let mut output = String::new();
    output.push_str(&format!("# LLM Call {:06}\n\n", call.number));
    output.push_str(
        "> Sensitive local diagnostic. It may contain prompts, source text, translations, \
custom parameters, and model output. Review before sharing and do not commit it.\n\n",
    );
    output.push_str(&format!("- Run ID: `{run_id}`\n"));
    output.push_str(&format!("- Call: `{:06}`\n", call.number));
    output.push_str(&format!(
        "- Started (UTC): `{}`\n",
        recorded_at_utc(call.started_at)
    ));
    output.push_str(&format!("- Duration: `{} ms`\n", duration.as_millis()));
    output.push_str(&format!("- Endpoint: `{}`\n", call.endpoint));
    output.push_str(&format!("- Result: `{result}`\n"));
    if let Some(status) = status {
        output.push_str(&format!("- HTTP status: `{status}`\n"));
    }
    output.push_str("\n## Request\n\n");
    output.push_str(&markdown_fence(
        &render_request_body(&call.request_body),
        "json",
    ));
    output.push_str("\n## Provider Response\n\n");
    output.push_str(&response);
    output
}

fn render_request_body(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| String::from_utf8_lossy(body).into_owned())
}

fn render_response_body(body: &[u8]) -> String {
    match std::str::from_utf8(body) {
        Ok(body) => markdown_fence(body, "text"),
        Err(_) => {
            let mut output = format!(
                "> Response body is not valid UTF-8; displayed with replacement characters \
({} bytes).\n\n",
                body.len()
            );
            output.push_str(&markdown_fence(&String::from_utf8_lossy(body), "text"));
            output
        }
    }
}

fn markdown_fence(content: &str, language: &str) -> String {
    let mut longest = 0usize;
    let mut current = 0usize;
    for byte in content.bytes() {
        if byte == b'`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    let fence = "`".repeat(longest.saturating_add(1).max(3));
    let mut output = format!("{fence}{language}\n");
    output.push_str(content);
    if !content.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(&fence);
    output.push('\n');
    output
}

fn recorded_at_utc(now: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.nanosecond() / 1_000_000,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renderer_keeps_request_response_and_uses_non_conflicting_fences() {
        let call = PendingLlmCall {
            number: 7,
            started_at: OffsetDateTime::UNIX_EPOCH,
            started: Instant::now(),
            endpoint: "https://example.com/v1/chat/completions".to_owned(),
            request_body:
                br#"{"model":"test","messages":[{"role":"user","content":"rule ``` kept"}],"stream":false}"#
                    .to_vec(),
        };
        let markdown = render_call(
            "550e8400-e29b-41d4-a716-446655440000",
            &call,
            Duration::from_millis(12),
            LlmCallOutcome::ResponseParsed {
                status: 200,
                body: br#"{"choices":[{"message":{"content":"answer ```` kept"}}]}"#,
            },
        );

        assert!(markdown.contains("# LLM Call 000007"));
        assert!(markdown.contains("\"content\": \"rule ``` kept\""));
        assert!(markdown.contains("answer ```` kept"));
        assert!(markdown.contains("- Result: `response_parsed`"));
        assert!(markdown.contains("- HTTP status: `200`"));
        assert!(markdown.contains("`````text"));
    }

    #[test]
    fn renderer_marks_missing_and_invalid_response_bodies() {
        let call = PendingLlmCall {
            number: 1,
            started_at: OffsetDateTime::UNIX_EPOCH,
            started: Instant::now(),
            endpoint: "https://example.com/".to_owned(),
            request_body: b"{}".to_vec(),
        };
        let missing = render_call(
            "run",
            &call,
            Duration::ZERO,
            LlmCallOutcome::ResponseNotReceived,
        );
        let invalid = render_call(
            "run",
            &call,
            Duration::ZERO,
            LlmCallOutcome::ResponseParseFailed {
                status: 200,
                body: &[0xff, b'a'],
            },
        );

        assert!(missing.contains("response_not_received"));
        assert!(invalid.contains("not valid UTF-8"));
        assert!(invalid.contains("2 bytes"));
    }
}
