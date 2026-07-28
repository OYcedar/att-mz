//! ATT 内部共用的无深度上限 JSON 基础能力。
//!
//! `serde_json::Value` 仍是 Standard 业务代码的访问模型；本模块只接管会随输入嵌套
//! 深度增长的解析、克隆、比较、序列化和析构调用栈。全部遍历使用显式堆栈，避免在
//! 删除人为深度限制后把规范内的深值转换成 Rust 栈溢出。

use std::error::Error;
use std::fmt;
use std::ops::{Deref, DerefMut};

use serde_json::{Map, Value};

use crate::json_diagnostic::JsonErrorCategory;
use crate::lossless_json::{
    JsonValueDecodeError, LosslessJsonError, decode_value, drop_serde_value,
};

/// 栈安全 JSON 边界保留的具体解析或编码错误。
#[derive(Debug)]
pub(crate) enum StackSafeJsonError {
    Syntax {
        source: LosslessJsonError,
        line: usize,
        column: usize,
    },
    Backend(serde_json::Error),
}

impl StackSafeJsonError {
    pub(crate) fn line(&self) -> usize {
        match self {
            Self::Syntax { line, .. } => *line,
            Self::Backend(source) => source.line(),
        }
    }

    pub(crate) fn column(&self) -> usize {
        match self {
            Self::Syntax { column, .. } => *column,
            Self::Backend(source) => source.column(),
        }
    }

    pub(crate) fn diagnostic_category(&self) -> JsonErrorCategory {
        match self {
            Self::Syntax {
                source: LosslessJsonError::Syntax { .. },
                ..
            } => JsonErrorCategory::Syntax,
            Self::Syntax {
                source: LosslessJsonError::DuplicateObjectKey { .. },
                ..
            } => JsonErrorCategory::DuplicateObjectKey,
            Self::Backend(source) => JsonErrorCategory::from(source),
        }
    }

    /// 只投影解析器闭集原因和坐标，不公开原始 JSON 或后端错误文本。
    pub(crate) fn safe_diagnostic_detail(&self) -> String {
        match self {
            Self::Syntax {
                source:
                    LosslessJsonError::Syntax {
                        byte_offset,
                        reason: _,
                    },
                line,
                column,
            } => {
                let category = self.diagnostic_category();
                format!(
                    "json_backend=lossless; json_category={category}; byte_offset={byte_offset}; json_line={line}; json_column={column}"
                )
            }
            Self::Syntax {
                source: LosslessJsonError::DuplicateObjectKey { byte_offset },
                line,
                column,
            } => {
                let category = self.diagnostic_category();
                format!(
                    "json_backend=lossless; json_category={category}; byte_offset={byte_offset}; json_line={line}; json_column={column}"
                )
            }
            Self::Backend(source) => {
                let category = self.diagnostic_category();
                format!(
                    "json_backend=serde_json; json_category={category}; json_line={}; json_column={}",
                    source.line(),
                    source.column()
                )
            }
        }
    }
}

impl fmt::Display for StackSafeJsonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax { source, .. } => source.fmt(formatter),
            Self::Backend(source) => source.fmt(formatter),
        }
    }
}

impl Error for StackSafeJsonError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Syntax { source, .. } => Some(source),
            Self::Backend(source) => Some(source),
        }
    }
}

impl From<serde_json::Error> for StackSafeJsonError {
    fn from(source: serde_json::Error) -> Self {
        Self::Backend(source)
    }
}

/// 一个保证克隆、比较和析构不递归进入 JSON 子树的拥有型根值。
pub(crate) struct StackSafeJsonValue {
    value: Option<Value>,
}

impl StackSafeJsonValue {
    pub(crate) fn new(value: Value) -> Self {
        Self { value: Some(value) }
    }

    pub(crate) fn into_inner(mut self) -> Value {
        self.value.take().expect("JSON 根值在消费前必须存在")
    }

    fn value(&self) -> &Value {
        self.value.as_ref().expect("JSON 根值在析构前必须存在")
    }

    fn value_mut(&mut self) -> &mut Value {
        self.value.as_mut().expect("JSON 根值在析构前必须存在")
    }
}

impl Clone for StackSafeJsonValue {
    fn clone(&self) -> Self {
        Self::new(clone_value(self.value()))
    }
}

impl fmt::Debug for StackSafeJsonValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let encoded = to_string(self.value()).map_err(|_| fmt::Error)?;
        formatter.write_str(&encoded)
    }
}

impl PartialEq for StackSafeJsonValue {
    fn eq(&self, other: &Self) -> bool {
        values_equal(self.value(), other.value())
    }
}

impl Eq for StackSafeJsonValue {}

impl Deref for StackSafeJsonValue {
    type Target = Value;

    fn deref(&self) -> &Self::Target {
        self.value()
    }
}

impl DerefMut for StackSafeJsonValue {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.value_mut()
    }
}

impl AsRef<Value> for StackSafeJsonValue {
    fn as_ref(&self) -> &Value {
        self.value()
    }
}

