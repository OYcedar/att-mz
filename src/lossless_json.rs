//! ATT 内部共用的严格、无深度上限 JSON 解析边界。

use std::collections::HashSet;
use std::convert::Infallible;
use std::error::Error;
use std::fmt;

use serde_json::{Map, Number, Value};

/// 保留 JSON number 原文的中间值。
#[derive(Debug)]
pub(crate) enum LosslessJsonValue {
    Null,
    Boolean(bool),
    String(String),
    Number(String),
    Array(Vec<Self>),
    Object(Vec<(String, Self)>),
}

impl Clone for LosslessJsonValue {
    fn clone(&self) -> Self {
        enum Work<'a> {
            Value(&'a LosslessJsonValue),
            Array(usize),
            Object(Vec<String>),
        }

        let mut work = vec![Work::Value(self)];
        let mut values = Vec::new();
        while let Some(item) = work.pop() {
            match item {
                Work::Value(Self::Null) => values.push(Self::Null),
                Work::Value(Self::Boolean(value)) => values.push(Self::Boolean(*value)),
                Work::Value(Self::String(value)) => values.push(Self::String(value.clone())),
                Work::Value(Self::Number(value)) => values.push(Self::Number(value.clone())),
                Work::Value(Self::Array(items)) => {
                    work.push(Work::Array(items.len()));
                    work.extend(items.iter().rev().map(Work::Value));
                }
                Work::Value(Self::Object(entries)) => {
                    work.push(Work::Object(
                        entries.iter().map(|(key, _)| key.clone()).collect(),
                    ));
                    work.extend(entries.iter().rev().map(|(_, value)| Work::Value(value)));
                }
                Work::Array(length) => {
                    let start = values.len() - length;
                    let children = values.split_off(start);
                    values.push(Self::Array(children));
                }
                Work::Object(keys) => {
                    let start = values.len() - keys.len();
                    let children = values.split_off(start);
                    values.push(Self::Object(keys.into_iter().zip(children).collect()));
                }
            }
        }
        values.pop().expect("根 JSON 值必须产生一个克隆")
    }
}

impl PartialEq for LosslessJsonValue {
    fn eq(&self, other: &Self) -> bool {
        let mut work = vec![(self, other)];
        while let Some((left, right)) = work.pop() {
            match (left, right) {
                (Self::Null, Self::Null) => {}
                (Self::Boolean(left), Self::Boolean(right)) if left == right => {}
                (Self::String(left), Self::String(right)) if left == right => {}
                (Self::Number(left), Self::Number(right)) if left == right => {}
                (Self::Array(left), Self::Array(right)) if left.len() == right.len() => {
                    work.extend(left.iter().zip(right).rev());
                }
                (Self::Object(left), Self::Object(right)) if left.len() == right.len() => {
                    for ((left_key, left_value), (right_key, right_value)) in
                        left.iter().zip(right).rev()
                    {
                        if left_key != right_key {
                            return false;
                        }
                        work.push((left_value, right_value));
                    }
                }
                _ => return false,
            }
        }
        true
    }
}

impl Drop for LosslessJsonValue {
    fn drop(&mut self) {
        let mut pending = Vec::new();
        take_children(self, &mut pending);
        while let Some(mut value) = pending.pop() {
            take_children(&mut value, &mut pending);
        }
    }
}

fn take_children(value: &mut LosslessJsonValue, pending: &mut Vec<LosslessJsonValue>) {
    match value {
        LosslessJsonValue::Array(values) => pending.append(values),
        LosslessJsonValue::Object(entries) => {
            pending.extend(entries.drain(..).map(|(_, value)| value));
        }
        LosslessJsonValue::Null
        | LosslessJsonValue::Boolean(_)
        | LosslessJsonValue::String(_)
        | LosslessJsonValue::Number(_) => {}
    }
}

