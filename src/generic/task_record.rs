//! Generic 模型任务的可读、非权威记录投影。
//!
//! 本模块只把 Generic 已经确认的请求、响应验收和提交终态渲染为 Markdown。目录、
//! 文件命名、并发写入和写入故障处理由公共任务记录 sink 负责。

use std::fmt::Write as _;
use std::time::Duration;

use serde_json::value::RawValue;
use time::OffsetDateTime;

use crate::diagnostic::{DiagnosticReport, render_diagnostic_report};
use crate::execution::llm_request::LlmRequestAttemptRecord;
use crate::i18n::{UiLocale, UiLocalizer, UiMessage};
use crate::llm::{ChatMessage, ChatMessageRole, LlmClientRecordMetadata};
use crate::translation::task_record::{
    TranslationTaskRecordArtifact, markdown_fence, markdown_heading_id, markdown_inline_code,
    recorded_at_utc, render_client_parameters, render_duration, render_json_repairs,
    render_raw_assistant, render_repaired_raw_assistant, render_task_record_attempt,
    task_record_text,
};
use crate::translation_protocol::{
    DecodedJsonStringArray, DecodedSourceEchoValue, DecodedTranslationAssistantValue,
    ParsedTranslationResponse, TranslationResponseRepair, TranslationTaskResponseParseError,
    TranslationTaskResponseParseErrorKind,
};