impl From<Value> for StackSafeJsonValue {
    fn from(value: Value) -> Self {
        Self::new(value)
    }
}

impl Drop for StackSafeJsonValue {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            drop_value(value);
        }
    }
}

/// 严格解析一个完整 JSON 值；重复 object key 与 Lua Host 使用相同语义。
pub(crate) fn from_str(source: &str) -> Result<StackSafeJsonValue, StackSafeJsonError> {
    match decode_value(source) {
        Ok(value) => Ok(StackSafeJsonValue::new(value)),
        Err(JsonValueDecodeError::Syntax(error)) => Err(lossless_error(source, error)),
        Err(JsonValueDecodeError::Backend(error)) => Err(StackSafeJsonError::Backend(error)),
    }
}

fn lossless_error(source: &str, error: LosslessJsonError) -> StackSafeJsonError {
    let byte_offset = match &error {
        LosslessJsonError::Syntax { byte_offset, .. }
        | LosslessJsonError::DuplicateObjectKey { byte_offset } => *byte_offset,
    };
    let prefix = &source[..byte_offset.min(source.len())];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map_or(prefix, |(_, tail)| tail)
        .chars()
        .count()
        + 1;
    StackSafeJsonError::Syntax {
        source: error,
        line,
        column,
    }
}

/// 克隆一个任意深的 `serde_json::Value`，不递归进入子树。
pub(crate) fn clone_value(root: &Value) -> Value {
    enum Work<'a> {
        Value(&'a Value),
        Array(usize),
        Object(Vec<String>),
    }

    let mut work = vec![Work::Value(root)];
    let mut completed = Vec::<Value>::new();
    while let Some(item) = work.pop() {
        match item {
            Work::Value(Value::Null) => completed.push(Value::Null),
            Work::Value(Value::Bool(value)) => completed.push(Value::Bool(*value)),
            Work::Value(Value::Number(value)) => completed.push(Value::Number(value.clone())),
            Work::Value(Value::String(value)) => completed.push(Value::String(value.clone())),
            Work::Value(Value::Array(values)) => {
                work.push(Work::Array(values.len()));
                work.extend(values.iter().rev().map(Work::Value));
            }
            Work::Value(Value::Object(entries)) => {
                work.push(Work::Object(entries.keys().cloned().collect()));
                work.extend(entries.values().rev().map(Work::Value));
            }
            Work::Array(length) => {
                let first = completed.len() - length;
                let values = completed.split_off(first);
                completed.push(Value::Array(values));
            }
            Work::Object(keys) => {
                let first = completed.len() - keys.len();
                let values = completed.split_off(first);
                let mut object = Map::with_capacity(keys.len());
                for (key, value) in keys.into_iter().zip(values) {
                    object.insert(key, value);
                }
                completed.push(Value::Object(object));
            }
        }
    }
    completed.pop().expect("JSON 克隆必须产生一个根值")
}

fn values_equal(left: &Value, right: &Value) -> bool {
    let mut work = vec![(left, right)];
    while let Some((left, right)) = work.pop() {
        match (left, right) {
            (Value::Null, Value::Null) => {}
            (Value::Bool(left), Value::Bool(right)) if left == right => {}
            (Value::Number(left), Value::Number(right)) if left == right => {}
            (Value::String(left), Value::String(right)) if left == right => {}
            (Value::Array(left), Value::Array(right)) if left.len() == right.len() => {
                work.extend(left.iter().zip(right));
            }
            (Value::Object(left), Value::Object(right)) if left.len() == right.len() => {
                for (key, left) in left {
                    let Some(right) = right.get(key) else {
                        return false;
                    };
                    work.push((left, right));
                }
            }
            _ => return false,
        }
    }
    true
}

/// 紧凑编码一个任意深的 JSON 值。
pub(crate) fn to_string(value: &Value) -> Result<String, StackSafeJsonError> {
    let bytes = serialize(value, false)?;
    Ok(String::from_utf8(bytes).expect("JSON 编码始终产生 UTF-8"))
}

/// 使用 `serde_json::to_vec_pretty` 相同的两空格布局编码任意深的 JSON 值。
pub(crate) fn to_vec_pretty(value: &Value) -> Result<Vec<u8>, StackSafeJsonError> {
    serialize(value, true)
}

/// 使用 `serde_json::to_string_pretty` 相同的两空格布局编码任意深的 JSON 值。
pub(crate) fn to_string_pretty(value: &Value) -> Result<String, StackSafeJsonError> {
    let bytes = serialize(value, true)?;
    Ok(String::from_utf8(bytes).expect("JSON 编码始终产生 UTF-8"))
}

