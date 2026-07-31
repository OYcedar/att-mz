//! Generic 模型任务的可读、非权威记录投影。
//!
//! 本模块只把 Generic 已经确认的请求、响应验收和提交终态渲染为 Markdown。目录、
//! 文件命名、并发写入和写入故障处理由公共任务记录 sink 负责。

use std::fmt::Write as _;
use std::time::Duration;

use serde_json::value::RawValue;
use time::OffsetDateTime;

use crate::diagnostic::SafeDiagnostic;
use crate::execution::llm_request::LlmRequestAttemptRecord;
use crate::i18n::{UiLocale, UiLocalizer, UiMessage};
use crate::llm::{ChatMessage, ChatMessageRole, LlmClientRecordMetadata};
use crate::translation::task_record::{
    TranslationTaskRecordArtifact, markdown_fence, markdown_heading_id, markdown_inline_code,
    recorded_at_utc, render_client_parameters, render_duration, render_task_record_attempt,
    task_record_text,
};
use crate::translation_protocol::{
    ParsedTranslationResponse, TranslationTaskResponseParseError,
    TranslationTaskResponseParseErrorKind,
};

use super::ResponseProblem;

#[derive(Debug)]
pub(crate) enum GenericTaskResponseRecord {
    Parsed {
        thinking: Option<String>,
        entries: Vec<(String, GenericTaskRecordedValue)>,
    },
    Invalid {
        raw_assistant: String,
        error: TranslationTaskResponseParseError,
    },
    Unprocessed {
        raw_assistant: String,
    },
}

#[derive(Debug)]
pub(crate) enum GenericTaskRecordedValue {
    Text(String),
    RawJson(Box<RawValue>),
}

