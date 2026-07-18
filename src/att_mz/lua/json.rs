//! 可信 Lua Host 使用的无损 JSON 值与严格解析边界。

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;

/// 单次 Lua Host 值转换能够占用的资源上限。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HostValueBudget {
    max_bytes: NonZeroUsize,
    max_nodes: NonZeroUsize,
    max_depth: NonZeroUsize,
}

impl HostValueBudget {
    pub(crate) const fn new(
        max_bytes: NonZeroUsize,
        max_nodes: NonZeroUsize,
        max_depth: NonZeroUsize,
    ) -> Self {
        Self {
            max_bytes,
            max_nodes,
            max_depth,
        }
    }

    pub(crate) const fn max_bytes(self) -> NonZeroUsize {
        self.max_bytes
    }

    pub(crate) const fn max_nodes(self) -> NonZeroUsize {
        self.max_nodes
    }

    pub(crate) const fn max_depth(self) -> NonZeroUsize {
        self.max_depth
    }
}

/// 一次 Lua/Host 值转换使用的统一预算计数器。
///
/// 根值深度为 1；容器和标量各计一个节点；字符串与二进制叶子的原始字节共同计入
/// `max_bytes`。协议固定字段名不属于调用值，动态键和值才计入字节预算。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HostValueBudgetTracker {
    budget: HostValueBudget,
    bytes: usize,
    nodes: usize,
}

impl HostValueBudgetTracker {
    pub(crate) const fn new(budget: HostValueBudget) -> Self {
        Self {
            budget,
            bytes: 0,
            nodes: 0,
        }
    }

    pub(crate) fn container(&mut self, depth: usize) -> Result<(), HostValueBudgetExceeded> {
        self.node(depth)
    }

    pub(crate) fn scalar(&mut self, depth: usize) -> Result<(), HostValueBudgetExceeded> {
        self.node(depth)
    }

    pub(crate) fn string(
        &mut self,
        depth: usize,
        value: &str,
    ) -> Result<(), HostValueBudgetExceeded> {
        self.node(depth)?;
        self.bytes(value.len())
    }

    pub(crate) fn binary(
        &mut self,
        depth: usize,
        value: &[u8],
    ) -> Result<(), HostValueBudgetExceeded> {
        self.binary_len(depth, value.len())
    }

    pub(crate) fn binary_len(
        &mut self,
        depth: usize,
        length: usize,
    ) -> Result<(), HostValueBudgetExceeded> {
        self.node(depth)?;
        self.bytes(length)
    }

    fn node(&mut self, depth: usize) -> Result<(), HostValueBudgetExceeded> {
        if depth > self.budget.max_depth.get() {
            return Err(HostValueBudgetExceeded::Depth {
                maximum: self.budget.max_depth.get(),
            });
        }
        self.nodes = self
            .nodes
            .checked_add(1)
            .ok_or(HostValueBudgetExceeded::Nodes {
                maximum: self.budget.max_nodes.get(),
            })?;
        if self.nodes > self.budget.max_nodes.get() {
            return Err(HostValueBudgetExceeded::Nodes {
                maximum: self.budget.max_nodes.get(),
            });
        }
        Ok(())
    }

    fn bytes(&mut self, additional: usize) -> Result<(), HostValueBudgetExceeded> {
        self.bytes = self
            .bytes
            .checked_add(additional)
            .ok_or(HostValueBudgetExceeded::Bytes {
                maximum: self.budget.max_bytes.get(),
            })?;
        if self.bytes > self.budget.max_bytes.get() {
            return Err(HostValueBudgetExceeded::Bytes {
                maximum: self.budget.max_bytes.get(),
            });
        }
        Ok(())
    }
}

/// Lua/Host 值转换超过统一资源预算。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HostValueBudgetExceeded {
    Bytes { maximum: usize },
    Nodes { maximum: usize },
    Depth { maximum: usize },
}