#[derive(Debug)]
pub(crate) enum GenericTaskResponseRecord {
    Parsed {
        thinking: Option<String>,
        entries: Vec<(String, GenericTaskRecordedValue)>,
        repairs: Vec<TranslationResponseRepair>,
        raw_assistant: String,
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
    Lines(Vec<String>),
    RawJson(Box<RawValue>),
}

impl GenericTaskResponseRecord {
    pub(crate) fn parsed_with_cancellation<E>(
        raw_assistant: String,
        parsed: ParsedTranslationResponse,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Self, E> {
        ensure_running()?;
        let (thinking, entries, repairs) = parsed.into_parts();
        let mut recorded_entries = Vec::with_capacity(entries.len());
        for entry in entries {
            ensure_running()?;
            let decoded = entry.decode_translation_value_with_cancellation(&mut ensure_running)?;
            let (id, value, _) = entry.into_parts();
            let value = match decoded {
                DecodedTranslationAssistantValue::Translation(DecodedJsonStringArray::Strings(
                    lines,
                ))
                | DecodedTranslationAssistantValue::SourceEcho(DecodedSourceEchoValue::Fields {
                    source: DecodedJsonStringArray::Strings(_),
                    translation: DecodedJsonStringArray::Strings(lines),
                }) => GenericTaskRecordedValue::Lines(lines),
                _ => GenericTaskRecordedValue::RawJson(value),
            };
            recorded_entries.push((id, value));
        }
        ensure_running()?;
        Ok(Self::Parsed {
            thinking,
            entries: recorded_entries,
            repairs,
            raw_assistant,
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

#[derive(Clone, Debug)]
pub(crate) struct GenericTaskRecordState {
    code: &'static str,
    accepted: usize,
    written: usize,
    diagnostics: Vec<DiagnosticReport>,
}

impl GenericTaskRecordState {
    pub(crate) fn committed(
        complete: bool,
        accepted: usize,
        written: usize,
        diagnostics: Vec<DiagnosticReport>,
    ) -> Self {
        Self {
            code: if complete && diagnostics.is_empty() {
                "complete"
            } else {
                "partial"
            },
            accepted,
            written,
            diagnostics,
        }
    }

    pub(crate) fn unavailable(diagnostic: DiagnosticReport) -> Self {
        Self {
            code: "unavailable",
            accepted: 0,
            written: 0,
            diagnostics: vec![diagnostic],
        }
    }

    pub(crate) fn cancelled() -> Self {
        Self {
            code: "cancelled",
            accepted: 0,
            written: 0,
            diagnostics: Vec::new(),
        }
    }

    pub(crate) fn not_committed_due_to_prior_failure() -> Self {
        Self {
            code: "not_committed",
            accepted: 0,
            written: 0,
            diagnostics: Vec::new(),
        }
    }

    pub(crate) fn failed(diagnostic: DiagnosticReport) -> Self {
        Self {
            code: "execution_failed",
            accepted: 0,
            written: 0,
            diagnostics: vec![diagnostic],
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
    for diagnostic in &document.state.diagnostics {
        let code = diagnostic.primary().code();
        let reason = markdown_inline_code(code);
        let _ = writeln!(
            output,
            "- {}",
            task_record_text(
                &localizer,
                UiMessage::TaskRecordTaskDiagnostic {
                    code,
                    reason: &reason,
                }
            )
        );
        let rendered = redactor.redact(&render_diagnostic_report(diagnostic, &localizer));
        output.push_str(&markdown_fence(&rendered, "text"));
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
        GenericTaskResponseRecord::Parsed {
            thinking,
            entries,
            repairs,
            raw_assistant,
        } => {
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
                    GenericTaskRecordedValue::Lines(lines) => {
                        for (line_index, line) in lines.iter().enumerate() {
                            if line_index != 0 {
                                output.push_str("\n\n");
                            }
                            output.push_str(&redactor.redact(line));
                        }
                        output.push('\n');
                    }
                    GenericTaskRecordedValue::RawJson(value) => {
                        let value = redactor.redact_json(value.as_ref())?;
                        output.push_str(&markdown_fence(&value, "json"));
                    }
                }
                output.push('\n');
            }
            render_json_repairs(output, repairs);
            if thinking.is_some() || !repairs.is_empty() {
                output.push_str("\n## Raw Assistant\n\n");
                output.push_str(&if repairs.is_empty() {
                    render_raw_assistant(raw_assistant, redactor)
                } else {
                    render_repaired_raw_assistant(raw_assistant, redactor)
                });
            }
        }
        GenericTaskResponseRecord::Invalid {
            raw_assistant,
            error,
        } => {
            output.push_str("\n## Assistant\n\n");
            let category = match error.kind() {
                TranslationTaskResponseParseErrorKind::Json(category)
                | TranslationTaskResponseParseErrorKind::JsonRepair { category, .. } => {
                    category.code()
                }
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
    use crate::translation_protocol::{TranslationResponseMode, parse_translation_response};

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
        let raw_assistant = format!(r#"{{"0":{deep_value}}}"#);
        let parsed =
            parse_translation_response(&raw_assistant, TranslationResponseMode::new(false, false))
                .expect("深层 raw value 应可解析");
        let record =
            GenericTaskResponseRecord::parsed_with_cancellation(raw_assistant, parsed, || {
                Ok::<_, Infallible>(())
            })
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
        assert!(!output.contains("## Raw Assistant"));

        drop(record);
    }

    #[test]
    fn thinking_source_echo_renders_translation_and_safe_raw_assistant() {
        const API_KEY: &str = "quote\"slash\\value";
        let raw_assistant = serde_json::json!({
            "think": format!("判断 ``` {API_KEY}"),
            "translations": {
                "0": {
                    "source": ["原文"],
                    "translation": ["第一行", "", "第二行"]
                }
            }
        })
        .to_string();
        let parsed =
            parse_translation_response(&raw_assistant, TranslationResponseMode::new(true, true))
                .expect("thinking 与原文回显响应应该可解析");
        let record =
            GenericTaskResponseRecord::parsed_with_cancellation(raw_assistant, parsed, || {
                Ok::<_, Infallible>(())
            })
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
        .expect("thinking 成功响应应该可渲染");

        assert!(output.contains("## Thinking"));
        assert!(output.contains("## Assistant\n\n### ID 0\n\n第一行\n\n\n\n第二行"));
        assert!(output.contains("## Raw Assistant\n\n````json\n"));
        assert!(!output.contains(API_KEY));
        assert!(output.contains("[REDACTED API KEY]"));
    }

    #[test]
    fn non_thinking_noncanonical_fenced_json_records_repairs_and_safe_text_raw_assistant() {
        const API_KEY: &str = "quote\"slash\\value";
        let encoded_api_key = serde_json::to_string(API_KEY).expect("API key 应可编码为 JSON");
        let encoded_fragment = &encoded_api_key[1..encoded_api_key.len() - 1];
        let raw_assistant = format!(
            "\u{2003}\r\n```json\r\n{{\"0\":[\"before-{encoded_fragment}-after\"]}}\r\n```\r\n"
        );
        let parsed =
            parse_translation_response(&raw_assistant, TranslationResponseMode::new(false, false))
                .expect("非 thinking 的非规范围栏应可保守修复");
        let record =
            GenericTaskResponseRecord::parsed_with_cancellation(raw_assistant, parsed, || {
                Ok::<_, Infallible>(())
            })
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
        .expect("修复后的非 thinking 响应应该可渲染");

        assert!(output.contains("## JSON Repairs"));
        assert_eq!(output.matches("`removed_markdown_fence`").count(), 2);
        assert!(output.contains("| `removed_markdown_fence` | 2 | 1 |"));
        assert!(output.contains("| `removed_markdown_fence` | 4 | 1 |"));
        assert!(output.contains("## Raw Assistant\n\n````text\n"));
        assert!(!output.contains(API_KEY));
        assert!(!output.contains(encoded_fragment));
        assert!(output.contains("before-[REDACTED API KEY]-after"));
    }

    #[test]
    fn non_thinking_canonical_fence_keeps_existing_record_shape() {
        let raw_assistant = "```json\n{\"0\":[\"严格响应\"]}\n```".to_owned();
        let parsed =
            parse_translation_response(&raw_assistant, TranslationResponseMode::new(false, false))
                .expect("规范围栏应直接解析内部 JSON");
        let record =
            GenericTaskResponseRecord::parsed_with_cancellation(raw_assistant, parsed, || {
                Ok::<_, Infallible>(())
            })
            .expect("未取消的记录投影应成功");
        let client = LlmClientRecordMetadata::new(
            "https://example.test".to_owned(),
            "model".to_owned(),
            Map::new(),
            ApiKeyRedactor::new(SecretString::from("unused-key")),
        );
        let mut output = String::new();

        render_response(
            &mut output,
            &UiLocalizer::new(UiLocale::SimplifiedChinese),
            &client,
            &record,
        )
        .expect("规范围栏的非 thinking 响应应该可渲染");

        assert!(output.contains("## Assistant\n\n### ID 0\n\n严格响应"));
        assert!(!output.contains("## JSON Repairs"));
        assert!(!output.contains("## Raw Assistant"));
    }

    #[test]
    fn invalid_response_renders_safe_raw_assistant_diagnostic() {
        const API_KEY: &str = "quote\"slash\\value";
        let encoded_api_key = serde_json::to_string(API_KEY).expect("API key 应可编码为 JSON");
        let encoded_fragment = &encoded_api_key[1..encoded_api_key.len() - 1];
        let raw_assistant = format!(
            "```malformed\n{{\"0\":[\"before-{encoded_fragment}-after\"]}}\n```\n```json\n{{\"1\":[]}}\n```"
        );
        let error =
            parse_translation_response(&raw_assistant, TranslationResponseMode::new(false, false))
                .expect_err("多个候选 JSON 应返回结构化解析错误");
        let record = GenericTaskResponseRecord::invalid(raw_assistant, error);
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
        .expect("无效响应诊断应该可渲染");

        assert!(output.contains("## Assistant"));
        assert!(output.contains("````text\n```malformed"));
        assert!(!output.contains(API_KEY));
        assert!(!output.contains(encoded_fragment));
        assert!(output.contains("[REDACTED API KEY]"));
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
                format!(
                    r#"{{"groups":[{{"units":[{{"id":"0","type":"free","text":["before-{encoded_fragment}-after"]}}]}}]}}"#
                ),
            )],
            1,
            OffsetDateTime::UNIX_EPOCH,
            Duration::ZERO,
            0,
            Vec::new(),
            Some(GenericTaskResponseRecord::unprocessed(format!(
                "prefix {{\"0\":[\"before-{encoded_fragment}-after\"]}} trailing {{"
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
