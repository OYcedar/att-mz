//! ATT 托管翻译任务共同使用的响应模式、JSON ID 与可读记录投影。
//!
//! 本模块不理解游戏引擎、持久化身份、Placeholder 或语言验收。调用方只消费一次解析
//! 建立的有序条目，并在自己的语义边界逐 ID 验收。

#[cfg(test)]
use std::convert::Infallible;
use std::fmt;
use std::io::{self, BufReader, Read};
use std::num::NonZeroUsize;

use att_json_repair::{RepairError, RepairOutput, repair_with_cancellation};
use serde::de::{DeserializeOwned, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;

use crate::json_diagnostic::JsonErrorCategory;
use crate::translation::task_planning::TaskId;

/// 托管翻译响应必须遵循的两个独立输出事实。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TranslationResponseMode {
    thinking: bool,
    source_echo: bool,
}

impl TranslationResponseMode {
    pub(crate) const fn new(thinking: bool, source_echo: bool) -> Self {
        Self {
            thinking,
            source_echo,
        }
    }

    pub(crate) const fn thinking(self) -> bool {
        self.thinking
    }

    pub(crate) const fn source_echo(self) -> bool {
        self.source_echo
    }
}

/// `serde_json` 在协议边界建立的稳定错误类别。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TranslationTaskResponseJsonErrorCategory {
    Io,
    Syntax,
    Shape,
    UnexpectedEof,
}

/// Assistant 响应无法按当前受信协议解析时的结构化类别。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TranslationTaskResponseParseErrorKind {
    Json(TranslationTaskResponseJsonErrorCategory),
    ThinkingEmpty,
}

#[cfg(test)]
impl TranslationTaskResponseParseErrorKind {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::Json(_) => "json",
            Self::ThinkingEmpty => "thinking_empty",
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
}

/// Assistant JSON 中保持原始顺序和重复项的一个条目。
#[derive(Debug)]
pub(crate) struct ParsedTranslationAssistantEntry {
    value: Box<RawValue>,
    canonical_id: Option<TaskId>,
    source_echo: bool,
}

impl ParsedTranslationAssistantEntry {
    pub(crate) const fn canonical_id(&self) -> Option<TaskId> {
        self.canonical_id
    }

    pub(crate) fn raw_value(&self) -> &RawValue {
        self.value.as_ref()
    }

    /// 按当前响应模式解码一个 ID 的译文值，并保留逐字段形状错误。
    ///
    /// 原文回显是配置明确选择的审阅输出，只供人工或 agent 排查，不参与译文关联。
    /// 这里故意要求它存在且为字符串数组，但不比较它和请求原文的内容。
    pub(crate) fn decode_translation_value_with_cancellation<E>(
        &self,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<DecodedTranslationAssistantValue, E> {
        if !self.source_echo {
            return Ok(DecodedTranslationAssistantValue::Translation(
                decode_json_string_array_with_cancellation(self.value.get(), &mut ensure_running)?,
            ));
        }

        let fields = match deserialize_json_with_cancellation::<SourceEchoObject, _>(
            self.value.get(),
            &mut ensure_running,
        )? {
            Ok(fields) => fields,
            Err(_) => {
                return Ok(DecodedTranslationAssistantValue::SourceEcho(
                    DecodedSourceEchoValue::NotObject,
                ));
            }
        };
        let mut source = None;
        let mut translation = None;
        for field in fields.0 {
            ensure_running()?;
            match field.name.as_str() {
                "source" => {
                    if source.replace(field.value).is_some() {
                        return Ok(DecodedTranslationAssistantValue::SourceEcho(
                            DecodedSourceEchoValue::InvalidFields(
                                DecodedSourceEchoFieldsError::DuplicateSource,
                            ),
                        ));
                    }
                }
                "translation" => {
                    if translation.replace(field.value).is_some() {
                        return Ok(DecodedTranslationAssistantValue::SourceEcho(
                            DecodedSourceEchoValue::InvalidFields(
                                DecodedSourceEchoFieldsError::DuplicateTranslation,
                            ),
                        ));
                    }
                }
                _ => {
                    return Ok(DecodedTranslationAssistantValue::SourceEcho(
                        DecodedSourceEchoValue::InvalidFields(
                            DecodedSourceEchoFieldsError::UnexpectedField { field: field.name },
                        ),
                    ));
                }
            }
        }

        let Some(source) = source else {
            return Ok(DecodedTranslationAssistantValue::SourceEcho(
                DecodedSourceEchoValue::InvalidFields(DecodedSourceEchoFieldsError::MissingSource),
            ));
        };
        let Some(translation) = translation else {
            return Ok(DecodedTranslationAssistantValue::SourceEcho(
                DecodedSourceEchoValue::InvalidFields(
                    DecodedSourceEchoFieldsError::MissingTranslation,
                ),
            ));
        };
        let source = decode_json_string_array_with_cancellation(source.get(), &mut ensure_running)?;
        let translation =
            decode_json_string_array_with_cancellation(translation.get(), &mut ensure_running)?;
        Ok(DecodedTranslationAssistantValue::SourceEcho(
            DecodedSourceEchoValue::Fields {
                source,
                translation,
            },
        ))
    }

    pub(crate) fn into_parts(self) -> (Box<RawValue>, Option<TaskId>) {
        (self.value, self.canonical_id)
    }
}

/// 已由公共响应边界确认语法正确的逐 ID JSON 值，其字符串数组形状。
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum DecodedJsonStringArray {
    NotArray,
    NonStringItem { item: NonZeroUsize },
    Strings(Vec<String>),
}

/// 一个逐 ID 值按当前 plain/source-echo 模式建立的公共投影。
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum DecodedTranslationAssistantValue {
    Translation(DecodedJsonStringArray),
    SourceEcho(DecodedSourceEchoValue),
}

/// 原文回显对象的结构和两个数组字段。
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum DecodedSourceEchoValue {
    NotObject,
    InvalidFields(DecodedSourceEchoFieldsError),
    Fields {
        source: DecodedJsonStringArray,
        translation: DecodedJsonStringArray,
    },
}

/// 原文回显对象没有严格包含一次 `source` 和一次 `translation` 时的原因。
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum DecodedSourceEchoFieldsError {
    MissingSource,
    MissingTranslation,
    DuplicateSource,
    DuplicateTranslation,
    UnexpectedField { field: String },
}

/// 唯一响应解析器建立的完整投影。
#[derive(Debug)]
pub(crate) struct ParsedTranslationResponse {
    entries: Vec<ParsedTranslationAssistantEntry>,
}

impl ParsedTranslationResponse {
    pub(crate) fn entries(&self) -> &[ParsedTranslationAssistantEntry] {
        &self.entries
    }