/// 严格 JSON 解析失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LosslessJsonError {
    Syntax {
        byte_offset: usize,
        reason: &'static str,
    },
    DuplicateObjectKey {
        byte_offset: usize,
    },
}

impl fmt::Display for LosslessJsonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax {
                byte_offset,
                reason,
            } => write!(formatter, "JSON 在字节 {byte_offset} 处无效：{reason}"),
            Self::DuplicateObjectKey { byte_offset } => {
                write!(formatter, "JSON object 在字节 {byte_offset} 处包含重复键")
            }
        }
    }
}

impl Error for LosslessJsonError {}

/// 直接构造 `serde_json::Value` 时保留的解析或 number 后端错误。
#[derive(Debug)]
pub(crate) enum JsonValueDecodeError {
    Syntax(LosslessJsonError),
    Backend(serde_json::Error),
}

enum ParseValueError<E> {
    Syntax(LosslessJsonError),
    Build(E),
}

impl<E> From<LosslessJsonError> for ParseValueError<E> {
    fn from(source: LosslessJsonError) -> Self {
        Self::Syntax(source)
    }
}

trait JsonValueBuilder {
    type Value;
    type Array;
    type Object;
    type Error;

    fn null() -> Self::Value;
    fn boolean(value: bool) -> Self::Value;
    fn string(value: String) -> Self::Value;
    fn number(source: &str) -> Result<Self::Value, Self::Error>;
    fn new_array() -> Self::Array;
    fn push_array(array: &mut Self::Array, value: Self::Value);
    fn finish_array(array: Self::Array) -> Self::Value;
    fn new_object() -> Self::Object;
    fn push_object(object: &mut Self::Object, key: String, value: Self::Value);
    fn finish_object(object: Self::Object) -> Self::Value;
    fn drop_value(value: Self::Value);

    fn drop_array(array: Self::Array) {
        Self::drop_value(Self::finish_array(array));
    }

    fn drop_object(object: Self::Object) {
        Self::drop_value(Self::finish_object(object));
    }
}

struct LosslessValueBuilder;

impl JsonValueBuilder for LosslessValueBuilder {
    type Value = LosslessJsonValue;
    type Array = Vec<LosslessJsonValue>;
    type Object = Vec<(String, LosslessJsonValue)>;
    type Error = Infallible;

    fn null() -> Self::Value {
        LosslessJsonValue::Null
    }

    fn boolean(value: bool) -> Self::Value {
        LosslessJsonValue::Boolean(value)
    }

    fn string(value: String) -> Self::Value {
        LosslessJsonValue::String(value)
    }

    fn number(source: &str) -> Result<Self::Value, Self::Error> {
        Ok(LosslessJsonValue::Number(source.to_owned()))
    }

    fn new_array() -> Self::Array {
        Vec::new()
    }

    fn push_array(array: &mut Self::Array, value: Self::Value) {
        array.push(value);
    }

    fn finish_array(array: Self::Array) -> Self::Value {
        LosslessJsonValue::Array(array)
    }

    fn new_object() -> Self::Object {
        Vec::new()
    }

    fn push_object(object: &mut Self::Object, key: String, value: Self::Value) {
        object.push((key, value));
    }

    fn finish_object(object: Self::Object) -> Self::Value {
        LosslessJsonValue::Object(object)
    }

    fn drop_value(value: Self::Value) {
        drop(value);
    }
}

struct SerdeValueBuilder;

impl JsonValueBuilder for SerdeValueBuilder {
    type Value = Value;
    type Array = Vec<Value>;
    type Object = Map<String, Value>;
    type Error = serde_json::Error;

    fn null() -> Self::Value {
        Value::Null
    }

    fn boolean(value: bool) -> Self::Value {
        Value::Bool(value)
    }

    fn string(value: String) -> Self::Value {
        Value::String(value)
    }

    fn number(source: &str) -> Result<Self::Value, Self::Error> {
        serde_json::from_str::<Number>(source).map(Value::Number)
    }