impl fmt::Display for HostValueBudgetExceeded {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bytes { maximum } => write!(formatter, "Host 值字节数超过上限 {maximum}"),
            Self::Nodes { maximum } => write!(formatter, "Host 值节点数超过上限 {maximum}"),
            Self::Depth { maximum } => write!(formatter, "Host 值嵌套深度超过上限 {maximum}"),
        }
    }
}

impl Error for HostValueBudgetExceeded {}

/// 保留 JSON number 原文的中间值。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum LosslessJsonValue {
    Null,
    Boolean(bool),
    String(String),
    Number(String),
    Array(Vec<Self>),
    Object(Vec<(String, Self)>),
}

/// 严格 JSON 解析或资源预算失败。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LosslessJsonError {
    InputTooLarge {
        actual: usize,
        maximum: usize,
    },
    NodeLimitExceeded {
        maximum: usize,
    },
    DepthLimitExceeded {
        maximum: usize,
    },
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
            Self::InputTooLarge { actual, maximum } => {
                write!(formatter, "JSON 输入为 {actual} 字节，超过上限 {maximum}")
            }
            Self::NodeLimitExceeded { maximum } => {
                write!(formatter, "JSON 节点数超过上限 {maximum}")
            }
            Self::DepthLimitExceeded { maximum } => {
                write!(formatter, "JSON 嵌套深度超过上限 {maximum}")
            }
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

/// 解析一个完整、严格且受预算约束的 JSON 值。
pub(crate) fn decode(
    source: &str,
    budget: HostValueBudget,
) -> Result<LosslessJsonValue, LosslessJsonError> {
    if source.len() > budget.max_bytes.get() {
        return Err(LosslessJsonError::InputTooLarge {
            actual: source.len(),
            maximum: budget.max_bytes.get(),
        });
    }
    let mut parser = Parser {
        source,
        position: 0,
        nodes: 0,
        budget,
    };
    parser.skip_whitespace();
    let value = parser.parse_value(1)?;
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
    nodes: usize,
    budget: HostValueBudget,
}

