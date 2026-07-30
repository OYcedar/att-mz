//! ATT 托管翻译任务共同使用的响应信封、JSON ID 与可读记录投影。
//!
//! 本模块不理解游戏引擎、持久化身份、Placeholder 或语言验收。调用方只消费一次解析
//! 建立的有序条目，并在自己的语义边界逐 ID 验收。

use std::fmt;
use std::num::NonZeroUsize;

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::json_diagnostic::JsonErrorCategory;

/// 托管翻译响应必须遵循的受信外层协议。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TranslationResponseEnvelope {
    JsonOnly,
    ThinkingThenJson,
}

/// `serde_json` 在协议边界建立的稳定错误类别。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TranslationTaskResponseJsonErrorCategory {
    Io,
    Syntax,
    Shape,
    UnexpectedEof,
}

impl TranslationTaskResponseJsonErrorCategory {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::Io => "io",
            Self::Syntax => "syntax",
            Self::Shape => "shape",
            Self::UnexpectedEof => "unexpected_eof",
        }
    }
}

/// Assistant 响应无法按当前受信协议解析时的结构化类别。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TranslationTaskResponseParseErrorKind {
    Json(TranslationTaskResponseJsonErrorCategory),
    ThinkingNotAllowed,
    ThinkingEnvelopeMissing,
    ThinkingEnvelopeUnclosed,
    ThinkingEmpty,
    ThinkingNested,
    ThinkingRepeated,
}

impl TranslationTaskResponseParseErrorKind {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::Json(_) => "json",
            Self::ThinkingNotAllowed => "thinking_not_allowed",
            Self::ThinkingEnvelopeMissing => "thinking_envelope_missing",
            Self::ThinkingEnvelopeUnclosed => "thinking_envelope_unclosed",
            Self::ThinkingEmpty => "thinking_empty",
            Self::ThinkingNested => "thinking_nested",
            Self::ThinkingRepeated => "thinking_repeated",
        }
    }
}

/// 相对于完整原始 Assistant 的一基解析位置及其稳定类别。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TranslationTaskResponseParseError {
    pub(crate) kind: TranslationTaskResponseParseErrorKind,
    pub(crate) line: NonZeroUsize,
    pub(crate) column: NonZeroUsize,
}

impl TranslationTaskResponseParseError {
    pub(crate) const fn new(
        kind: TranslationTaskResponseParseErrorKind,
        line: NonZeroUsize,
        column: NonZeroUsize,
    ) -> Self {
        Self { kind, line, column }
    }

    pub(crate) const fn kind(self) -> TranslationTaskResponseParseErrorKind {
        self.kind
    }

    pub(crate) const fn line(self) -> NonZeroUsize {
        self.line
    }

    pub(crate) const fn column(self) -> NonZeroUsize {
        self.column
    }

    pub(crate) fn business_message(self) -> String {
        let message = match self.kind {
            TranslationTaskResponseParseErrorKind::Json(category) => {
                return format!(
                    "模型响应 JSON 无效：类别 {}，第 {} 行、第 {} 列",
                    category.code(),
                    self.line,
                    self.column
                );
            }
            TranslationTaskResponseParseErrorKind::ThinkingNotAllowed => {
                "当前响应模式不接受思考输出"
            }
            TranslationTaskResponseParseErrorKind::ThinkingEnvelopeMissing => {
                "模型响应缺少规定的思考信封"
            }
            TranslationTaskResponseParseErrorKind::ThinkingEnvelopeUnclosed => {
                "模型响应的思考信封没有闭合"
            }
            TranslationTaskResponseParseErrorKind::ThinkingEmpty => "模型响应的思考内容为空",
            TranslationTaskResponseParseErrorKind::ThinkingNested => "模型响应包含嵌套的思考信封",
            TranslationTaskResponseParseErrorKind::ThinkingRepeated => "模型响应包含重复的思考信封",
        };
        format!("{message}，第 {} 行、第 {} 列", self.line, self.column)
    }
}

/// Assistant JSON 中保持原始顺序和重复项的一个条目。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ParsedTranslationAssistantEntry {
    id: String,
    value: Value,
    canonical_id: Option<usize>,
}