    pub(crate) fn into_entries(self) -> Vec<ParsedTranslationAssistantEntry> {
        self.entries
    }
}

#[derive(Debug)]
struct ModelOutputWire {
    id: String,
    value: Box<RawValue>,
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
        while let Some((id, value)) = map.next_entry::<String, Box<RawValue>>()? {
            outputs.push(ModelOutputWire { id, value });
        }
        Ok(ModelOutputBatch(outputs))
    }
}

/// thinking 模式唯一允许的根对象。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThinkingModelOutputBatch {
    think: Box<RawValue>,
    translations: ModelOutputBatch,
}

#[derive(Debug)]
enum TranslationResponseWire {
    Plain(ModelOutputBatch),
    Thinking(ThinkingModelOutputBatch),
}

#[derive(Debug)]
struct SourceEchoField {
    name: String,
    value: Box<RawValue>,
}

#[derive(Debug)]
struct SourceEchoObject(Vec<SourceEchoField>);

impl<'de> Deserialize<'de> for SourceEchoObject {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(SourceEchoObjectVisitor)
    }
}

struct SourceEchoObjectVisitor;

impl<'de> Visitor<'de> for SourceEchoObjectVisitor {
    type Value = SourceEchoObject;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("只包含 source 和 translation 的 JSON 对象")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut fields = Vec::with_capacity(map.size_hint().unwrap_or_default());
        while let Some((name, value)) = map.next_entry::<String, Box<RawValue>>()? {
            fields.push(SourceEchoField { name, value });
        }
        Ok(SourceEchoObject(fields))
    }
}

/// 解析 ATT 托管翻译任务的唯一 Assistant wire。
#[cfg(test)]
pub(crate) fn parse_translation_response(
    value: &str,
    response_mode: TranslationResponseMode,
) -> Result<ParsedTranslationResponse, TranslationTaskResponseParseError> {
    match parse_translation_response_with_cancellation(value, response_mode, || {
        Ok::<_, Infallible>(())
    }) {
        Ok(result) => result,
        Err(unreachable) => match unreachable {},
    }
}

