//! ATT 托管翻译任务共同使用的响应模式、JSON ID 与可读记录投影。
//!
//! 本模块不理解游戏引擎、持久化身份、Placeholder 或语言验收。调用方只消费一次解析
//! 建立的有序条目，并在自己的语义边界逐 ID 验收。

#[cfg(test)]
use std::convert::Infallible;
use std::fmt;
use std::io::{self, BufReader, Read};
use std::num::NonZeroUsize;

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
    ThinkingEmpty,
}

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
            TranslationTaskResponseParseErrorKind::ThinkingEmpty => "模型响应的思考内容为空",
        };
        format!("{message}，第 {} 行、第 {} 列", self.line, self.column)
    }
}

/// Assistant JSON 中保持原始顺序和重复项的一个条目。
#[derive(Debug)]
pub(crate) struct ParsedTranslationAssistantEntry {
    id: String,
    value: Box<RawValue>,
    canonical_id: Option<TaskId>,
    source_echo: bool,
}

impl ParsedTranslationAssistantEntry {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) const fn canonical_id(&self) -> Option<TaskId> {
        self.canonical_id
    }

    #[cfg(test)]
    pub(crate) fn raw_value(&self) -> &RawValue {
        self.value.as_ref()
    }

    /// 按当前响应模式解码一个 ID 的译文值，并保留逐字段形状错误。
    ///
    /// 原文回显只供人工或 agent 排查，不参与译文关联。这里验证它存在且为字符串数组，
    /// 但不比较它和请求原文的内容。
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

    pub(crate) fn into_parts(self) -> (String, Box<RawValue>, Option<TaskId>) {
        (self.id, self.value, self.canonical_id)
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
    let value = trim_model_response_with_cancellation(value, &mut ensure_running)?;
    let (thinking, batch) = if response_mode.thinking() {
        let wrapper = match deserialize_json_with_cancellation::<ThinkingModelOutputBatch, _>(
            value.value,
            &mut ensure_running,
        )? {
            Ok(wrapper) => wrapper,
            Err(source) => {
                return Ok(Err(translation_response_json_error_with_cancellation(
                    value,
                    source,
                    &mut ensure_running,
                )?));
            }
        };
        let Some(thinking) =
            decode_owned_json_string_with_cancellation(wrapper.think.get(), &mut ensure_running)?
        else {
            return Ok(Err(value.error_at_with_cancellation(
                TranslationTaskResponseParseErrorKind::Json(
                    TranslationTaskResponseJsonErrorCategory::Shape,
                ),
                0,
                &mut ensure_running,
            )?));
        };
        if response_text_is_whitespace_with_cancellation(&thinking, &mut ensure_running)? {
            return Ok(Err(value.error_at_with_cancellation(
                TranslationTaskResponseParseErrorKind::ThinkingEmpty,
                0,
                &mut ensure_running,
            )?));
        }
        (Some(thinking), wrapper.translations)
    } else {
        let batch = match deserialize_model_output_batch_with_cancellation(
            value.value,
            &mut ensure_running,
        )? {
            Ok(batch) => batch,
            Err(source) => {
                return Ok(Err(translation_response_json_error_with_cancellation(
                    value,
                    source,
                    &mut ensure_running,
                )?));
            }
        };
        (None, batch)
    };
    let mut entries = Vec::with_capacity(batch.0.len());
    for output in batch.0 {
        ensure_running()?;
        let canonical_id =
            parse_model_output_id_with_cancellation(&output.id, &mut ensure_running)?;
        entries.push(ParsedTranslationAssistantEntry {
            canonical_id,
            id: output.id,
            value: output.value,
            source_echo: response_mode.source_echo(),
        });
    }
    ensure_running()?;
    Ok(Ok(ParsedTranslationResponse { thinking, entries }))
}

