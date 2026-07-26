//! JSON 后端错误在安全诊断中的稳定分类。
//!
//! 这里只投影 `serde_json` 的闭集 Category，不携带原始正文或后端错误文本。

use std::fmt;

/// 安全诊断可以长期依赖的 JSON 错误分类。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JsonErrorCategory {
    Io,
    Syntax,
    Data,
    Eof,
    DuplicateObjectKey,
}

impl JsonErrorCategory {
    pub(crate) const fn storage_name(self) -> &'static str {
        match self {
            Self::Io => "io",
            Self::Syntax => "syntax",
            Self::Data => "data",
            Self::Eof => "eof",
            Self::DuplicateObjectKey => "duplicate_object_key",
        }
    }
}

impl From<serde_json::error::Category> for JsonErrorCategory {
    fn from(category: serde_json::error::Category) -> Self {
        match category {
            serde_json::error::Category::Io => Self::Io,
            serde_json::error::Category::Syntax => Self::Syntax,
            serde_json::error::Category::Data => Self::Data,
            serde_json::error::Category::Eof => Self::Eof,
        }
    }
}

impl From<&serde_json::Error> for JsonErrorCategory {
    fn from(error: &serde_json::Error) -> Self {
        error.classify().into()
    }
}

impl fmt::Display for JsonErrorCategory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.storage_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_json_categories_have_unique_stable_names() {
        let cases = [
            (serde_json::error::Category::Io, "io"),
            (serde_json::error::Category::Syntax, "syntax"),
            (serde_json::error::Category::Data, "data"),
            (serde_json::error::Category::Eof, "eof"),
        ];

        let mut names = std::collections::HashSet::new();
        for (category, expected) in cases {
            let category = JsonErrorCategory::from(category);
            assert_eq!(category.storage_name(), expected);
            assert_eq!(category.to_string(), expected);
            assert!(names.insert(category.storage_name()));
        }
        assert!(names.insert(JsonErrorCategory::DuplicateObjectKey.storage_name()));
    }

    #[test]
    fn errors_map_through_the_typed_category() {
        let error = serde_json::from_str::<serde_json::Value>("{").expect_err("截断 JSON 必须失败");

        assert_eq!(JsonErrorCategory::from(&error), JsonErrorCategory::Eof);
    }
}