    fn new_array() -> Self::Array {
        Vec::new()
    }

    fn push_array(array: &mut Self::Array, value: Self::Value) {
        array.push(value);
    }

    fn finish_array(array: Self::Array) -> Self::Value {
        Value::Array(array)
    }

    fn new_object() -> Self::Object {
        Map::new()
    }

    fn push_object(object: &mut Self::Object, key: String, value: Self::Value) {
        if let Some(previous) = object.insert(key, value) {
            drop_serde_value(previous);
            panic!("parser 已拒绝重复 object key");
        }
    }

    fn finish_object(object: Self::Object) -> Self::Value {
        Value::Object(object)
    }

    fn drop_value(value: Self::Value) {
        drop_serde_value(value);
    }
}

enum ParseFrame<B: JsonValueBuilder> {
    Array(B::Array),
    Object {
        entries: B::Object,
        keys: HashSet<String>,
        pending_key: String,
    },
}

struct ParseState<B: JsonValueBuilder> {
    frames: Vec<ParseFrame<B>>,
    completed: Option<B::Value>,
}

impl<B: JsonValueBuilder> ParseState<B> {
    fn new() -> Self {
        Self {
            frames: Vec::new(),
            completed: None,
        }
    }
}

impl<B: JsonValueBuilder> Drop for ParseState<B> {
    fn drop(&mut self) {
        if let Some(value) = self.completed.take() {
            B::drop_value(value);
        }
        while let Some(frame) = self.frames.pop() {
            match frame {
                ParseFrame::Array(array) => B::drop_array(array),
                ParseFrame::Object { entries, .. } => B::drop_object(entries),
            }
        }
    }
}

/// 解析一个完整且严格的 JSON 值。
pub(crate) fn decode(source: &str) -> Result<LosslessJsonValue, LosslessJsonError> {
    match decode_with_builder::<LosslessValueBuilder>(source) {
        Ok(value) => Ok(value),
        Err(ParseValueError::Syntax(source)) => Err(source),
        Err(ParseValueError::Build(source)) => match source {},
    }
}

/// 使用同一解析器直接构造一个完整且严格的 `serde_json::Value`。
///
/// 成功结果可能任意深；调用方必须立即转交栈安全拥有型边界，或使用
/// [`drop_serde_value`] 迭代释放，不能让裸值沿 Rust 调用栈递归析构。
pub(crate) fn decode_value(source: &str) -> Result<Value, JsonValueDecodeError> {
    match decode_with_builder::<SerdeValueBuilder>(source) {
        Ok(value) => Ok(value),
        Err(ParseValueError::Syntax(source)) => Err(JsonValueDecodeError::Syntax(source)),
        Err(ParseValueError::Build(source)) => Err(JsonValueDecodeError::Backend(source)),
    }
}

fn decode_with_builder<B: JsonValueBuilder>(
    source: &str,
) -> Result<B::Value, ParseValueError<B::Error>> {
    let mut parser = Parser {
        source,
        position: 0,
    };
    parser.skip_whitespace();
    let value = parser.parse_value::<B>()?;
    parser.skip_whitespace();
    if parser.position != source.len() {
        let error = parser.syntax("顶层值之后存在额外内容");
        B::drop_value(value);
        return Err(error.into());
    }
    Ok(value)
}

pub(crate) fn drop_serde_value(mut root: Value) {
    let mut pending = Vec::new();
    take_serde_children(&mut root, &mut pending);
    while let Some(mut value) = pending.pop() {
        take_serde_children(&mut value, &mut pending);
    }
}