fn translation_response_json_error_with_cancellation<E>(
    value: LocatedModelResponse<'_>,
    source: serde_json::Error,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<TranslationTaskResponseParseError, E> {
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

    fn trim_with_cancellation<E>(
        self,
        ensure_running: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Self, E> {
        let leading = leading_whitespace_bytes_with_cancellation(self.value, ensure_running)?;
        let value = self.advance(leading);
        let trailing = trailing_whitespace_bytes_with_cancellation(value.value, ensure_running)?;
        Ok(value.prefix(value.value.len() - trailing))
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

fn trim_model_response_with_cancellation<'a, E>(
    value: &'a str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<LocatedModelResponse<'a>, E> {
    let value = LocatedModelResponse::new(value).trim_with_cancellation(ensure_running)?;
    let value = if value.value.starts_with('\u{feff}') {
        value.advance('\u{feff}'.len_utf8())
    } else {
        value
    };
    value.trim_with_cancellation(ensure_running)
}

fn leading_whitespace_bytes_with_cancellation<E>(
    value: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<usize, E> {
    const CANCELLATION_CHECK_CHARACTERS: usize = 16 * 1024;

    let mut leading = 0_usize;
    for (index, (offset, character)) in value.char_indices().enumerate() {
        if index.is_multiple_of(CANCELLATION_CHECK_CHARACTERS) {
            ensure_running()?;
        }
        if !character.is_whitespace() {
            break;
        }
        leading = offset + character.len_utf8();
    }
    ensure_running()?;
    Ok(leading)
}

fn trailing_whitespace_bytes_with_cancellation<E>(
    value: &str,
    ensure_running: &mut impl FnMut() -> Result<(), E>,
) -> Result<usize, E> {
    const CANCELLATION_CHECK_CHARACTERS: usize = 16 * 1024;

    let mut trailing_start = value.len();
    for (index, (offset, character)) in value.char_indices().rev().enumerate() {
        if index.is_multiple_of(CANCELLATION_CHECK_CHARACTERS) {
            ensure_running()?;
        }
        if !character.is_whitespace() {
            break;
        }
        trailing_start = offset;
    }
    ensure_running()?;
    Ok(value.len() - trailing_start)
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
            .map(|entry| (entry.id(), entry.canonical_id()))
            .collect::<Vec<_>>();
        assert_eq!(
            entries,
            [
                ("0", Some(TaskId::new(0))),
                ("bad", None),
                ("0", Some(TaskId::new(0))),
                ("00", None),
                ("01", None),
            ]
        );
        assert!(entries[0].1.is_some(), "规范 ID 0 必须可表示");
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
        assert_eq!(plain.thinking(), None);
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
        assert_eq!(thinking.thinking(), Some("结合上下文判断语气"));

        let echo = parse_translation_response(
            r#"{"0":{"source":["原文"],"translation":["译文"]}}"#,
            mode(false, true),
        )
        .expect("source echo 响应应解析");
        assert_eq!(echo.thinking(), None);
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
        assert_eq!(thinking_echo.thinking(), Some("判断"));
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

    #[test]
    fn keeps_each_id_raw_value_without_recursing_into_wrong_shapes() {
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
            r#"{"think":"第一次","think":"第二次","translations":{"0":["译文"]}}"#,
            r#"{"think":"判断","translations":{},"translations":{"0":["译文"]}}"#,
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
    fn reports_shape_and_full_response_location() {
        let parsed =
            parse_translation_response("\n{\"0\":[\"ok\"],\"1\":true}\n", mode(false, false))
                .expect("值形状由逐 ID 验收");
        assert_eq!(parsed.entries()[1].raw_value().get(), "true");

        let error = parse_translation_response(
            "\n{\"think\":\"判断\",\"translations\":{\"0\":",
            mode(true, false),
        )
        .expect_err("截断 JSON 必须失败");
        assert_eq!(error.line().get(), 2);
        assert!(error.column().get() >= 35);

        let fenced =
            parse_translation_response("```json\n{\"0\":[\"ok\"]}\n```", mode(false, false))
                .expect_err("当前协议只接受裸 JSON，不接受 Markdown 围栏");
        assert!(matches!(
            fenced.kind(),
            TranslationTaskResponseParseErrorKind::Json(
                TranslationTaskResponseJsonErrorCategory::Syntax
            )
        ));
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
