//! Generic 模型任务的可读、非权威记录投影。
//!
//! 本模块只把 Generic 已经确认的请求、响应验收和提交终态渲染为 Markdown。目录、
//! 文件命名、并发写入和写入故障处理由公共任务记录 sink 负责。

use std::fmt::Write as _;
use std::time::Duration;

use serde_json::Value;
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

#[derive(Clone, Debug)]
pub(crate) enum GenericTaskResponseRecord {
    Parsed {
        thinking: Option<String>,
        entries: Vec<(String, Value)>,
    },
    Invalid {
        raw_assistant: String,
        error: TranslationTaskResponseParseError,
    },
    Unprocessed {
        raw_assistant: String,
    },
}

impl GenericTaskResponseRecord {
    pub(crate) fn parsed(parsed: ParsedTranslationResponse) -> Self {
        let (thinking, entries) = parsed.into_parts();
        Self::Parsed {
            thinking,
            entries: entries
                .into_iter()
                .map(|entry| {
                    let (id, value, _) = entry.into_parts();
                    (id, value)
                })
                .collect(),
        }
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
    pub(crate) fn from_response_problem(problem: &ResponseProblem) -> Self {
        match problem {
            ResponseProblem::InvalidId(id) => Self {
                id: id.clone(),
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
                detail: Some(detail.clone()),
            },
            ResponseProblem::InvalidDestination {
                output_id,
                key,
                detail,
            } => Self {
                id: format!("{output_id}:{}/{}", key.group_id(), key.unit_id()),
                code: "invalid_destination_translation",
                detail: Some(detail.clone()),
            },
        }
    }

    pub(crate) fn commit_conflicts(count: usize) -> Self {
        Self {
            id: "commit".to_owned(),
            code: "cas_conflict",
            detail: Some(format!("{count} 个 Unit 在提交前已发生变化")),
        }
    }
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
        let content = redactor.redact(message.content());
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
                    Value::String(value) => {
                        let value = redactor.redact(value);
                        output.push_str(&value);
                        if !value.ends_with('\n') {
                            output.push('\n');
                        }
                    }
                    value => {
                        let value = redactor.redact_json_pretty(value)?;
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
            output.push_str(&markdown_fence(&redactor.redact(raw_assistant), "text"));
        }
        GenericTaskResponseRecord::Unprocessed { raw_assistant } => {
            output.push_str("\n## Assistant\n\n");
            output.push_str(&markdown_fence(&redactor.redact(raw_assistant), "text"));
        }
    }
    Ok(())
}
