//! 进入公开诊断前已经完成清理和不变量校验的动态值。

use std::borrow::Cow;
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize, de};

use crate::user_text::sanitize_user_text;

/// 可公开的一般短文本。该类型只负责控制字符清理，不承诺文本可作为标识符。
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub(crate) struct SafeText(String);

impl SafeText {
    pub(crate) fn new(value: impl AsRef<str>) -> Self {
        Self(sanitize_user_text(value.as_ref()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SafeText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SafeText {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let sanitized = sanitize_user_text(&value);
        if sanitized == value {
            Ok(Self(value))
        } else {
            Err(de::Error::custom("安全文本包含禁止公开的控制字符"))
        }
    }
}

/// 已经清理、可安全公开的路径。
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct SafePath(String);

impl SafePath {
    pub(crate) fn new(path: impl AsRef<Path>) -> Self {
        Self(public_path(path))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// 把内部路径转换成唯一的公开文本；Windows verbatim 前缀只用于系统调用。
pub(crate) fn public_path(path: impl AsRef<Path>) -> String {
    let path = path.as_ref().to_string_lossy();
    sanitize_user_text(readable_windows_path(&path).as_ref())
}

fn readable_windows_path(path: &str) -> Cow<'_, str> {
    if let Some(path) = path.strip_prefix(r"\\?\UNC\") {
        Cow::Owned(format!(r"\\{path}"))
    } else if let Some(path) = path.strip_prefix(r"\\?\") {
        Cow::Borrowed(path)
    } else {
        Cow::Borrowed(path)
    }
}

impl fmt::Display for SafePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SafePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let sanitized = sanitize_user_text(&value);
        if sanitized == value {
            Ok(Self(value))
        } else {
            Err(de::Error::custom("安全路径包含禁止公开的控制字符"))
        }
    }
}

/// 已经由领域边界确认非空的公开 ID。
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub(crate) struct SafeIdentifier(String);

impl SafeIdentifier {
    pub(crate) fn new(value: impl AsRef<str>) -> Result<Self, InvalidSafeIdentifier> {
        let value = value.as_ref();
        let sanitized = sanitize_user_text(value);
        if sanitized != value {
            return Err(InvalidSafeIdentifier::UnsafeCharacters);
        }
        if value.trim().is_empty() {
            return Err(InvalidSafeIdentifier::Blank);
        }
        Ok(Self(value.to_owned()))
    }

    /// 调用方已经通过领域类型保证 ID 非空时使用。
    pub(crate) fn from_validated(value: impl AsRef<str>) -> Self {
        Self::new(value).expect("领域标识符必须保持非空")
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SafeIdentifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SafeIdentifier {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let sanitized = sanitize_user_text(&value);
        if sanitized != value {
            return Err(de::Error::custom("安全标识符包含禁止公开的控制字符"));
        }
        Self::new(value).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InvalidSafeIdentifier {
    Blank,
    UnsafeCharacters,
}

impl fmt::Display for InvalidSafeIdentifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Blank => "公开标识符不能为空",
            Self::UnsafeCharacters => "公开标识符包含禁止公开的控制字符",
        })
    }
}

impl std::error::Error for InvalidSafeIdentifier {}

/// UTF-8 字节半开范围。构造时保证起点不大于终点。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ByteRange {
    start: usize,
    end: usize,
}

impl ByteRange {
    pub(crate) fn new(start: usize, end: usize) -> Result<Self, InvalidByteRange> {
        if start <= end {
            Ok(Self { start, end })
        } else {
            Err(InvalidByteRange { start, end })
        }
    }
}

impl fmt::Display for ByteRange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}..{}", self.start, self.end)
    }
}

impl<'de> Deserialize<'de> for ByteRange {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            start: usize,
            end: usize,
        }

        let value = Wire::deserialize(deserializer)?;
        Self::new(value.start, value.end).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InvalidByteRange {
    start: usize,
    end: usize,
}

impl fmt::Display for InvalidByteRange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "UTF-8 字节范围起点 {} 大于终点 {}",
            self.start, self.end
        )
    }
}

impl std::error::Error for InvalidByteRange {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_range_rejects_reversed_bounds() {
        assert!(ByteRange::new(5, 4).is_err());
        assert_eq!(ByteRange::new(4, 5).expect("有效范围").to_string(), "4..5");
    }

    #[test]
    fn safe_identifier_rejects_blank_after_sanitizing() {
        assert!(SafeIdentifier::new("\r\n").is_err());
        assert!(SafeIdentifier::new("safe\r\nforged").is_err());
    }

    #[test]
    fn safe_path_removes_windows_verbatim_prefixes_from_public_paths() {
        assert_eq!(
            SafePath::new(r"\\?\C:\games\sample").as_str(),
            r"C:\games\sample"
        );
        assert_eq!(
            SafePath::new(r"\\?\UNC\server\share\sample").as_str(),
            r"\\server\share\sample"
        );
        assert_eq!(
            SafePath::new(r"C:\games\sample").as_str(),
            r"C:\games\sample"
        );
    }

    #[test]
    fn wire_deserialization_cannot_bypass_safe_value_invariants() {
        assert!(serde_json::from_str::<SafeText>(r#""line\nforged""#).is_err());
        assert!(serde_json::from_str::<SafePath>(r#""C:/safe\rforged""#).is_err());
        assert!(serde_json::from_str::<SafeIdentifier>(r#""   ""#).is_err());
        assert!(serde_json::from_str::<ByteRange>(r#"{"start":9,"end":4}"#).is_err());
        assert!(serde_json::from_str::<ByteRange>(r#"{"start":4,"end":9,"extra":1}"#).is_err());
    }
}