fn take_serde_children(value: &mut Value, pending: &mut Vec<Value>) {
    match value {
        Value::Array(values) => pending.append(values),
        Value::Object(values) => pending.extend(std::mem::take(values).into_values()),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// 验证文本是否恰好为一个标准 JSON number。
pub(crate) fn validate_number(source: &str) -> Result<(), LosslessJsonError> {
    let end = scan_number(source.as_bytes(), 0)?;
    if end != source.len() {
        return Err(LosslessJsonError::Syntax {
            byte_offset: end,
            reason: "number 之后存在额外内容",
        });
    }
    Ok(())
}

struct Parser<'a> {
    source: &'a str,
    position: usize,
}

impl Parser<'_> {
    fn parse_value<B: JsonValueBuilder>(&mut self) -> Result<B::Value, ParseValueError<B::Error>> {
        let mut state = ParseState::<B>::new();
        loop {
            if let Some(value) = state.completed.take() {
                let Some(frame) = state.frames.last_mut() else {
                    return Ok(value);
                };
                match frame {
                    ParseFrame::Array(values) => B::push_array(values, value),
                    ParseFrame::Object {
                        entries,
                        pending_key,
                        ..
                    } => B::push_object(entries, std::mem::take(pending_key), value),
                }
                self.skip_whitespace();

                let closing = match frame {
                    ParseFrame::Array(_) if self.consume_if(b']') => true,
                    ParseFrame::Object { .. } if self.consume_if(b'}') => true,
                    ParseFrame::Array(_) => {
                        if !self.consume_if(b',') {
                            return Err(self.syntax("JSON array 项之间缺少逗号或结束括号").into());
                        }
                        self.skip_whitespace();
                        if self.peek_byte() == Some(b']') {
                            return Err(self.syntax("JSON array 不允许尾逗号").into());
                        }
                        false
                    }
                    ParseFrame::Object {
                        keys, pending_key, ..
                    } => {
                        if !self.consume_if(b',') {
                            return Err(self.syntax("JSON object 项之间缺少逗号或结束括号").into());
                        }
                        self.skip_whitespace();
                        if self.peek_byte() == Some(b'}') {
                            return Err(self.syntax("JSON object 不允许尾逗号").into());
                        }
                        *pending_key = self.parse_object_key(keys)?;
                        false
                    }
                };
                if closing {
                    state.completed = Some(match state.frames.pop().expect("已检查 frame 存在")
                    {
                        ParseFrame::Array(values) => B::finish_array(values),
                        ParseFrame::Object { entries, .. } => B::finish_object(entries),
                    });
                }
                continue;
            }

            state.completed = match self.peek_byte() {
                Some(b'n') => {
                    self.consume_literal(b"null")?;
                    Some(B::null())
                }
                Some(b't') => {
                    self.consume_literal(b"true")?;
                    Some(B::boolean(true))
                }
                Some(b'f') => {
                    self.consume_literal(b"false")?;
                    Some(B::boolean(false))
                }
                Some(b'"') => Some(B::string(self.parse_string()?)),
                Some(b'[') => {
                    self.position += 1;
                    self.skip_whitespace();
                    if self.consume_if(b']') {
                        Some(B::finish_array(B::new_array()))
                    } else {
                        state.frames.push(ParseFrame::Array(B::new_array()));
                        None
                    }
                }
                Some(b'{') => {
                    self.position += 1;
                    self.skip_whitespace();
                    if self.consume_if(b'}') {
                        Some(B::finish_object(B::new_object()))
                    } else {
                        let mut keys = HashSet::new();
                        let pending_key = self.parse_object_key(&mut keys)?;
                        state.frames.push(ParseFrame::Object {
                            entries: B::new_object(),
                            keys,
                            pending_key,
                        });
                        None
                    }
                }
                Some(b'-' | b'0'..=b'9') => {
                    let start = self.position;
                    self.position = scan_number(self.source.as_bytes(), start)?;
                    Some(
                        B::number(&self.source[start..self.position])
                            .map_err(ParseValueError::Build)?,
                    )
                }
                Some(_) => return Err(self.syntax("期望 JSON value").into()),
                None => return Err(self.syntax("缺少 JSON value").into()),
            };
        }
    }

    fn parse_object_key(
        &mut self,
        keys: &mut HashSet<String>,
    ) -> Result<String, LosslessJsonError> {
        if self.peek_byte() != Some(b'"') {
            return Err(self.syntax("JSON object 键必须是字符串"));
        }
        let key_offset = self.position;
        let key = self.parse_string()?;
        if !keys.insert(key.clone()) {
            return Err(LosslessJsonError::DuplicateObjectKey {
                byte_offset: key_offset,
            });
        }
        self.skip_whitespace();
        if !self.consume_if(b':') {
            return Err(self.syntax("JSON object 键后缺少冒号"));
        }
        self.skip_whitespace();
        Ok(key)
    }

    fn parse_string(&mut self) -> Result<String, LosslessJsonError> {
        debug_assert_eq!(self.peek_byte(), Some(b'"'));
        self.position += 1;
        let mut output = String::new();
        loop {
            let Some(byte) = self.peek_byte() else {
                return Err(self.syntax("JSON string 未闭合"));
            };
            match byte {
                b'"' => {
                    self.position += 1;
                    return Ok(output);
                }
                b'\\' => {
                    self.position += 1;
                    self.parse_escape(&mut output)?;
                }
                0x00..=0x1f => return Err(self.syntax("JSON string 包含未转义控制字符")),
                0x20..=0x7f => {
                    output.push(char::from(byte));
                    self.position += 1;
                }
                _ => {
                    let character = self.source[self.position..]
                        .chars()
                        .next()
                        .expect("position 必须位于 UTF-8 字符边界");
                    output.push(character);
                    self.position += character.len_utf8();
                }
            }
        }
    }

    fn parse_escape(&mut self, output: &mut String) -> Result<(), LosslessJsonError> {
        let Some(escape) = self.peek_byte() else {
            return Err(self.syntax("JSON string 转义被截断"));
        };
        self.position += 1;
        match escape {
            b'"' => output.push('"'),
            b'\\' => output.push('\\'),
            b'/' => output.push('/'),
            b'b' => output.push('\u{0008}'),
            b'f' => output.push('\u{000c}'),
            b'n' => output.push('\n'),
            b'r' => output.push('\r'),
            b't' => output.push('\t'),
            b'u' => {
                let first = self.parse_hex_quad()?;
                let scalar = if (0xd800..=0xdbff).contains(&first) {
                    if !self.consume_if(b'\\') || !self.consume_if(b'u') {
                        return Err(self.syntax("高代理项后缺少低代理项"));
                    }
                    let second = self.parse_hex_quad()?;
                    if !(0xdc00..=0xdfff).contains(&second) {
                        return Err(self.syntax("高代理项后不是低代理项"));
                    }
                    0x1_0000 + ((u32::from(first) - 0xd800) << 10) + (u32::from(second) - 0xdc00)
                } else if (0xdc00..=0xdfff).contains(&first) {
                    return Err(self.syntax("出现孤立低代理项"));
                } else {
                    u32::from(first)
                };
                output.push(
                    char::from_u32(scalar)
                        .ok_or_else(|| self.syntax("Unicode 转义不是有效标量值"))?,
                );
            }
            _ => return Err(self.syntax("JSON string 包含未知转义")),
        }
        Ok(())
    }

    fn parse_hex_quad(&mut self) -> Result<u16, LosslessJsonError> {
        let mut value = 0_u16;
        for _ in 0..4 {
            let Some(byte) = self.peek_byte() else {
                return Err(self.syntax("Unicode 转义被截断"));
            };
            let digit = match byte {
                b'0'..=b'9' => u16::from(byte - b'0'),
                b'a'..=b'f' => u16::from(byte - b'a') + 10,
                b'A'..=b'F' => u16::from(byte - b'A') + 10,
                _ => return Err(self.syntax("Unicode 转义包含非十六进制字符")),
            };
            self.position += 1;
            value = value * 16 + digit;
        }
        Ok(value)
    }

    fn consume_literal(&mut self, literal: &[u8]) -> Result<(), LosslessJsonError> {
        let end = self.position.saturating_add(literal.len());
        if self.source.as_bytes().get(self.position..end) != Some(literal) {
            return Err(self.syntax("JSON literal 无效或被截断"));
        }
        self.position = end;
        Ok(())
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek_byte(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.position += 1;
        }
    }

    fn consume_if(&mut self, expected: u8) -> bool {
        if self.peek_byte() == Some(expected) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn peek_byte(&self) -> Option<u8> {
        self.source.as_bytes().get(self.position).copied()
    }

    const fn syntax(&self, reason: &'static str) -> LosslessJsonError {
        LosslessJsonError::Syntax {
            byte_offset: self.position,
            reason,
        }
    }
}

fn scan_number(bytes: &[u8], start: usize) -> Result<usize, LosslessJsonError> {
    let mut position = start;
    if bytes.get(position) == Some(&b'-') {
        position += 1;
    }

    match bytes.get(position) {
        Some(b'0') => {
            position += 1;
            if matches!(bytes.get(position), Some(b'0'..=b'9')) {
                return Err(number_syntax(position, "JSON number 不允许前导零"));
            }
        }
        Some(b'1'..=b'9') => {
            position += 1;
            while matches!(bytes.get(position), Some(b'0'..=b'9')) {
                position += 1;
            }
        }
        _ => return Err(number_syntax(position, "JSON number 缺少整数部分")),
    }

    if bytes.get(position) == Some(&b'.') {
        position += 1;
        let fraction_start = position;
        while matches!(bytes.get(position), Some(b'0'..=b'9')) {
            position += 1;
        }
        if position == fraction_start {
            return Err(number_syntax(position, "JSON number 小数点后缺少数字"));
        }
    }

    if matches!(bytes.get(position), Some(b'e' | b'E')) {
        position += 1;
        if matches!(bytes.get(position), Some(b'+' | b'-')) {
            position += 1;
        }
        let exponent_start = position;
        while matches!(bytes.get(position), Some(b'0'..=b'9')) {
            position += 1;
        }
        if position == exponent_start {
            return Err(number_syntax(position, "JSON number 指数缺少数字"));
        }
    }

    Ok(position)
}

const fn number_syntax(byte_offset: usize, reason: &'static str) -> LosslessJsonError {
    LosslessJsonError::Syntax {
        byte_offset,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_unicode_escapes_and_preserves_number_text() {
        let value =
            decode(r#"{"文本":"\uD83D\uDE00","numbers":[0,-0,1.25,1e999,9223372036854775808]}"#)
                .unwrap();
        assert_eq!(
            value,
            LosslessJsonValue::Object(vec![
                (
                    "文本".to_owned(),
                    LosslessJsonValue::String("😀".to_owned()),
                ),
                (
                    "numbers".to_owned(),
                    LosslessJsonValue::Array(vec![
                        LosslessJsonValue::Number("0".to_owned()),
                        LosslessJsonValue::Number("-0".to_owned()),
                        LosslessJsonValue::Number("1.25".to_owned()),
                        LosslessJsonValue::Number("1e999".to_owned()),
                        LosslessJsonValue::Number("9223372036854775808".to_owned()),
                    ]),
                ),
            ])
        );
    }

    #[test]
    fn direct_value_builder_preserves_number_and_object_semantics() {
        let source = r#"{"z":-0,"a":1e999,"integer":9223372036854775808,"text":"\uD83D\uDE00"}"#;
        let value = decode_value(source).unwrap();
        let serde_value = serde_json::from_str::<Value>(source).unwrap();
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            serde_json::to_string(&serde_value).unwrap()
        );
        drop_serde_value(value);
        drop_serde_value(serde_value);

        assert!(matches!(
            decode_value(r#"{"a":1,"\u0061":2}"#),
            Err(JsonValueDecodeError::Syntax(
                LosslessJsonError::DuplicateObjectKey { .. }
            ))
        ));
    }

    #[test]
    fn rejects_duplicate_keys_after_escape_decoding() {
        assert!(matches!(
            decode(r#"{"a":1,"\u0061":2}"#),
            Err(LosslessJsonError::DuplicateObjectKey { .. })
        ));
    }

    #[test]
    fn number_validation_is_the_exact_json_grammar() {
        for valid in ["0", "-0", "123", "-12.5", "1e9", "1E+9", "1e-9"] {
            validate_number(valid).unwrap();
        }
        for invalid in ["", "-", "01", "+1", ".1", "1.", "1e", "1  ", "NaN", "inf"] {
            assert!(validate_number(invalid).is_err(), "{invalid:?} 应被拒绝");
        }
    }

    #[test]
    fn rejects_non_standard_or_truncated_json() {
        for invalid in [
            "",
            "[1,]",
            r#"{"a":1,}"#,
            "[1 2]",
            "true false",
            "/*comment*/null",
            r#""\x""#,
            r#""\uD800""#,
            r#""\uDC00""#,
            r#""unterminated"#,
        ] {
            assert!(decode(invalid).is_err(), "{invalid:?} 应被拒绝");
        }
    }

    #[test]
    fn direct_value_builder_cleans_deep_partial_trees_iteratively_on_every_failure_shape() {
        const DEPTH: usize = 20_000;
        let mut deep = "[".repeat(DEPTH);
        deep.push_str("null");
        deep.push_str(&"]".repeat(DEPTH));

        let trailing = format!("{deep}x");
        assert!(matches!(
            decode_value(&trailing),
            Err(JsonValueDecodeError::Syntax(_))
        ));

        let missing_closing = &deep[..deep.len() - 1];
        assert!(matches!(
            decode_value(missing_closing),
            Err(JsonValueDecodeError::Syntax(_))
        ));

        let nested_failure = format!("[{deep},]");
        assert!(matches!(
            decode_value(&nested_failure),
            Err(JsonValueDecodeError::Syntax(_))
        ));

        let duplicate_after_deep = format!(r#"{{"\u0064eep":{deep},"deep":null}}"#);
        assert!(matches!(
            decode_value(&duplicate_after_deep),
            Err(JsonValueDecodeError::Syntax(
                LosslessJsonError::DuplicateObjectKey { .. }
            ))
        ));
    }

    #[test]
    fn deeply_nested_values_parse_clone_compare_and_drop_without_using_the_rust_stack() {
        const DEPTH: usize = 20_000;
        let mut source = "[".repeat(DEPTH);
        source.push_str("null");
        source.push_str(&"]".repeat(DEPTH));

        let value = decode(&source).unwrap();
        let cloned = value.clone();
        assert_eq!(value, cloned);

        let mut current = &value;
        for _ in 0..DEPTH {
            let LosslessJsonValue::Array(values) = current else {
                panic!("每一层都应是 JSON array");
            };
            assert_eq!(values.len(), 1);
            current = &values[0];
        }
        assert!(matches!(current, LosslessJsonValue::Null));
    }

    #[test]
    fn parses_a_seventeen_mibibyte_string_without_an_att_size_check() {
        const PAYLOAD_BYTES: usize = 17 * 1024 * 1024;
        let mut source = String::with_capacity(PAYLOAD_BYTES + 2);
        source.push('"');
        source.extend(std::iter::repeat_n('x', PAYLOAD_BYTES));
        source.push('"');
        let value = decode(&source).unwrap();
        let LosslessJsonValue::String(value) = &value else {
            panic!("顶层值应是字符串")
        };
        assert_eq!(value.len(), PAYLOAD_BYTES);
    }
}