fn serialize(root: &Value, pretty: bool) -> Result<Vec<u8>, StackSafeJsonError> {
    enum Work<'a> {
        Value(&'a Value, usize),
        Byte(u8),
        Bytes(&'static [u8]),
        Indent(usize),
        String(&'a str),
    }

    let mut output = Vec::new();
    let mut work = vec![Work::Value(root, 0)];
    while let Some(item) = work.pop() {
        match item {
            Work::Value(Value::Null, _) => output.extend_from_slice(b"null"),
            Work::Value(Value::Bool(true), _) => output.extend_from_slice(b"true"),
            Work::Value(Value::Bool(false), _) => output.extend_from_slice(b"false"),
            Work::Value(Value::Number(value), _) => {
                output.extend_from_slice(value.to_string().as_bytes());
            }
            Work::Value(Value::String(value), _) => {
                serde_json::to_writer(&mut output, value).map_err(StackSafeJsonError::Backend)?;
            }
            Work::Value(Value::Array(values), depth) => {
                if values.is_empty() {
                    output.extend_from_slice(b"[]");
                    continue;
                }
                output.push(b'[');
                work.push(Work::Byte(b']'));
                if pretty {
                    work.push(Work::Indent(depth));
                    work.push(Work::Byte(b'\n'));
                }
                for (index, value) in values.iter().enumerate().rev() {
                    work.push(Work::Value(value, depth + 1));
                    if pretty {
                        work.push(Work::Indent(depth + 1));
                    }
                    if index != 0 {
                        if pretty {
                            work.push(Work::Byte(b'\n'));
                        }
                        work.push(Work::Bytes(b","));
                    }
                }
                if pretty {
                    work.push(Work::Byte(b'\n'));
                }
            }
            Work::Value(Value::Object(values), depth) => {
                if values.is_empty() {
                    output.extend_from_slice(b"{}");
                    continue;
                }
                output.push(b'{');
                work.push(Work::Byte(b'}'));
                if pretty {
                    work.push(Work::Indent(depth));
                    work.push(Work::Byte(b'\n'));
                }
                for (index, (key, value)) in values.iter().enumerate().rev() {
                    work.push(Work::Value(value, depth + 1));
                    work.push(Work::Bytes(if pretty { b": " } else { b":" }));
                    work.push(Work::String(key));
                    if pretty {
                        work.push(Work::Indent(depth + 1));
                    }
                    if index != 0 {
                        if pretty {
                            work.push(Work::Byte(b'\n'));
                        }
                        work.push(Work::Bytes(b","));
                    }
                }
                if pretty {
                    work.push(Work::Byte(b'\n'));
                }
            }
            Work::Byte(byte) => output.push(byte),
            Work::Bytes(bytes) => output.extend_from_slice(bytes),
            Work::Indent(depth) => {
                for _ in 0..depth {
                    output.extend_from_slice(b"  ");
                }
            }
            Work::String(value) => {
                serde_json::to_writer(&mut output, value).map_err(StackSafeJsonError::Backend)?
            }
        }
    }
    Ok(output)
}

/// 迭代释放一个任意深的 `serde_json::Value`。
pub(crate) fn drop_value(root: Value) {
    drop_serde_value(root);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deeply_nested_values_parse_clone_compare_serialize_and_drop_iteratively() {
        const DEPTH: usize = 10_000;
        let mut source = "[".repeat(DEPTH);
        source.push_str(r#"{"number":1e999,"negativeZero":-0,"text":"值"}"#);
        source.push_str(&"]".repeat(DEPTH));

        let value = from_str(&source).unwrap();
        let cloned = value.clone();
        assert_eq!(value, cloned);
        let canonical_leaf = serde_json::to_string(
            &serde_json::from_str::<Value>(r#"{"number":1e999,"negativeZero":-0,"text":"值"}"#)
                .unwrap(),
        )
        .unwrap();
        let expected = format!(
            "{}{}{}",
            "[".repeat(DEPTH),
            canonical_leaf,
            "]".repeat(DEPTH)
        );
        assert_eq!(to_string(&value).unwrap(), expected);

        let mut object_source = String::with_capacity(DEPTH * 6 + 4);
        for _ in 0..DEPTH {
            object_source.push_str(r#"{"v":"#);
        }
        object_source.push_str("null");
        object_source.push_str(&"}".repeat(DEPTH));
        let object = from_str(&object_source).unwrap();
        let mut current = object.as_ref();
        for _ in 0..DEPTH {
            current = current.get("v").expect("每层 object 都应保留唯一的 v 属性");
        }
        assert!(current.is_null());
    }

    #[test]
    fn pretty_layout_matches_serde_json_for_regular_values() {
        let source = r#"{"a":[1,true,null],"文本":{"x":"值"}}"#;
        let value = from_str(source).unwrap();
        assert_eq!(
            to_vec_pretty(&value).unwrap(),
            serde_json::to_vec_pretty(&serde_json::from_str::<Value>(source).unwrap()).unwrap()
        );
    }

    #[test]
    fn duplicate_keys_are_rejected_after_escape_decoding() {
        assert!(from_str(r#"{"a":1,"\u0061":2}"#).is_err());

        let error = from_str("{\n \"a\": 1,\n \"a\": 2\n}").unwrap_err();
        assert_eq!((error.line(), error.column()), (3, 2));
        assert!(matches!(
            error,
            StackSafeJsonError::Syntax {
                source: LosslessJsonError::DuplicateObjectKey { byte_offset: 12 },
                line: 3,
                column: 2,
            }
        ));
    }
}