impl ParsedTranslationAssistantEntry {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) const fn canonical_id(&self) -> Option<usize> {
        self.canonical_id
    }

    pub(crate) fn value(&self) -> &Value {
        &self.value
    }

    pub(crate) fn into_parts(self) -> (String, Value, Option<usize>) {
        (self.id, self.value, self.canonical_id)
    }
}

/// 唯一响应解析器建立的完整投影。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ParsedTranslationResponse {
    thinking: Option<String>,
    entries: Vec<ParsedTranslationAssistantEntry>,
}

impl ParsedTranslationResponse {
    #[cfg(test)]
    pub(crate) fn thinking(&self) -> Option<&str> {
        self.thinking.as_deref()
    }

    pub(crate) fn entries(&self) -> &[ParsedTranslationAssistantEntry] {
        &self.entries
    }

    pub(crate) fn into_parts(self) -> (Option<String>, Vec<ParsedTranslationAssistantEntry>) {
        (self.thinking, self.entries)
    }
}

#[derive(Clone, Debug)]
struct ModelOutputWire {
    id: String,
    value: Value,
}

#[derive(Debug)]
struct ModelOutputBatch(Vec<ModelOutputWire>);

impl<'de> Deserialize<'de> for ModelOutputBatch {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(ModelOutputBatchVisitor)
    }
}

struct ModelOutputBatchVisitor;

impl<'de> Visitor<'de> for ModelOutputBatchVisitor {
    type Value = ModelOutputBatch;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("以数字 ID 为键的 JSON 对象")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut outputs = Vec::with_capacity(map.size_hint().unwrap_or_default());
        while let Some((id, value)) = map.next_entry::<String, Value>()? {
            outputs.push(ModelOutputWire { id, value });
        }
        Ok(ModelOutputBatch(outputs))
    }
}

/// 解析 ATT 托管翻译任务的唯一 Assistant wire。
pub(crate) fn parse_translation_response(
    value: &str,
    response_envelope: TranslationResponseEnvelope,
) -> Result<ParsedTranslationResponse, TranslationTaskResponseParseError> {
    let value = trim_model_response(value);
    let envelope = parse_translation_response_envelope(value, response_envelope)?;
    let value = envelope.assistant_json.trim();
    serde_json::from_str::<ModelOutputBatch>(value.value)
        .map(|batch| ParsedTranslationResponse {
            thinking: envelope.thinking.map(str::to_owned),
            entries: batch
                .0
                .into_iter()
                .map(|output| ParsedTranslationAssistantEntry {
                    canonical_id: parse_model_output_id(&output.id),
                    id: output.id,
                    value: output.value,
                })
                .collect(),
        })
        .map_err(|source| {
            let category = match JsonErrorCategory::from(&source) {
                JsonErrorCategory::Io => TranslationTaskResponseJsonErrorCategory::Io,
                JsonErrorCategory::Syntax | JsonErrorCategory::DuplicateObjectKey => {
                    TranslationTaskResponseJsonErrorCategory::Syntax
                }
                JsonErrorCategory::Data => TranslationTaskResponseJsonErrorCategory::Shape,
                JsonErrorCategory::Eof => TranslationTaskResponseJsonErrorCategory::UnexpectedEof,
            };
            let (line, column) = if matches!(
                category,
                TranslationTaskResponseJsonErrorCategory::UnexpectedEof
            ) {
                value.location_at(value.value.len())
            } else {
                value.location_for_local(source.line(), source.column())
            };
            TranslationTaskResponseParseError::new(
                TranslationTaskResponseParseErrorKind::Json(category),
                line,
                column,
            )
        })
}

fn parse_model_output_id(value: &str) -> Option<usize> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || value.starts_with('0')
    {
        return None;
    }
    value.parse().ok()
}

#[derive(Clone, Copy)]
struct LocatedModelResponse<'a> {
    raw: &'a str,
    value: &'a str,
    start: usize,
}

impl<'a> LocatedModelResponse<'a> {
    fn new(raw: &'a str) -> Self {
        Self {
            raw,
            value: raw,
            start: 0,
        }
    }

