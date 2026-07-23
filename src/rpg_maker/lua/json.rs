//! 可信 Lua Host 使用的无损 JSON 值与严格解析边界。

use std::collections::HashSet;
use std::error::Error;
use std::fmt;

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

impl LosslessJsonValue {
    pub(crate) fn take_object_value(&mut self, key: &str) -> Option<Self> {
        let Self::Object(entries) = self else {
            return None;
        };
        entries
            .iter_mut()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| std::mem::replace(value, Self::Null))
    }

    pub(crate) fn take_array_value(&mut self, index: usize) -> Option<Self> {
        let Self::Array(values) = self else {
            return None;
        };
        values
            .get_mut(index)
            .map(|value| std::mem::replace(value, Self::Null))
    }
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

/// 解析一个完整且严格的 JSON 值。
pub(crate) fn decode(source: &str) -> Result<LosslessJsonValue, LosslessJsonError> {
    let mut parser = Parser {
        source,
        position: 0,
    };
    parser.skip_whitespace();
    let value = parser.parse_value()?;
    parser.skip_whitespace();
    if parser.position != source.len() {
        return Err(parser.syntax("顶层值之后存在额外内容"));
    }
    Ok(value)
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
    fn parse_value(&mut self) -> Result<LosslessJsonValue, LosslessJsonError> {
        enum Frame {
            Array(Vec<LosslessJsonValue>),
            Object {
                entries: Vec<(String, LosslessJsonValue)>,
                keys: HashSet<String>,
                pending_key: String,
            },
        }

        let mut frames = Vec::new();
        let mut completed = None;
        loop {
            if let Some(value) = completed.take() {
                let Some(frame) = frames.last_mut() else {
                    return Ok(value);
                };
                match frame {
                    Frame::Array(values) => values.push(value),
                    Frame::Object {
                        entries,
                        pending_key,
                        ..
                    } => entries.push((std::mem::take(pending_key), value)),
                }
                self.skip_whitespace();

                let closing = match frame {
                    Frame::Array(_) if self.consume_if(b']') => true,
                    Frame::Object { .. } if self.consume_if(b'}') => true,
                    Frame::Array(_) => {
                        if !self.consume_if(b',') {
                            return Err(self.syntax("JSON array 项之间缺少逗号或结束括号"));
                        }
                        self.skip_whitespace();
                        if self.peek_byte() == Some(b']') {
                            return Err(self.syntax("JSON array 不允许尾逗号"));
                        }
                        false
                    }
                    Frame::Object {
                        keys, pending_key, ..
                    } => {
                        if !self.consume_if(b',') {
                            return Err(self.syntax("JSON object 项之间缺少逗号或结束括号"));
                        }
                        self.skip_whitespace();
                        if self.peek_byte() == Some(b'}') {
                            return Err(self.syntax("JSON object 不允许尾逗号"));
                        }
                        *pending_key = self.parse_object_key(keys)?;
                        false
                    }
                };
                if closing {
                    completed = Some(match frames.pop().expect("已检查 frame 存在") {
                        Frame::Array(values) => LosslessJsonValue::Array(values),
                        Frame::Object { entries, .. } => LosslessJsonValue::Object(entries),
                    });
                }
                continue;
            }

            completed = match self.peek_byte() {
                Some(b'n') => {
                    self.consume_literal(b"null")?;
                    Some(LosslessJsonValue::Null)
                }
                Some(b't') => {
                    self.consume_literal(b"true")?;
                    Some(LosslessJsonValue::Boolean(true))
                }
                Some(b'f') => {
                    self.consume_literal(b"false")?;
                    Some(LosslessJsonValue::Boolean(false))
                }
                Some(b'"') => Some(LosslessJsonValue::String(self.parse_string()?)),
                Some(b'[') => {
                    self.position += 1;
                    self.skip_whitespace();
                    if self.consume_if(b']') {
                        Some(LosslessJsonValue::Array(Vec::new()))
                    } else {
                        frames.push(Frame::Array(Vec::new()));
                        None
                    }
                }
                Some(b'{') => {
                    self.position += 1;
                    self.skip_whitespace();
                    if self.consume_if(b'}') {
                        Some(LosslessJsonValue::Object(Vec::new()))
                    } else {
                        let mut keys = HashSet::new();
                        let pending_key = self.parse_object_key(&mut keys)?;
                        frames.push(Frame::Object {
                            entries: Vec::new(),
                            keys,
                            pending_key,
                        });
                        None
                    }
                }
                Some(b'-' | b'0'..=b'9') => {
                    let start = self.position;
                    self.position = scan_number(self.source.as_bytes(), start)?;
                    Some(LosslessJsonValue::Number(
                        self.source[start..self.position].to_owned(),
                    ))
                }
                Some(_) => return Err(self.syntax("期望 JSON value")),
                None => return Err(self.syntax("缺少 JSON value")),
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
