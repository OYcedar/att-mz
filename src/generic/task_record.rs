//! Generic 模型任务的最小、非权威记录投影。
//!
//! 每份记录只保留实际 User 请求、原始 Assistant 响应和最终验收结果。目录、文件名、
//! 并发写入、敏感值处理和写入故障由公共任务记录 sink 负责。

use std::fmt::Write as _;

use crate::diagnostic::{DiagnosticReport, render_diagnostic_report};
use crate::i18n::{UiLocale, UiLocalizer, UiMessage};
use crate::llm::ApiKeyRedactor;
use crate::translation::task_record::{
    TranslationTaskRecordArtifact, markdown_fence, task_record_text,
};

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
    task_index: usize,
    user_message: String,
    raw_assistant: Option<String>,
    state: GenericTaskRecordState,
}

impl GenericTaskRecordDocument {
    pub(crate) fn new(
        task_index: usize,
        user_message: String,
        raw_assistant: Option<String>,
        state: GenericTaskRecordState,
    ) -> Self {
        Self {
            task_index,
            user_message,
            raw_assistant,
            state,
        }
    }
}

impl TranslationTaskRecordArtifact for GenericTaskRecordDocument {
    fn task_index(&self) -> usize {
        self.task_index
    }

    fn render(&self, redactor: &ApiKeyRedactor, locale: UiLocale) -> String {
        render_generic_task_record(redactor, locale, self)
    }
}

fn render_generic_task_record(
    redactor: &ApiKeyRedactor,
    locale: UiLocale,
    document: &GenericTaskRecordDocument,
) -> String {
    let localizer = UiLocalizer::new(locale);
    let mut output = format!(
        "# {}\n\n## User\n\n",
        task_record_text(&localizer, UiMessage::TaskRecordTitle)
    );
    let user = redactor.redact_text_with_json_strings(&document.user_message);
    output.push_str(&user);
    if !user.ends_with('\n') {
        output.push('\n');
    }

    if let Some(raw_assistant) = &document.raw_assistant {
        output.push_str("\n## Assistant\n\n");
        let assistant = redactor.redact_text_with_json_strings(raw_assistant);
        output.push_str(&markdown_fence(&assistant, "text"));
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
        let _ = writeln!(
            output,
            "- {}",
            task_record_text(&localizer, UiMessage::TaskRecordTaskDiagnostic)
        );
        let rendered = redactor.redact(&render_diagnostic_report(diagnostic, &localizer));
        output.push_str(&markdown_fence(&rendered, "text"));
    }
    output
}

#[cfg(test)]
mod tests {
    use secrecy::SecretString;

    use super::*;

    #[test]
    fn record_keeps_only_user_assistant_and_final_result() {
        let document = GenericTaskRecordDocument::new(
            0,
            r#"{"units":[{"text":"原文"}]}"#.to_owned(),
            Some(r#"{"translations":{"0":["译文"]}}"#.to_owned()),
            GenericTaskRecordState::committed(true, 1, 1, Vec::new()),
        );
        let rendered = document.render(
            &ApiKeyRedactor::new(SecretString::from("secret")),
            UiLocale::SimplifiedChinese,
        );

        assert!(rendered.contains("## User"));
        assert!(rendered.contains("## Assistant"));
        assert!(rendered.contains("# 翻译任务"));
        assert!(rendered.contains("## 最终结果"));
        assert!(!rendered.contains("## System"));
        assert!(!rendered.contains("Attempts"));
        assert!(!rendered.contains("RunId"));
        assert!(!rendered.contains("Endpoint"));
    }

    #[test]
    fn record_redacts_user_and_raw_assistant() {
        let document = GenericTaskRecordDocument::new(
            0,
            r#"{"text":"secret"}"#.to_owned(),
            Some(r#"{"translation":"secret"}"#.to_owned()),
            GenericTaskRecordState::committed(true, 1, 1, Vec::new()),
        );
        let rendered = document.render(
            &ApiKeyRedactor::new(SecretString::from("secret")),
            UiLocale::SimplifiedChinese,
        );

        assert!(!rendered.contains("secret"));
    }
}