    fn advance(self, bytes: usize) -> Self {
        Self {
            raw: self.raw,
            value: &self.value[bytes..],
            start: self.start + bytes,
        }
    }

    fn prefix(self, bytes: usize) -> Self {
        Self {
            raw: self.raw,
            value: &self.value[..bytes],
            start: self.start,
        }
    }

    fn trim(self) -> Self {
        let leading = self.value.len() - self.value.trim_start().len();
        self.advance(leading).prefix(self.value.trim().len())
    }

    fn trim_start(self) -> Self {
        let leading = self.value.len() - self.value.trim_start().len();
        self.advance(leading)
    }

    fn location_at(self, local_byte_offset: usize) -> (NonZeroUsize, NonZeroUsize) {
        response_location(self.raw, self.start + local_byte_offset)
    }

    fn location_for_local(
        self,
        local_line: usize,
        local_column: usize,
    ) -> (NonZeroUsize, NonZeroUsize) {
        let (start_line, start_column) = response_location(self.raw, self.start);
        let local_line = local_line.max(1);
        let local_column = local_column.max(1);
        if local_line == 1 {
            (
                start_line,
                NonZeroUsize::new(start_column.get() + local_column - 1)
                    .expect("一基列号相加后仍非零"),
            )
        } else {
            (
                NonZeroUsize::new(start_line.get() + local_line - 1).expect("一基行号相加后仍非零"),
                NonZeroUsize::new(local_column).expect("局部列号已收窄为至少一"),
            )
        }
    }

    fn error_at(
        self,
        kind: TranslationTaskResponseParseErrorKind,
        local_byte_offset: usize,
    ) -> TranslationTaskResponseParseError {
        let (line, column) = self.location_at(local_byte_offset);
        TranslationTaskResponseParseError::new(kind, line, column)
    }

    fn error_at_raw_eof(
        self,
        kind: TranslationTaskResponseParseErrorKind,
    ) -> TranslationTaskResponseParseError {
        let (line, column) = response_location(self.raw, self.raw.len());
        TranslationTaskResponseParseError::new(kind, line, column)
    }
}

fn response_location(raw: &str, byte_offset: usize) -> (NonZeroUsize, NonZeroUsize) {
    let byte_offset = byte_offset.min(raw.len());
    let preceding = &raw[..byte_offset];
    let line = preceding.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = preceding
        .rsplit_once('\n')
        .map_or(preceding.len(), |(_, tail)| tail.len())
        + 1;
    (
        NonZeroUsize::new(line).expect("一基行号不可能为零"),
        NonZeroUsize::new(column).expect("一基列号不可能为零"),
    )
}

fn trim_model_response(value: &str) -> LocatedModelResponse<'_> {
    let value = LocatedModelResponse::new(value).trim();
    let value = if value.value.starts_with('\u{feff}') {
        value.advance('\u{feff}'.len_utf8())
    } else {
        value
    };
    value.trim()
}

fn parse_translation_response_envelope(
    value: LocatedModelResponse<'_>,
    response_envelope: TranslationResponseEnvelope,
) -> Result<TranslationResponseEnvelopeParts<'_>, TranslationTaskResponseParseError> {
    match response_envelope {
        TranslationResponseEnvelope::JsonOnly => {
            if starts_with_thinking_tag(value.value) {
                return Err(
                    value.error_at(TranslationTaskResponseParseErrorKind::ThinkingNotAllowed, 0)
                );
            }
            Ok(TranslationResponseEnvelopeParts {
                thinking: None,
                assistant_json: value,
            })
        }
        TranslationResponseEnvelope::ThinkingThenJson => parse_thinking_then_json(value),
    }
}

struct TranslationResponseEnvelopeParts<'a> {
    thinking: Option<&'a str>,
    assistant_json: LocatedModelResponse<'a>,
}