impl GenericTaskResponseRecord {
    pub(crate) fn parsed_with_cancellation<E>(
        parsed: ParsedTranslationResponse,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Self, E> {
        ensure_running()?;
        let (thinking, entries) = parsed.into_parts();
        let mut recorded_entries = Vec::with_capacity(entries.len());
        for entry in entries {
            ensure_running()?;
            let decoded = entry.decode_value_with_cancellation::<String, _>(&mut ensure_running)?;
            let (id, value, _) = entry.into_parts();
            let value = match decoded {
                Ok(value) => GenericTaskRecordedValue::Text(value),
                Err(_) => GenericTaskRecordedValue::RawJson(value),
            };
            recorded_entries.push((id, value));
        }
        ensure_running()?;
        Ok(Self::Parsed {
            thinking,
            entries: recorded_entries,
        })
    }

    pub(crate) const fn invalid(
        raw_assistant: String,
        error: TranslationTaskResponseParseError,
    ) -> Self {
        Self::Invalid {
            raw_assistant,
            error,
        }
    }

    pub(crate) const fn unprocessed(raw_assistant: String) -> Self {
        Self::Unprocessed { raw_assistant }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GenericTaskRecordIssue {
    id: String,
    code: &'static str,
    detail: Option<String>,
}

impl GenericTaskRecordIssue {
    pub(crate) fn from_response_problem_with_cancellation<E>(
        problem: &ResponseProblem,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Self, E> {
        ensure_running()?;
        let issue = match problem {
            ResponseProblem::InvalidId(id) => Self {
                id: clone_task_record_text(id, &mut ensure_running)?,
                code: "invalid_id",
                detail: None,
            },
            ResponseProblem::UnexpectedId(id) => Self {
                id: id.to_string(),
                code: "unexpected_id",
                detail: None,
            },
            ResponseProblem::DuplicateId(id) => Self {
                id: id.to_string(),
                code: "duplicate_id",
                detail: None,
            },
            ResponseProblem::MissingId(id) => Self {
                id: id.to_string(),
                code: "missing_id",
                detail: None,
            },
            ResponseProblem::NonStringValue(id) => Self {
                id: id.to_string(),
                code: "non_string_value",
                detail: None,
            },
            ResponseProblem::InvalidTranslation { output_id, detail } => Self {
                id: output_id.to_string(),
                code: "invalid_translation",
                detail: Some(clone_task_record_text(detail, &mut ensure_running)?),
            },
            ResponseProblem::InvalidDestination {
                output_id,
                key,
                detail,
            } => Self {
                id: {
                    let mut id = output_id.to_string();
                    id.push(':');
                    append_task_record_text(&mut id, key.group_id(), &mut ensure_running)?;
                    id.push('/');
                    append_task_record_text(&mut id, key.unit_id(), &mut ensure_running)?;
                    id
                },
                code: "invalid_destination_translation",
                detail: Some(clone_task_record_text(detail, &mut ensure_running)?),
            },
        };
        ensure_running()?;
        Ok(issue)
    }

    pub(crate) fn commit_conflicts(count: usize) -> Self {
        Self {
            id: "commit".to_owned(),
            code: "cas_conflict",
            detail: Some(format!("{count} 个 Unit 在提交前已发生变化")),
        }
    }
}

fn clone_task_record_text<E>(
    text: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<String, E> {
    let mut output = String::with_capacity(text.len());
    append_task_record_text(&mut output, text, ensure_running)?;
    Ok(output)
}

fn append_task_record_text<E>(
    output: &mut String,
    text: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    let mut start = 0_usize;
    while start < text.len() {
        ensure_running()?;
        let mut end = start
            .saturating_add(CANCELLATION_CHECK_BYTES)
            .min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end -= 1;
        }
        output.push_str(&text[start..end]);
        start = end;
    }
    ensure_running()
}

#[derive(Clone, Debug)]
pub(crate) struct GenericTaskRecordState {
    code: &'static str,
    accepted: usize,
    written: usize,
    issues: Vec<GenericTaskRecordIssue>,
    diagnostic: Option<SafeDiagnostic>,
}

impl GenericTaskRecordState {
    pub(crate) fn committed(
        complete: bool,
        accepted: usize,
        written: usize,
        issues: Vec<GenericTaskRecordIssue>,
    ) -> Self {
        Self {
            code: if complete && issues.is_empty() {
                "complete"
            } else {
                "partial"
            },
            accepted,
            written,
            issues,
            diagnostic: None,
        }
    }

    pub(crate) fn unavailable(code: &'static str) -> Self {
        Self {
            code: "unavailable",
            accepted: 0,
            written: 0,
            issues: vec![GenericTaskRecordIssue {
                id: "task".to_owned(),
                code,
                detail: None,
            }],
            diagnostic: None,
        }
    }

    pub(crate) fn cancelled() -> Self {
        Self {
            code: "cancelled",
            accepted: 0,
            written: 0,
            issues: Vec::new(),
            diagnostic: None,
        }
    }

    pub(crate) fn not_committed_due_to_prior_failure() -> Self {
        Self {
            code: "not_committed",
            accepted: 0,
            written: 0,
            issues: vec![GenericTaskRecordIssue {
                id: "task".to_owned(),
                code: "prior_task_failed",
                detail: None,
            }],
            diagnostic: None,
        }
    }

    pub(crate) fn failed(diagnostic: SafeDiagnostic) -> Self {
        Self {
            code: "execution_failed",
            accepted: 0,
            written: 0,
            issues: Vec::new(),
            diagnostic: Some(diagnostic),
        }
    }
}

pub(crate) struct GenericTaskRecordDocument {
    total_tasks: usize,
    task_index: usize,
    messages: Vec<ChatMessage>,
    expected_outputs: usize,
    started_at: OffsetDateTime,
    duration: Duration,
    attempt_count: usize,
    attempts: Vec<LlmRequestAttemptRecord>,
    response: Option<GenericTaskResponseRecord>,
    state: GenericTaskRecordState,
}

impl GenericTaskRecordDocument {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        total_tasks: usize,
        task_index: usize,
        messages: Vec<ChatMessage>,
        expected_outputs: usize,
        started_at: OffsetDateTime,
        duration: Duration,
        attempt_count: usize,
        attempts: Vec<LlmRequestAttemptRecord>,
        response: Option<GenericTaskResponseRecord>,
        state: GenericTaskRecordState,
    ) -> Self {
        Self {
            total_tasks,
            task_index,
            messages,
            expected_outputs,
            started_at,
            duration,
            attempt_count,
            attempts,
            response,
            state,
        }
    }
}

impl TranslationTaskRecordArtifact for GenericTaskRecordDocument {
    fn task_index(&self) -> usize {
        self.task_index
    }

    fn total_tasks(&self) -> usize {
        self.total_tasks
    }

    fn render(
        &self,
        run_id: &str,
        client: &LlmClientRecordMetadata,
        locale: UiLocale,
        total_tasks: usize,
    ) -> Result<String, serde_json::Error> {
        render_generic_task_record(run_id, client, locale, self, total_tasks)
    }
}

fn render_generic_task_record(
    run_id: &str,
    client: &LlmClientRecordMetadata,
    locale: UiLocale,
    document: &GenericTaskRecordDocument,
    total_tasks: usize,
) -> Result<String, serde_json::Error> {
    let localizer = UiLocalizer::new(locale);
    let redactor = client.api_key_redactor();
    let ordinal = document.task_index.saturating_add(1);
    let mut output = String::new();
    let state_label = task_record_text(
        &localizer,
        UiMessage::TaskRecordStateLabel {
            state: document.state.code,
        },
    );
    let padded_ordinal = format!("{ordinal:06}");
    let title = task_record_text(
        &localizer,
        UiMessage::TaskRecordTitle {
            ordinal: &padded_ordinal,
            state: &state_label,
        },
    );
    let _ = writeln!(output, "# {title}\n");
    let summary = task_record_text(
        &localizer,
        UiMessage::TaskRecordSummaryWithWritten {
            ordinal: ordinal as u64,
            total: total_tasks as u64,
            attempts: document.attempt_count as u64,
            accepted: document.state.accepted as u64,
            expected: document.expected_outputs as u64,
            written: document.state.written as u64,
        },
    );
    let _ = writeln!(output, "{summary}\n");
    let _ = writeln!(output, "- Engine: `generic`");
    let run_id = markdown_inline_code(&redactor.redact(run_id));
    let _ = writeln!(
        output,
        "- {}{run_id}",
        task_record_text(&localizer, UiMessage::TaskRecordRunIdLabel),
    );
    let started_at = markdown_inline_code(&recorded_at_utc(document.started_at));
    let _ = writeln!(
        output,
        "- {}{started_at}",
        task_record_text(&localizer, UiMessage::TaskRecordStartedAtLabel),
    );
    let duration = markdown_inline_code(&render_duration(&localizer, document.duration));
    let _ = writeln!(
        output,
        "- {}{duration}",
        task_record_text(&localizer, UiMessage::TaskRecordDurationLabel),
    );
    let endpoint = markdown_inline_code(client.endpoint());
    let _ = writeln!(
        output,
        "- {}{endpoint}",
        task_record_text(&localizer, UiMessage::TaskRecordEndpointLabel),
    );
    let model = markdown_inline_code(client.model());
    let _ = writeln!(
        output,
        "- {}{model}",
        task_record_text(&localizer, UiMessage::TaskRecordModelLabel),
    );

    let _ = write!(
        output,
        "\n## {}\n\n",
        task_record_text(&localizer, UiMessage::TaskRecordCustomParametersHeading)
    );
    output.push_str(&markdown_fence(&render_client_parameters(client)?, "json"));

    for message in &document.messages {
        let title = match message.role() {
            ChatMessageRole::System => "System",
            ChatMessageRole::User => "User",
        };
        let _ = write!(output, "\n## {title}\n\n");
        let content = match message.role() {
            ChatMessageRole::System => redactor.redact(message.content()),
            ChatMessageRole::User => redactor.redact_text_with_json_strings(message.content()),
        };
        output.push_str(&content);
        if !content.ends_with('\n') {
            output.push('\n');
        }
    }

    let _ = write!(
        output,
        "\n## {}\n\n",
        task_record_text(&localizer, UiMessage::TaskRecordAttemptsHeading)
    );
    if document.attempts.is_empty() {
        let _ = writeln!(
            output,
            "- {}",
            task_record_text(&localizer, UiMessage::TaskRecordNoRequest)
        );
    } else {
        for attempt in &document.attempts {
            render_task_record_attempt(&mut output, &localizer, redactor, attempt)?;
        }
    }

    if let Some(response) = &document.response {
        render_response(&mut output, &localizer, client, response)?;
    }

    let _ = write!(
        output,
        "\n## {}\n\n",
        task_record_text(&localizer, UiMessage::TaskRecordFinalResultHeading)
    );
    let _ = writeln!(
        output,
        "- {}",
        task_record_text(
            &localizer,
            UiMessage::TaskRecordFinalStatus {
                state: document.state.code,
            }
        )
    );
    let _ = writeln!(
        output,
        "- {}",
        task_record_text(
            &localizer,
            UiMessage::TaskRecordAcceptedWritten {
                accepted: document.state.accepted as u64,
                written: document.state.written as u64,
            }
        )
    );
    if !document.state.issues.is_empty() {
        let _ = writeln!(
            output,
            "- {}",
            task_record_text(&localizer, UiMessage::TaskRecordRejectedHeading)
        );
        for issue in &document.state.issues {
            let detail = issue.detail.as_deref().unwrap_or(issue.code);
            let reason = markdown_inline_code(&redactor.redact(detail));
            let id = markdown_inline_code(&redactor.redact(&issue.id));
            let _ = writeln!(
                output,
                "  - {}",
                task_record_text(
                    &localizer,
                    UiMessage::TaskRecordRejectedItem {
                        id: &id,
                        reason: &reason,
                    }
                )
            );
        }
    }
    if let Some(diagnostic) = &document.state.diagnostic {
        let reason =
            markdown_inline_code(&redactor.redact(&diagnostic.reason.render_localized(&localizer)));
        let _ = writeln!(
            output,
            "- {}",
            task_record_text(
                &localizer,
                UiMessage::TaskRecordTaskDiagnostic {
                    code: diagnostic.code.as_str(),
                    reason: &reason,
                }
            )
        );
    }
    Ok(output)
}

fn render_response(
    output: &mut String,
    localizer: &UiLocalizer,
    client: &LlmClientRecordMetadata,
    response: &GenericTaskResponseRecord,
) -> Result<(), serde_json::Error> {
    let redactor = client.api_key_redactor();
    match response {
        GenericTaskResponseRecord::Parsed { thinking, entries } => {
            if let Some(thinking) = thinking {
                output.push_str("\n## Thinking\n\n");
                let thinking = redactor.redact(thinking);
                output.push_str(&thinking);
                if !thinking.ends_with('\n') {
                    output.push('\n');
                }
            }
            output.push_str("\n## Assistant\n\n");
            if entries.is_empty() {
                let _ = writeln!(
                    output,
                    "_{}_",
                    task_record_text(localizer, UiMessage::TaskRecordEmptyAssistant)
                );
            }
            for (id, value) in entries {
                let id = redactor.redact(id);
                let _ = writeln!(output, "### ID {}\n", markdown_heading_id(&id));
                match value {
                    GenericTaskRecordedValue::Text(value) => {
                        let value = redactor.redact(value);
                        output.push_str(&value);
                        if !value.ends_with('\n') {
                            output.push('\n');
                        }
                    }
                    GenericTaskRecordedValue::RawJson(value) => {
                        let value = redactor.redact_json(value.as_ref())?;
                        output.push_str(&markdown_fence(&value, "json"));
                    }
                }
                output.push('\n');
            }
        }
        GenericTaskResponseRecord::Invalid {
            raw_assistant,
            error,
        } => {
            output.push_str("\n## Assistant\n\n");
            let category = match error.kind() {
                TranslationTaskResponseParseErrorKind::Json(category) => category.code(),
                _ => "",
            };
            let _ = writeln!(
                output,
                "> {}\n",
                task_record_text(
                    localizer,
                    UiMessage::TaskRecordParseError {
                        kind: error.kind().code(),
                        category,
                        line: error.line().get() as u64,
                        column: error.column().get() as u64,
                    }
                )
            );
            output.push_str(&markdown_fence(
                &redactor.redact_text_with_json_strings(raw_assistant),
                "text",
            ));
        }
        GenericTaskResponseRecord::Unprocessed { raw_assistant } => {
            output.push_str("\n## Assistant\n\n");
            output.push_str(&markdown_fence(
                &redactor.redact_text_with_json_strings(raw_assistant),
                "text",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use secrecy::SecretString;
    use serde_json::Map;

    use super::*;
    use crate::llm::ApiKeyRedactor;
    use crate::translation_protocol::{TranslationResponseEnvelope, parse_translation_response};

    #[test]
    fn deeply_nested_raw_value_renders_as_valid_redacted_json_without_value_tree() {
        const API_KEY: &str = "quote\"slash\\value";
        const DEPTH: usize = 10_000;

        let encoded_api_key = serde_json::to_string(API_KEY).expect("API key 应可编码为 JSON");
        let deep_value = format!(
            "{}{}{}",
            "[".repeat(DEPTH),
            encoded_api_key,
            "]".repeat(DEPTH)
        );
        let parsed = parse_translation_response(
            &format!(r#"{{"1":{deep_value}}}"#),
            TranslationResponseEnvelope::JsonOnly,
        )
        .expect("深层 raw value 应可解析");
        let record =
            GenericTaskResponseRecord::parsed_with_cancellation(parsed, || Ok::<_, Infallible>(()))
                .expect("未取消的记录投影应成功");
        let client = LlmClientRecordMetadata::new(
            "https://example.test".to_owned(),
            "model".to_owned(),
            Map::new(),
            ApiKeyRedactor::new(SecretString::from(API_KEY)),
        );
        let mut output = String::new();

        render_response(
            &mut output,
            &UiLocalizer::new(UiLocale::SimplifiedChinese),
            &client,
            &record,
        )
        .expect("RawValue 的 JSON 序列化不应递归展开");

        let opening = "```json\n";
        let json_start = output.find(opening).expect("非字符串值应进入 JSON fence") + opening.len();
        let json_end = json_start
            + output[json_start..]
                .find("\n```\n")
                .expect("JSON fence 应闭合");
        let recorded_json = &output[json_start..json_end];
        serde_json::from_str::<Box<RawValue>>(recorded_json)
            .expect("脱敏后的记录仍必须是有效 JSON");
        let escaped_api_key = &encoded_api_key[1..encoded_api_key.len() - 1];
        assert!(!recorded_json.contains(API_KEY));
        assert!(!recorded_json.contains(escaped_api_key));
        assert!(recorded_json.contains("[REDACTED API KEY]"));

        drop(record);
    }

    #[test]
    fn rendered_generic_user_and_unprocessed_assistant_redact_json_escaped_key() {
        const API_KEY: &str = "quote\"slash\\value";
        let encoded_api_key = serde_json::to_string(API_KEY).expect("API key 应可编码为 JSON");
        let encoded_fragment = &encoded_api_key[1..encoded_api_key.len() - 1];
        let document = GenericTaskRecordDocument::new(
            1,
            0,
            vec![ChatMessage::new(
                ChatMessageRole::User,
                format!("units:\n[1] \"before-{encoded_fragment}-after\""),
            )],
            1,
            OffsetDateTime::UNIX_EPOCH,
            Duration::ZERO,
            0,
            Vec::new(),
            Some(GenericTaskResponseRecord::unprocessed(format!(
                "prefix {{\"1\":\"before-{encoded_fragment}-after\"}} trailing {{"
            ))),
            GenericTaskRecordState::cancelled(),
        );
        let client = LlmClientRecordMetadata::new(
            "https://example.test".to_owned(),
            "model".to_owned(),
            Map::new(),
            ApiKeyRedactor::new(SecretString::from(API_KEY)),
        );

        let markdown = render_generic_task_record(
            "run-redaction",
            &client,
            UiLocale::SimplifiedChinese,
            &document,
            1,
        )
        .expect("Generic 任务记录应可渲染");

        assert!(!markdown.contains(API_KEY));
        assert!(!markdown.contains(encoded_fragment));
        assert_eq!(markdown.matches("[REDACTED API KEY]").count(), 2);
    }
}