impl Parser<'_> {
    fn parse_value(&mut self, depth: usize) -> Result<LosslessJsonValue, LosslessJsonError> {
        if depth > self.budget.max_depth.get() {
            return Err(LosslessJsonError::DepthLimitExceeded {
                maximum: self.budget.max_depth.get(),
            });
        }
        self.nodes = self
            .nodes
            .checked_add(1)
            .ok_or(LosslessJsonError::NodeLimitExceeded {
                maximum: self.budget.max_nodes.get(),
            })?;
        if self.nodes > self.budget.max_nodes.get() {
            return Err(LosslessJsonError::NodeLimitExceeded {
                maximum: self.budget.max_nodes.get(),
            });
        }

        match self.peek_byte() {
            Some(b'n') => {
                self.consume_literal(b"null")?;
                Ok(LosslessJsonValue::Null)
            }
            Some(b't') => {
                self.consume_literal(b"true")?;
                Ok(LosslessJsonValue::Boolean(true))
            }
            Some(b'f') => {
                self.consume_literal(b"false")?;
                Ok(LosslessJsonValue::Boolean(false))
            }
            Some(b'"') => self.parse_string().map(LosslessJsonValue::String),
            Some(b'[') => self.parse_array(depth),
            Some(b'{') => self.parse_object(depth),
            Some(b'-' | b'0'..=b'9') => {
                let start = self.position;
                self.position = scan_number(self.source.as_bytes(), start)?;
                Ok(LosslessJsonValue::Number(
                    self.source[start..self.position].to_owned(),
                ))
            }
            Some(_) => Err(self.syntax("期望 JSON value")),
            None => Err(self.syntax("缺少 JSON value")),
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<LosslessJsonValue, LosslessJsonError> {
        self.position += 1;
        self.skip_whitespace();
        let mut values = Vec::new();
        if self.consume_if(b']') {
            return Ok(LosslessJsonValue::Array(values));
        }
        loop {
            values.push(self.parse_value(depth + 1)?);
            self.skip_whitespace();
            if self.consume_if(b']') {
                return Ok(LosslessJsonValue::Array(values));
            }
            if !self.consume_if(b',') {
                return Err(self.syntax("JSON array 项之间缺少逗号或结束括号"));
            }
            self.skip_whitespace();
            if self.peek_byte() == Some(b']') {
                return Err(self.syntax("JSON array 不允许尾逗号"));
            }
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<LosslessJsonValue, LosslessJsonError> {
        self.position += 1;
        self.skip_whitespace();
        let mut entries = Vec::new();
        let mut keys = HashSet::new();
        if self.consume_if(b'}') {
            return Ok(LosslessJsonValue::Object(entries));
        }
        loop {
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
            let value = self.parse_value(depth + 1)?;
            entries.push((key, value));
            self.skip_whitespace();
            if self.consume_if(b'}') {
                return Ok(LosslessJsonValue::Object(entries));
            }
            if !self.consume_if(b',') {
                return Err(self.syntax("JSON object 项之间缺少逗号或结束括号"));
            }
            self.skip_whitespace();
            if self.peek_byte() == Some(b'}') {
                return Err(self.syntax("JSON object 不允许尾逗号"));
            }
        }
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

    fn budget(bytes: usize, nodes: usize, depth: usize) -> HostValueBudget {
        HostValueBudget::new(
            NonZeroUsize::new(bytes).unwrap(),
            NonZeroUsize::new(nodes).unwrap(),
            NonZeroUsize::new(depth).unwrap(),
        )
    }

    #[test]
    fn decodes_unicode_escapes_and_preserves_number_text() {
        let value = decode(
            r#"{"文本":"\uD83D\uDE00","numbers":[0,-0,1.25,1e999,9223372036854775808]}"#,
            budget(1024, 16, 4),
        )
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
            decode(r#"{"a":1,"\u0061":2}"#, budget(64, 8, 3)),
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
            assert!(
                decode(invalid, budget(1024, 32, 8)).is_err(),
                "{invalid:?} 应被拒绝"
            );
        }
    }

    #[test]
    fn enforces_bytes_nodes_and_root_based_depth() {
        assert!(matches!(
            decode("   null", budget(6, 2, 2)),
            Err(LosslessJsonError::InputTooLarge { .. })
        ));
        assert!(matches!(
            decode("[1,2]", budget(32, 2, 2)),
            Err(LosslessJsonError::NodeLimitExceeded { maximum: 2 })
        ));
        assert!(matches!(
            decode("[[1]]", budget(32, 3, 2)),
            Err(LosslessJsonError::DepthLimitExceeded { maximum: 2 })
        ));
        decode("[1,2]", budget(32, 3, 2)).unwrap();
    }

    #[test]
    fn host_value_tracker_applies_one_root_based_model_to_all_dimensions() {
        let mut bytes = HostValueBudgetTracker::new(budget(3, 8, 4));
        bytes.container(1).unwrap();
        assert!(matches!(bytes.string(2, "四"), Ok(())));
        assert!(matches!(bytes.scalar(2), Ok(())));
        assert!(matches!(
            bytes.string(2, "x"),
            Err(HostValueBudgetExceeded::Bytes { maximum: 3 })
        ));

        let mut nodes = HostValueBudgetTracker::new(budget(64, 2, 4));
        nodes.container(1).unwrap();
        nodes.scalar(2).unwrap();
        assert!(matches!(
            nodes.scalar(2),
            Err(HostValueBudgetExceeded::Nodes { maximum: 2 })
        ));

        let mut depth = HostValueBudgetTracker::new(budget(64, 8, 2));
        depth.container(1).unwrap();
        assert!(matches!(
            depth.scalar(3),
            Err(HostValueBudgetExceeded::Depth { maximum: 2 })
        ));
    }
}