fn parse_thinking_then_json(
    value: LocatedModelResponse<'_>,
) -> Result<TranslationResponseEnvelopeParts<'_>, TranslationTaskResponseParseError> {
    let Some(after_opening) = value.value.strip_prefix("<why>") else {
        return Err(value.error_at(
            TranslationTaskResponseParseErrorKind::ThinkingEnvelopeMissing,
            0,
        ));
    };
    let after_opening = value.advance(value.value.len() - after_opening.len());
    let Some(closing_start) = after_opening.value.find("</why>") else {
        return Err(
            value.error_at_raw_eof(TranslationTaskResponseParseErrorKind::ThinkingEnvelopeUnclosed)
        );
    };
    let thinking = after_opening.prefix(closing_start);
    if thinking.value.trim().is_empty() {
        return Err(after_opening.error_at(
            TranslationTaskResponseParseErrorKind::ThinkingEmpty,
            closing_start,
        ));
    }
    if let Some(offending) = first_thinking_tag(thinking.value) {
        return Err(thinking.error_at(
            TranslationTaskResponseParseErrorKind::ThinkingNested,
            offending,
        ));
    }

    let json = after_opening
        .advance(closing_start + "</why>".len())
        .trim_start();
    if starts_with_thinking_tag(json.value) {
        return Err(json.error_at(TranslationTaskResponseParseErrorKind::ThinkingRepeated, 0));
    }
    Ok(TranslationResponseEnvelopeParts {
        thinking: Some(thinking.value),
        assistant_json: json,
    })
}

fn starts_with_thinking_tag(value: &str) -> bool {
    value.starts_with("<why>") || value.starts_with("</why>")
}

fn first_thinking_tag(value: &str) -> Option<usize> {
    ["<why>", "</why>"]
        .into_iter()
        .filter_map(|tag| value.find(tag))
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_order_duplicates_and_invalid_ids() {
        let parsed = parse_translation_response(
            r#"{"1":["甲"],"bad":["乙"],"1":["丙"],"02":["丁"]}"#,
            TranslationResponseEnvelope::JsonOnly,
        )
        .expect("合法对象应解析");

        assert_eq!(
            parsed
                .entries()
                .iter()
                .map(|entry| (entry.id(), entry.canonical_id()))
                .collect::<Vec<_>>(),
            [("1", Some(1)), ("bad", None), ("1", Some(1)), ("02", None)]
        );
    }

    #[test]
    fn thinking_envelope_is_exact_and_not_business_state() {
        let parsed = parse_translation_response(
            "<why>逐项检查</why>\n{\"1\":[\"译文\"]}",
            TranslationResponseEnvelope::ThinkingThenJson,
        )
        .expect("合法 thinking 信封应解析");
        assert_eq!(parsed.thinking(), Some("逐项检查"));
        assert_eq!(parsed.entries()[0].value(), &serde_json::json!(["译文"]));

        let error = parse_translation_response(
            "<why>不应出现</why>{\"1\":[\"译文\"]}",
            TranslationResponseEnvelope::JsonOnly,
        )
        .expect_err("JSON-only 不接受 thinking");
        assert_eq!(
            error.kind(),
            TranslationTaskResponseParseErrorKind::ThinkingNotAllowed
        );
    }

    #[test]
    fn reports_shape_and_full_response_location() {
        let parsed = parse_translation_response(
            "\n{\"1\":[\"ok\"],\"2\":true}\n",
            TranslationResponseEnvelope::JsonOnly,
        )
        .expect("值形状由逐 ID 验收");
        assert_eq!(parsed.entries()[1].value(), &serde_json::Value::Bool(true));

        let error = parse_translation_response(
            "\n<why>ok</why>\n{\"1\":",
            TranslationResponseEnvelope::ThinkingThenJson,
        )
        .expect_err("截断 JSON 必须失败");
        assert_eq!(error.line().get(), 3);
        assert!(error.column().get() >= 6);

        let fenced = parse_translation_response(
            "```json\n{\"1\":[\"ok\"]}\n```",
            TranslationResponseEnvelope::JsonOnly,
        )
        .expect_err("当前协议只接受裸 JSON，不接受 Markdown 围栏");
        assert!(matches!(
            fenced.kind(),
            TranslationTaskResponseParseErrorKind::Json(
                TranslationTaskResponseJsonErrorCategory::Syntax
            )
        ));
    }
}