/// 解析 Assistant wire，并在所有可由本模块控制的长扫描之间轮询调用方。
///
/// `serde_json` 通过分块 `BufReader` 读取，避免一次 `from_str` 把任意长 JSON 变成
/// 不可观察取消的单次调用。返回外层错误表示调用方取消，内层错误保持现有 wire 契约。
pub(crate) fn parse_translation_response_with_cancellation<E>(
    value: &str,
    response_mode: TranslationResponseMode,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Result<ParsedTranslationResponse, TranslationTaskResponseParseError>, E> {
    ensure_running()?;
    let value = unwrap_translation_response_json_fence_with_cancellation(
        LocatedModelResponse::new(value),
        &mut ensure_running,
    )?;
    let strict_error = match deserialize_translation_response_wire_with_cancellation(
        value.value,
        response_mode,
        &mut ensure_running,
    )? {
        Ok(wire) => {
            return finish_translation_response_with_cancellation(
                wire,
                response_mode,
                value,
                0,
                &mut ensure_running,
            );
        }
        Err(source) => source,
    };

    if !matches!(
        JsonErrorCategory::from(&strict_error),
        JsonErrorCategory::Syntax | JsonErrorCategory::Eof
    ) {
        return Ok(Err(translation_response_json_error_with_cancellation(
            value,
            strict_error,
            &mut ensure_running,
        )?));
    }

    let repaired = match repair_with_cancellation(value.value, &mut ensure_running)? {
        Ok(repaired) => repaired,
        Err(repair_error) => {
            return Ok(Err(translation_response_repair_error_with_cancellation(
                value,
                &strict_error,
                repair_error,
                &mut ensure_running,
            )?));
        }
    };
    let repaired_wire = match deserialize_translation_response_wire_with_cancellation(
        repaired.json(),
        response_mode,
        &mut ensure_running,
    )? {
        Ok(wire) => wire,
        Err(source) => {
            return Ok(Err(
                repaired_translation_response_json_error_with_cancellation(
                    value,
                    &repaired,
                    source,
                    &mut ensure_running,
                )?,
            ));
        }
    };
    let root_original_offset = repaired.original_offset(0).unwrap_or_default();
    finish_translation_response_with_cancellation(
        repaired_wire,
        response_mode,
        value,
        root_original_offset,
        &mut ensure_running,
    )
}

/// 识别正文中唯一的规范 `json` Markdown 围栏，并把后续解析范围收窄到围栏内部。
///
/// 围栏只是模型协议允许的外层表示，不属于 JSON 修复。其他标签、围栏外正文、缺少结束
/// 围栏或多个代码块仍交给严格解析和保守修复决定结果。
fn unwrap_translation_response_json_fence_with_cancellation<'a, E>(
    response: LocatedModelResponse<'a>,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<LocatedModelResponse<'a>, E> {
    const OPENING: &[u8] = b"```json";
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    let bytes = response.value.as_bytes();
    let opening_start = skip_json_whitespace_with_cancellation(bytes, 0, ensure_running)?;
    if !bytes
        .get(opening_start..)
        .is_some_and(|tail| tail.starts_with(OPENING))
    {
        return Ok(response);
    }

    let mut cursor = opening_start + OPENING.len();
    let mut opening_whitespace = 0_usize;
    while bytes
        .get(cursor)
        .is_some_and(|byte| matches!(*byte, b' ' | b'\t'))
    {
        cursor += 1;
        opening_whitespace += 1;
        if opening_whitespace.is_multiple_of(CANCELLATION_CHECK_BYTES) {
            ensure_running()?;
        }
    }
    let content_start = match bytes.get(cursor) {
        Some(b'\n') => cursor + 1,
        Some(b'\r') if bytes.get(cursor + 1) == Some(&b'\n') => cursor + 2,
        _ => return Ok(response),
    };

    let mut line_start = content_start;
    let mut scanned = 0_usize;
    loop {
        let mut line_end = line_start;
        while bytes.get(line_end).is_some_and(|byte| *byte != b'\n') {
            line_end += 1;
            scanned += 1;
            if scanned.is_multiple_of(CANCELLATION_CHECK_BYTES) {
                ensure_running()?;
            }
        }
        let next_line_start = if line_end < bytes.len() {
            line_end + 1
        } else {
            line_end
        };
        if is_translation_response_json_fence_closing_line(
            &bytes[line_start..line_end],
            ensure_running,
        )? {
            let trailing =
                skip_json_whitespace_with_cancellation(bytes, next_line_start, ensure_running)?;
            if trailing == bytes.len() {
                return Ok(response.subslice(content_start, line_start));
            }
            return Ok(response);
        }
        if line_end == bytes.len() {
            break;
        }
        line_start = next_line_start;
    }
    ensure_running()?;
    Ok(response)
}

fn is_translation_response_json_fence_closing_line<E>(
    line: &[u8],
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<bool, E> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    let mut start = 0_usize;
    let mut end = line.len();
    if line.ends_with(b"\r") {
        end -= 1;
    }
    while line
        .get(start)
        .is_some_and(|byte| matches!(*byte, b' ' | b'\t'))
    {
        start += 1;
        if start.is_multiple_of(CANCELLATION_CHECK_BYTES) {
            ensure_running()?;
        }
    }
    let mut scanned = 0_usize;
    while end > start
        && line
            .get(end - 1)
            .is_some_and(|byte| matches!(*byte, b' ' | b'\t'))
    {
        end -= 1;
        scanned += 1;
        if scanned.is_multiple_of(CANCELLATION_CHECK_BYTES) {
            ensure_running()?;
        }
    }
    ensure_running()?;
    Ok(&line[start..end] == b"```")
}

fn deserialize_translation_response_wire_with_cancellation<E>(
    value: &str,
    response_mode: TranslationResponseMode,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Result<TranslationResponseWire, serde_json::Error>, E> {
    if response_mode.thinking() {
        return Ok(
            deserialize_json_with_cancellation::<ThinkingModelOutputBatch, _>(
                value,
                ensure_running,
            )?
            .map(TranslationResponseWire::Thinking),
        );
    }
    Ok(
        deserialize_model_output_batch_with_cancellation(value, ensure_running)?
            .map(TranslationResponseWire::Plain),
    )
}

fn finish_translation_response_with_cancellation<E>(
    wire: TranslationResponseWire,
    response_mode: TranslationResponseMode,
    source: LocatedModelResponse<'_>,
    root_original_offset: usize,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Result<ParsedTranslationResponse, TranslationTaskResponseParseError>, E> {
    let batch = match wire {
        TranslationResponseWire::Thinking(wrapper) => {
            // thinking_output 是显式选择的响应契约：模型必须返回非空判断，但业务只消费
            // 译文；需要审阅时由任务记录保留原始 Assistant，不在解析结果中重复持有。
            let Some(thinking) =
                decode_owned_json_string_with_cancellation(wrapper.think.get(), ensure_running)?
            else {
                return Ok(Err(source.error_at_with_cancellation(
                    TranslationTaskResponseParseErrorKind::Json(
                        TranslationTaskResponseJsonErrorCategory::Shape,
                    ),
                    root_original_offset,
                    ensure_running,
                )?));
            };
            if response_text_is_whitespace_with_cancellation(&thinking, ensure_running)? {
                return Ok(Err(source.error_at_with_cancellation(
                    TranslationTaskResponseParseErrorKind::ThinkingEmpty,
                    root_original_offset,
                    ensure_running,
                )?));
            }
            wrapper.translations
        }
        TranslationResponseWire::Plain(batch) => batch,
    };
    let mut entries = Vec::with_capacity(batch.0.len());
    for output in batch.0 {
        ensure_running()?;
        let canonical_id = parse_model_output_id_with_cancellation(&output.id, ensure_running)?;
        entries.push(ParsedTranslationAssistantEntry {
            canonical_id,
            value: output.value,
            source_echo: response_mode.source_echo(),
        });
    }
    ensure_running()?;
    Ok(Ok(ParsedTranslationResponse { entries }))
}

fn translation_response_json_error_with_cancellation<E>(
    value: LocatedModelResponse<'_>,
    source: serde_json::Error,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<TranslationTaskResponseParseError, E> {
    let category = translation_response_json_error_category(&source);
    let (line, column) = if matches!(
        category,
        TranslationTaskResponseJsonErrorCategory::UnexpectedEof
    ) {
        value.location_at_with_cancellation(value.value.len(), ensure_running)?
    } else {
        value.location_for_local_with_cancellation(
            source.line(),
            source.column(),
            ensure_running,
        )?
    };
    Ok(TranslationTaskResponseParseError::new(
        TranslationTaskResponseParseErrorKind::Json(category),
        line,
        column,
    ))
}

fn repaired_translation_response_json_error_with_cancellation<E>(
    original: LocatedModelResponse<'_>,
    repaired: &RepairOutput<'_>,
    source: serde_json::Error,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<TranslationTaskResponseParseError, E> {
    let category = translation_response_json_error_category(&source);
    let output_offset = if matches!(
        category,
        TranslationTaskResponseJsonErrorCategory::UnexpectedEof
    ) {
        repaired.json().len()
    } else {
        json_location_byte_offset_with_cancellation(
            repaired.json(),
            source.line(),
            source.column(),
            ensure_running,
        )?
    };
    let original_offset = repaired
        .original_offset(output_offset)
        .unwrap_or(original.value.len());
    let (line, column) = original.location_at_with_cancellation(original_offset, ensure_running)?;
    Ok(TranslationTaskResponseParseError::new(
        TranslationTaskResponseParseErrorKind::Json(category),
        line,
        column,
    ))
}

fn translation_response_repair_error_with_cancellation<E>(
    original: LocatedModelResponse<'_>,
    strict_source: &serde_json::Error,
    repair_error: RepairError,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<TranslationTaskResponseParseError, E> {
    let category = translation_response_json_error_category(strict_source);
    let (line, column) =
        original.location_at_with_cancellation(repair_error.original_offset(), ensure_running)?;
    Ok(TranslationTaskResponseParseError::new(
        TranslationTaskResponseParseErrorKind::Json(category),
        line,
        column,
    ))
}

fn translation_response_json_error_category(
    source: &serde_json::Error,
) -> TranslationTaskResponseJsonErrorCategory {
    match JsonErrorCategory::from(source) {
        JsonErrorCategory::Io => TranslationTaskResponseJsonErrorCategory::Io,
        JsonErrorCategory::Syntax | JsonErrorCategory::DuplicateObjectKey => {
            TranslationTaskResponseJsonErrorCategory::Syntax
        }
        JsonErrorCategory::Data => TranslationTaskResponseJsonErrorCategory::Shape,
        JsonErrorCategory::Eof => TranslationTaskResponseJsonErrorCategory::UnexpectedEof,
    }
}

fn json_location_byte_offset_with_cancellation<E>(
    json: &str,
    line: usize,
    column: usize,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<usize, E> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    let target_line = line.max(1);
    let target_column = column.max(1);
    let mut current_line = 1_usize;
    let mut current_column = 1_usize;
    for (offset, byte) in json.bytes().enumerate() {
        if offset.is_multiple_of(CANCELLATION_CHECK_BYTES) {
            ensure_running()?;
        }
        if current_line == target_line && current_column == target_column {
            return Ok(offset);
        }
        if byte == b'\n' {
            current_line += 1;
            current_column = 1;
        } else {
            current_column += 1;
        }
    }
    ensure_running()?;
    Ok(json.len())
}

fn parse_model_output_id_with_cancellation<E>(
    value: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Option<TaskId>, E> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    ensure_running()?;
    if value.is_empty() || (value.len() > 1 && value.starts_with('0')) {
        return Ok(None);
    }
    let mut parsed = 0_usize;
    for (index, byte) in value.bytes().enumerate() {
        if index.is_multiple_of(CANCELLATION_CHECK_BYTES) {
            ensure_running()?;
        }
        if !byte.is_ascii_digit() {
            return Ok(None);
        }
        let Some(next) = parsed
            .checked_mul(10)
            .and_then(|current| current.checked_add(usize::from(byte - b'0')))
        else {
            return Ok(None);
        };
        parsed = next;
    }
    ensure_running()?;
    Ok(Some(TaskId::new(parsed)))
}

fn deserialize_model_output_batch_with_cancellation<E>(
    value: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Result<ModelOutputBatch, serde_json::Error>, E> {
    deserialize_json_with_cancellation(value, ensure_running)
}

/// 如果 `value` 是 JSON string，就直接分段建立拥有型 UTF-8 文本。
///
/// 调用方只会传入已经由 `RawValue` 证明有效的 JSON。本函数仍在发现不符合该不变量的
/// 结构时返回 `None`，让通用 serde_json 路径产生原有错误，而不是在内部不变量失效时
/// 猜测结果。每次复制到输出的原始片段都不超过一个取消检查窗口。
fn decode_owned_json_string_with_cancellation<E>(
    value: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Option<String>, E> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    ensure_running()?;
    let bytes = value.as_bytes();
    let mut opening = 0_usize;
    while opening < bytes.len() && is_json_whitespace(bytes[opening]) {
        opening += 1;
        if opening.is_multiple_of(CANCELLATION_CHECK_BYTES) {
            ensure_running()?;
        }
    }
    if bytes.get(opening) != Some(&b'"') {
        return Ok(None);
    }

    let mut output = String::with_capacity(bytes.len().saturating_sub(opening + 2));
    let mut cursor = opening + 1;
    let mut literal_start = cursor;
    let mut next_check = cursor.saturating_add(CANCELLATION_CHECK_BYTES);
    loop {
        if cursor >= next_check && value.is_char_boundary(cursor) {
            output.push_str(&value[literal_start..cursor]);
            literal_start = cursor;
            ensure_running()?;
            next_check = cursor.saturating_add(CANCELLATION_CHECK_BYTES);
        }

        let Some(&byte) = bytes.get(cursor) else {
            return Ok(None);
        };
        match byte {
            b'"' => {
                output.push_str(&value[literal_start..cursor]);
                cursor += 1;
                if contains_non_json_whitespace_with_cancellation(&bytes[cursor..], ensure_running)?
                {
                    return Ok(None);
                }
                ensure_running()?;
                return Ok(Some(output));
            }
            b'\\' => {
                output.push_str(&value[literal_start..cursor]);
                let Some((escaped, next_cursor)) = decode_json_escape(bytes, cursor) else {
                    return Ok(None);
                };
                output.push(escaped);
                cursor = next_cursor;
                literal_start = cursor;
            }
            0x00..=0x1f => return Ok(None),
            _ => cursor += 1,
        }
    }
}

fn decode_json_string_array_with_cancellation<E>(
    value: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<DecodedJsonStringArray, E> {
    ensure_running()?;
    let shape = inspect_json_string_array_with_cancellation(value, ensure_running)?;
    let JsonStringArrayShape::Strings { count } = shape else {
        return Ok(match shape {
            JsonStringArrayShape::NotArray => DecodedJsonStringArray::NotArray,
            JsonStringArrayShape::NonStringItem { item } => {
                DecodedJsonStringArray::NonStringItem { item }
            }
            JsonStringArrayShape::Strings { .. } => unreachable!(),
        });
    };

    // 第一遍已经得到精确项数。这里的单次分配不会搬移已有 String，后续 push 也不会
    // 触发几何扩容；分配前后都轮询，保证取消不会被后续解码工作吞掉。
    ensure_running()?;
    let mut strings = Vec::with_capacity(count);
    ensure_running()?;

    let bytes = value.as_bytes();
    let mut cursor = skip_json_whitespace_with_cancellation(bytes, 0, ensure_running)?;
    debug_assert_eq!(bytes.get(cursor), Some(&b'['));
    cursor += 1;
    loop {
        cursor = skip_json_whitespace_with_cancellation(bytes, cursor, ensure_running)?;
        if bytes.get(cursor) == Some(&b']') {
            break;
        }

        let end = skip_json_string_with_cancellation(bytes, cursor, ensure_running)?
            .expect("公共响应解析已经确认字符串数组中的 JSON string 合法");
        let decoded =
            decode_owned_json_string_with_cancellation(&value[cursor..end], ensure_running)?
                .expect("公共响应解析已经确认字符串数组中的 JSON string 合法");
        strings.push(decoded);

        cursor = skip_json_whitespace_with_cancellation(bytes, end, ensure_running)?;
        match bytes.get(cursor) {
            Some(b',') => cursor += 1,
            Some(b']') => break,
            _ => unreachable!("公共响应解析已经确认字符串数组的 JSON 分隔符合法"),
        }
    }
    ensure_running()?;
    debug_assert_eq!(strings.len(), count);
    Ok(DecodedJsonStringArray::Strings(strings))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JsonStringArrayShape {
    NotArray,
    NonStringItem { item: NonZeroUsize },
    Strings { count: usize },
}

fn inspect_json_string_array_with_cancellation<E>(
    value: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<JsonStringArrayShape, E> {
    let bytes = value.as_bytes();
    let mut cursor = skip_json_whitespace_with_cancellation(bytes, 0, ensure_running)?;
    if bytes.get(cursor) != Some(&b'[') {
        return Ok(JsonStringArrayShape::NotArray);
    }
    cursor += 1;

    let mut count = 0_usize;
    loop {
        cursor = skip_json_whitespace_with_cancellation(bytes, cursor, ensure_running)?;
        if bytes.get(cursor) == Some(&b']') {
            ensure_running()?;
            return Ok(JsonStringArrayShape::Strings { count });
        }

        let item = NonZeroUsize::new(count + 1).expect("字符串数组的一基项目编号不可能为零");
        if bytes.get(cursor) != Some(&b'"') {
            return Ok(JsonStringArrayShape::NonStringItem { item });
        }
        cursor = skip_json_string_with_cancellation(bytes, cursor, ensure_running)?
            .expect("公共响应解析已经确认字符串数组中的 JSON string 合法");
        count += 1;

        cursor = skip_json_whitespace_with_cancellation(bytes, cursor, ensure_running)?;
        match bytes.get(cursor) {
            Some(b',') => cursor += 1,
            Some(b']') => {
                ensure_running()?;
                return Ok(JsonStringArrayShape::Strings { count });
            }
            _ => unreachable!("公共响应解析已经确认字符串数组的 JSON 分隔符合法"),
        }
    }
}

fn skip_json_string_with_cancellation<E>(
    bytes: &[u8],
    opening: usize,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Option<usize>, E> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    if bytes.get(opening) != Some(&b'"') {
        return Ok(None);
    }
    let mut cursor = opening + 1;
    let mut next_check = cursor.saturating_add(CANCELLATION_CHECK_BYTES);
    loop {
        if cursor >= next_check {
            ensure_running()?;
            next_check = cursor.saturating_add(CANCELLATION_CHECK_BYTES);
        }
        let Some(&byte) = bytes.get(cursor) else {
            return Ok(None);
        };
        match byte {
            b'"' => return Ok(Some(cursor + 1)),
            b'\\' => {
                let Some((_, next_cursor)) = decode_json_escape(bytes, cursor) else {
                    return Ok(None);
                };
                cursor = next_cursor;
            }
            0x00..=0x1f => return Ok(None),
            _ => cursor += 1,
        }
    }
}

fn skip_json_whitespace_with_cancellation<E>(
    bytes: &[u8],
    mut cursor: usize,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<usize, E> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    let mut scanned = 0_usize;
    while bytes
        .get(cursor)
        .is_some_and(|byte| is_json_whitespace(*byte))
    {
        cursor += 1;
        scanned += 1;
        if scanned.is_multiple_of(CANCELLATION_CHECK_BYTES) {
            ensure_running()?;
        }
    }
    ensure_running()?;
    Ok(cursor)
}

fn contains_non_json_whitespace_with_cancellation<E>(
    bytes: &[u8],
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<bool, E> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    for (index, byte) in bytes.iter().enumerate() {
        if index.is_multiple_of(CANCELLATION_CHECK_BYTES) {
            ensure_running()?;
        }
        if !is_json_whitespace(*byte) {
            return Ok(true);
        }
    }
    ensure_running()?;
    Ok(false)
}

fn is_json_whitespace(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

fn decode_json_escape(bytes: &[u8], slash: usize) -> Option<(char, usize)> {
    let escaped = *bytes.get(slash + 1)?;
    let result = match escaped {
        b'"' => ('"', slash + 2),
        b'\\' => ('\\', slash + 2),
        b'/' => ('/', slash + 2),
        b'b' => ('\u{0008}', slash + 2),
        b'f' => ('\u{000c}', slash + 2),
        b'n' => ('\n', slash + 2),
        b'r' => ('\r', slash + 2),
        b't' => ('\t', slash + 2),
        b'u' => {
            let first = decode_json_hex_quad(bytes, slash + 2)?;
            if (0xd800..=0xdbff).contains(&first) {
                if bytes.get(slash + 6..slash + 8) != Some(b"\\u") {
                    return None;
                }
                let second = decode_json_hex_quad(bytes, slash + 8)?;
                if !(0xdc00..=0xdfff).contains(&second) {
                    return None;
                }
                let code_point =
                    0x1_0000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00);
                (char::from_u32(code_point)?, slash + 12)
            } else {
                if (0xdc00..=0xdfff).contains(&first) {
                    return None;
                }
                (char::from_u32(u32::from(first))?, slash + 6)
            }
        }
        _ => return None,
    };
    Some(result)
}

fn decode_json_hex_quad(bytes: &[u8], start: usize) -> Option<u16> {
    let mut value = 0_u16;
    for byte in bytes.get(start..start + 4)? {
        value = value.checked_mul(16)?;
        value = value.checked_add(match byte {
            b'0'..=b'9' => u16::from(*byte - b'0'),
            b'a'..=b'f' => u16::from(*byte - b'a' + 10),
            b'A'..=b'F' => u16::from(*byte - b'A' + 10),
            _ => return None,
        })?;
    }
    Some(value)
}

fn deserialize_json_with_cancellation<T, E>(
    value: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<Result<T, serde_json::Error>, E>
where
    T: DeserializeOwned,
{
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    ensure_running()?;
    let reader = CancellableResponseReader {
        input: value.as_bytes(),
        position: 0,
        bytes_until_check: 0,
        ensure_running,
        cancellation: None,
    };
    let mut reader = BufReader::with_capacity(CANCELLATION_CHECK_BYTES, reader);
    let parsed = serde_json::from_reader::<_, T>(&mut reader);
    let reader = reader.into_inner();
    if let Some(error) = reader.cancellation {
        return Err(error);
    }
    ensure_running()?;
    Ok(parsed)
}

struct CancellableResponseReader<'a, 'check, E, F> {
    input: &'a [u8],
    position: usize,
    bytes_until_check: usize,
    ensure_running: &'check mut F,
    cancellation: Option<E>,
}

impl<E, F> Read for CancellableResponseReader<'_, '_, E, F>
where
    F: FnMut() -> Result<(), E>,
{
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

        if self.cancellation.is_some() {
            return Err(io::Error::other("翻译响应解析已取消"));
        }
        if output.is_empty() || self.position == self.input.len() {
            return Ok(0);
        }
        if self.bytes_until_check == 0 {
            if let Err(error) = (self.ensure_running)() {
                self.cancellation = Some(error);
                // serde_json 使用的读取辅助会自动重试 `Interrupted`。取消必须让解析
                // 立即返回，具体取消值由 `cancellation` 字段原样交还调用方。
                return Err(io::Error::other("翻译响应解析已取消"));
            }
            self.bytes_until_check = CANCELLATION_CHECK_BYTES;
        }
        let available = self.input.len() - self.position;
        let read = available.min(output.len()).min(self.bytes_until_check);
        output[..read].copy_from_slice(&self.input[self.position..self.position + read]);
        self.position += read;
        self.bytes_until_check -= read;
        Ok(read)
    }
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

    fn subslice(self, local_start: usize, local_end: usize) -> Self {
        debug_assert!(local_start <= local_end);
        debug_assert!(local_end <= self.value.len());
        Self {
            raw: self.raw,
            value: &self.value[local_start..local_end],
            start: self.start + local_start,
        }
    }

    fn location_at_with_cancellation<E>(
        self,
        local_byte_offset: usize,
        ensure_running: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<(NonZeroUsize, NonZeroUsize), E> {
        response_location_with_cancellation(
            self.raw,
            self.start + local_byte_offset,
            ensure_running,
        )
    }

    fn location_for_local_with_cancellation<E>(
        self,
        local_line: usize,
        local_column: usize,
        ensure_running: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<(NonZeroUsize, NonZeroUsize), E> {
        let (start_line, start_column) =
            response_location_with_cancellation(self.raw, self.start, ensure_running)?;
        let local_line = local_line.max(1);
        let local_column = local_column.max(1);
        if local_line == 1 {
            Ok((
                start_line,
                NonZeroUsize::new(start_column.get() + local_column - 1)
                    .expect("一基列号相加后仍非零"),
            ))
        } else {
            Ok((
                NonZeroUsize::new(start_line.get() + local_line - 1).expect("一基行号相加后仍非零"),
                NonZeroUsize::new(local_column).expect("局部列号已收窄为至少一"),
            ))
        }
    }

    fn error_at_with_cancellation<E>(
        self,
        kind: TranslationTaskResponseParseErrorKind,
        local_byte_offset: usize,
        ensure_running: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<TranslationTaskResponseParseError, E> {
        let (line, column) =
            self.location_at_with_cancellation(local_byte_offset, ensure_running)?;
        Ok(TranslationTaskResponseParseError::new(kind, line, column))
    }
}

fn response_location_with_cancellation<E>(
    raw: &str,
    byte_offset: usize,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<(NonZeroUsize, NonZeroUsize), E> {
    const CANCELLATION_CHECK_BYTES: usize = 64 * 1024;

    let byte_offset = byte_offset.min(raw.len());
    let preceding = &raw[..byte_offset];
    let mut line = 1_usize;
    let mut line_start = 0_usize;
    for (chunk_index, chunk) in preceding
        .as_bytes()
        .chunks(CANCELLATION_CHECK_BYTES)
        .enumerate()
    {
        ensure_running()?;
        let chunk_start = chunk_index * CANCELLATION_CHECK_BYTES;
        for (index, byte) in chunk.iter().enumerate() {
            if *byte == b'\n' {
                line += 1;
                line_start = chunk_start + index + 1;
            }
        }
    }
    ensure_running()?;
    let column = preceding.len() - line_start + 1;
    Ok((
        NonZeroUsize::new(line).expect("一基行号不可能为零"),
        NonZeroUsize::new(column).expect("一基列号不可能为零"),
    ))
}

fn response_text_is_whitespace_with_cancellation<E>(
    value: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<bool, E> {
    const CANCELLATION_CHECK_CHARACTERS: usize = 16 * 1024;

    for (index, character) in value.chars().enumerate() {
        if index.is_multiple_of(CANCELLATION_CHECK_CHARACTERS) {
            ensure_running()?;
        }
        if !character.is_whitespace() {
            return Ok(false);
        }
    }
    ensure_running()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    const fn mode(thinking: bool, source_echo: bool) -> TranslationResponseMode {
        TranslationResponseMode::new(thinking, source_echo)
    }

    #[test]
    fn preserves_order_duplicates_and_zero_based_canonical_ids() {
        let parsed = parse_translation_response(
            r#"{"0":["甲"],"bad":["乙"],"0":["丙"],"00":["丁"],"01":["戊"]}"#,
            mode(false, false),
        )
        .expect("合法对象应解析");

        let entries = parsed
            .entries()
            .iter()
            .map(ParsedTranslationAssistantEntry::canonical_id)
            .collect::<Vec<_>>();
        assert_eq!(
            entries,
            [Some(TaskId::new(0)), None, Some(TaskId::new(0)), None, None,]
        );
        assert!(entries[0].is_some(), "规范 ID 0 必须可表示");
    }

    #[test]
    fn rejects_negative_nondigit_leading_zero_and_overflow_ids() {
        let overflow = format!("{}0", usize::MAX);
        for invalid in ["", "-1", "+1", " 1", "1 ", "00", "01", &overflow] {
            assert_eq!(
                parse_model_output_id_with_cancellation(invalid, &mut || {
                    Ok::<_, Infallible>(())
                })
                .expect("未取消"),
                None,
                "ID 必须是规范的无符号十进制数：{invalid:?}"
            );
        }
        assert_eq!(
            parse_model_output_id_with_cancellation("0", &mut || Ok::<_, Infallible>(()))
                .expect("未取消"),
            Some(TaskId::new(0))
        );
        assert_eq!(
            parse_model_output_id_with_cancellation(&usize::MAX.to_string(), &mut || {
                Ok::<_, Infallible>(())
            })
            .expect("未取消"),
            Some(TaskId::new(usize::MAX))
        );
    }

    #[test]
    fn parses_all_four_response_modes() {
        let plain = parse_translation_response(r#"{"0":["译文"]}"#, mode(false, false))
            .expect("plain 响应应解析");
        assert_eq!(
            plain.entries()[0]
                .decode_translation_value_with_cancellation::<Infallible>(|| Ok(()))
                .expect("未取消"),
            DecodedTranslationAssistantValue::Translation(DecodedJsonStringArray::Strings(vec![
                "译文".to_owned()
            ]))
        );

        let thinking = parse_translation_response(
            r#"{"think":"结合上下文判断语气","translations":{"0":["译文"]}}"#,
            mode(true, false),
        )
        .expect("thinking 响应应解析");
        assert_eq!(thinking.entries().len(), 1);

        let echo = parse_translation_response(
            r#"{"0":{"source":["原文"],"translation":["译文"]}}"#,
            mode(false, true),
        )
        .expect("source echo 响应应解析");
        assert_eq!(
            echo.entries()[0]
                .decode_translation_value_with_cancellation::<Infallible>(|| Ok(()))
                .expect("未取消"),
            DecodedTranslationAssistantValue::SourceEcho(DecodedSourceEchoValue::Fields {
                source: DecodedJsonStringArray::Strings(vec!["原文".to_owned()]),
                translation: DecodedJsonStringArray::Strings(vec!["译文".to_owned()]),
            })
        );

        let thinking_echo = parse_translation_response(
            r#"{"think":"判断","translations":{"0":{"source":["回显可以不同"],"translation":["译文"]}}}"#,
            mode(true, true),
        )
        .expect("thinking + source echo 响应应解析");
        assert_eq!(thinking_echo.entries().len(), 1);
        assert_eq!(
            thinking_echo.entries()[0]
                .decode_translation_value_with_cancellation::<Infallible>(|| Ok(()))
                .expect("未取消"),
            DecodedTranslationAssistantValue::SourceEcho(DecodedSourceEchoValue::Fields {
                source: DecodedJsonStringArray::Strings(vec!["回显可以不同".to_owned()]),
                translation: DecodedJsonStringArray::Strings(vec!["译文".to_owned()]),
            })
        );
    }

    #[cfg(feature = "release-stress")]
    #[test]
    fn release_stress_keeps_each_id_raw_value_without_recursing_into_wrong_shapes() {
        const DEPTH: usize = 10_000;

        let deep_value = format!("{}0{}", "[".repeat(DEPTH), "]".repeat(DEPTH));
        let response = format!(r#"{{"0":{deep_value},"1":["合法译文"]}}"#);
        let parsed = parse_translation_response(&response, mode(false, false))
            .expect("外层对象与每个 raw value 均为有效 JSON");

        assert_eq!(parsed.entries()[0].raw_value().get(), deep_value);
        assert_eq!(parsed.entries()[1].raw_value().get(), r#"["合法译文"]"#);
        drop(parsed);
    }

    #[test]
    fn thinking_wrapper_is_exact_and_thinking_must_be_nonempty_string() {
        for invalid in [
            r#"{"translations":{"0":["译文"]}}"#,
            r#"{"think":"判断"}"#,
            r#"{"think":3,"translations":{"0":["译文"]}}"#,
            r#"{"think":"判断","translations":{"0":["译文"]},"extra":true}"#,
            r#"{"think":"判断","translations":[]}"#,
            r#"不是 JSON"#,
        ] {
            assert!(
                parse_translation_response(invalid, mode(true, false)).is_err(),
                "thinking 根对象的字段和类型必须严格：{invalid}"
            );
        }

        for blank in ["", " ", "\n\t"] {
            let response = format!(
                r#"{{"think":{},"translations":{{"0":["译文"]}}}}"#,
                serde_json::to_string(blank).expect("测试字符串可编码")
            );
            let error = parse_translation_response(&response, mode(true, false))
                .expect_err("空白 thinking 必须使整份响应无效");
            assert_eq!(
                error.kind(),
                TranslationTaskResponseParseErrorKind::ThinkingEmpty
            );
        }

        for invalid in [
            r#"[]"#,
            r#"{"think":"第一次","think":"第二次","translations":{"0":["译文"]}}"#,
            r#"{"think":"判断","translations":{},"translations":{"0":["译文"]}}"#,
        ] {
            let error = parse_translation_response(invalid, mode(true, false))
                .expect_err("合法 JSON 的 shape 错误不能进入 repair");
            assert_eq!(
                error.kind(),
                TranslationTaskResponseParseErrorKind::Json(
                    TranslationTaskResponseJsonErrorCategory::Shape
                )
            );
        }

        let missing_translations =
            parse_translation_response(r#"{"think":"判断"}"#, mode(true, false))
                .expect_err("缺少 translations 的合法 JSON 必须按根响应合同拒绝");
        assert_eq!(
            missing_translations.kind(),
            TranslationTaskResponseParseErrorKind::Json(
                TranslationTaskResponseJsonErrorCategory::Shape
            )
        );
    }

    #[test]
    fn source_echo_shape_errors_stay_on_the_individual_id() {
        let parsed = parse_translation_response(
            r#"{"0":true,"1":{"translation":["译文"]},"2":{"source":["原文"]},"3":{"source":["甲"],"source":["乙"],"translation":["译文"]},"4":{"source":["原文"],"translation":["甲"],"translation":["乙"]},"5":{"source":["原文"],"translation":["译文"],"extra":0},"6":{"source":true,"translation":["译文"]},"7":{"source":["原文"],"translation":[3]}}"#,
            mode(false, true),
        )
        .expect("逐 ID 的 echo 形状错误不能使整个根响应失败");

        let decoded = parsed
            .entries()
            .iter()
            .map(|entry| {
                entry
                    .decode_translation_value_with_cancellation::<Infallible>(|| Ok(()))
                    .expect("未取消")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            decoded[0],
            DecodedTranslationAssistantValue::SourceEcho(DecodedSourceEchoValue::NotObject)
        );
        assert_eq!(
            decoded[1],
            DecodedTranslationAssistantValue::SourceEcho(DecodedSourceEchoValue::InvalidFields(
                DecodedSourceEchoFieldsError::MissingSource
            ))
        );
        assert_eq!(
            decoded[2],
            DecodedTranslationAssistantValue::SourceEcho(DecodedSourceEchoValue::InvalidFields(
                DecodedSourceEchoFieldsError::MissingTranslation
            ))
        );
        assert_eq!(
            decoded[3],
            DecodedTranslationAssistantValue::SourceEcho(DecodedSourceEchoValue::InvalidFields(
                DecodedSourceEchoFieldsError::DuplicateSource
            ))
        );
        assert_eq!(
            decoded[4],
            DecodedTranslationAssistantValue::SourceEcho(DecodedSourceEchoValue::InvalidFields(
                DecodedSourceEchoFieldsError::DuplicateTranslation
            ))
        );
        assert_eq!(
            decoded[5],
            DecodedTranslationAssistantValue::SourceEcho(DecodedSourceEchoValue::InvalidFields(
                DecodedSourceEchoFieldsError::UnexpectedField {
                    field: "extra".to_owned()
                }
            ))
        );
        assert_eq!(
            decoded[6],
            DecodedTranslationAssistantValue::SourceEcho(DecodedSourceEchoValue::Fields {
                source: DecodedJsonStringArray::NotArray,
                translation: DecodedJsonStringArray::Strings(vec!["译文".to_owned()]),
            })
        );
        assert_eq!(
            decoded[7],
            DecodedTranslationAssistantValue::SourceEcho(DecodedSourceEchoValue::Fields {
                source: DecodedJsonStringArray::Strings(vec!["原文".to_owned()]),
                translation: DecodedJsonStringArray::NonStringItem {
                    item: NonZeroUsize::new(1).expect("测试项编号非零"),
                },
            })
        );
    }

    #[test]
    fn string_array_decoder_preserves_json_escape_semantics_and_shape_errors() {
        let response = parse_translation_response(
            r#"{"0":["原文","换行\n","\ud83d\ude00"],"1":["合法",3],"2":true}"#,
            mode(false, false),
        )
        .expect("合法外层响应应解析");

        assert_eq!(
            response.entries()[0]
                .decode_translation_value_with_cancellation::<Infallible>(|| Ok(()))
                .expect("未取消"),
            DecodedTranslationAssistantValue::Translation(DecodedJsonStringArray::Strings(vec![
                "原文".to_owned(),
                "换行\n".to_owned(),
                "😀".to_owned(),
            ]))
        );
        assert_eq!(
            response.entries()[1]
                .decode_translation_value_with_cancellation::<Infallible>(|| Ok(()))
                .expect("未取消"),
            DecodedTranslationAssistantValue::Translation(DecodedJsonStringArray::NonStringItem {
                item: NonZeroUsize::new(2).expect("测试项编号非零"),
            })
        );
        assert_eq!(
            response.entries()[2]
                .decode_translation_value_with_cancellation::<Infallible>(|| Ok(()))
                .expect("未取消"),
            DecodedTranslationAssistantValue::Translation(DecodedJsonStringArray::NotArray)
        );
    }

    #[test]
    fn repairs_model_json_without_weakening_shape_validation() {
        let parsed =
            parse_translation_response("\n{\"0\":[\"ok\"],\"1\":true}\n", mode(false, false))
                .expect("值形状由逐 ID 验收");
        assert_eq!(parsed.entries()[1].raw_value().get(), "true");

        let bom = parse_translation_response("\u{feff}{\"0\":[\"ok\"]}", mode(false, false))
            .expect("BOM 应经过受控修复");
        assert_eq!(bom.entries()[0].raw_value().get(), r#"["ok"]"#);

        let unicode_whitespace =
            parse_translation_response("\u{2003}{\"0\":[\"ok\"]}\u{2003}", mode(false, false))
                .expect("非 JSON 空白应经过受控修复");
        assert_eq!(
            unicode_whitespace.entries()[0].raw_value().get(),
            r#"["ok"]"#
        );

        let error = parse_translation_response(
            "\n{\"think\":\"判断\",\"translations\":{\"0\":",
            mode(true, false),
        )
        .expect_err("截断 JSON 必须失败");
        assert_eq!(error.line().get(), 2);
        assert!(error.column().get() >= 35);

        let fenced = parse_translation_response(
            "\r\n```json \r\n{\"0\":[\"ok\"]}\r\n```  \r\n",
            mode(false, false),
        )
        .expect("规范 JSON 围栏应作为合法响应外层");
        assert_eq!(fenced.entries()[0].raw_value().get(), r#"["ok"]"#);

        let repaired_inside_fence =
            parse_translation_response("```json\n{'0':['ok']}\n```", mode(false, false))
                .expect("规范围栏内部仍应使用保守 JSON 修复");
        assert_eq!(
            repaired_inside_fence.entries()[0]
                .decode_translation_value_with_cancellation::<Infallible>(|| Ok(()))
                .expect("未取消"),
            DecodedTranslationAssistantValue::Translation(DecodedJsonStringArray::Strings(vec![
                "ok".to_owned(),
            ]))
        );

        let repaired_fence = parse_translation_response(
            "\u{2003}\r\n```json\r\n{\"0\":[\"ok\"]}\r\n```\r\n",
            mode(false, false),
        )
        .expect("非 JSON 空白包围的围栏仍应由保守修复处理");
        assert_eq!(repaired_fence.entries()[0].raw_value().get(), r#"["ok"]"#);

        let multiple_fences = parse_translation_response(
            "```json\n{\"0\":[\"first\"]}\n```\n```json\n{\"1\":[\"second\"]}\n```",
            mode(false, false),
        )
        .expect_err("多个 JSON 围栏必须保持为歧义响应");
        assert_eq!(
            multiple_fences.kind(),
            TranslationTaskResponseParseErrorKind::Json(
                TranslationTaskResponseJsonErrorCategory::Syntax
            )
        );

        let quoted = parse_translation_response(r#"{"0":["type: "free""]}"#, mode(false, false))
            .expect_err("内部双引号存在多种合理解释时必须拒绝整份响应");
        assert_eq!(
            quoted.kind(),
            TranslationTaskResponseParseErrorKind::Json(
                TranslationTaskResponseJsonErrorCategory::Syntax
            )
        );

        let adjacent =
            parse_translation_response(r#"{"0":["第一行" "第二行"]}"#, mode(false, false))
                .expect_err("空白分隔的相邻引号也不能被猜成两个译文数组项");
        assert_eq!(
            adjacent.kind(),
            TranslationTaskResponseParseErrorKind::Json(
                TranslationTaskResponseJsonErrorCategory::Syntax
            )
        );

        let unterminated = parse_translation_response("{\"0\":[\"unfinished]}", mode(false, false))
            .expect_err("未结束字符串不能由保守修复器补造结束引号");
        assert_eq!(
            unterminated.kind(),
            TranslationTaskResponseParseErrorKind::Json(
                TranslationTaskResponseJsonErrorCategory::UnexpectedEof
            )
        );

        let repaired_shape = parse_translation_response(
            "说明\r\n```json\r\n{\"think\":\"判断\",\"translations\":{\"0\":[\"ok\"]},\"extra\":true}\r\n```",
            mode(true, false),
        )
        .expect_err("围栏修复后仍必须执行严格根结构验收");
        assert_eq!(
            repaired_shape.kind(),
            TranslationTaskResponseParseErrorKind::Json(
                TranslationTaskResponseJsonErrorCategory::Shape
            )
        );
        assert_eq!(repaired_shape.line().get(), 3);
        assert!(repaired_shape.column().get() > 40);

        let repair_rejection = parse_translation_response(
            "{\"0\":[\"first\"]}\n{\"1\":[\"second\"]}",
            mode(false, false),
        )
        .expect_err("多个 JSON 候选必须由 Conservative 拒绝");
        assert_eq!(
            repair_rejection.kind(),
            TranslationTaskResponseParseErrorKind::Json(
                TranslationTaskResponseJsonErrorCategory::Syntax
            )
        );
        assert_eq!(repair_rejection.line().get(), 2);
        assert_eq!(repair_rejection.column().get(), 1);
    }

    #[test]
    fn cancellable_parser_stops_while_reading_long_json_and_thinking() {
        let response = format!(
            r#"{{"think":"{}","translations":{{"0":["译文"]}}}}"#,
            "分析".repeat(512 * 1024)
        );
        let polls = Cell::new(0_usize);

        let parsed =
            parse_translation_response_with_cancellation(&response, mode(true, false), || {
                let next = polls.get() + 1;
                polls.set(next);
                if next >= 20 { Err("cancelled") } else { Ok(()) }
            });

        assert!(matches!(parsed, Err("cancelled")));
        assert_eq!(polls.get(), 20);
    }

    #[test]
    fn cancellable_parser_stops_during_json_repair() {
        let response = format!("{{'0':['{}']}}", "译".repeat(512 * 1024));
        let polls = Cell::new(0_usize);

        let parsed =
            parse_translation_response_with_cancellation(&response, mode(false, false), || {
                let next = polls.get() + 1;
                polls.set(next);
                if next >= 12 { Err("cancelled") } else { Ok(()) }
            });

        assert!(matches!(parsed, Err("cancelled")));
        assert_eq!(polls.get(), 12);
    }

    #[test]
    fn cancellable_parser_stops_while_locating_long_json_fence() {
        let response = format!("```json\n{{\"0\":[\"{}\"]}}\n```", "译".repeat(512 * 1024));
        let polls = Cell::new(0_usize);

        let parsed =
            parse_translation_response_with_cancellation(&response, mode(false, false), || {
                let next = polls.get() + 1;
                polls.set(next);
                if next >= 12 { Err("cancelled") } else { Ok(()) }
            });

        assert!(matches!(parsed, Err("cancelled")));
        assert_eq!(polls.get(), 12);
    }

    #[test]
    fn response_error_location_scan_observes_cancellation() {
        let raw = format!("{}\n{}", "前".repeat(128 * 1024), "后".repeat(128 * 1024));
        let polls = Cell::new(0_usize);

        let location = response_location_with_cancellation(&raw, raw.len(), &mut || {
            let next = polls.get() + 1;
            polls.set(next);
            if next >= 3 { Err("cancelled") } else { Ok(()) }
        });

        assert_eq!(location, Err("cancelled"));
        assert_eq!(polls.get(), 3);
    }
}
