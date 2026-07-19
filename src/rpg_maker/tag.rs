//! RPG Maker note 与事件注释共同使用的简单标签扫描。
//!
//! Lua Document 与 WriteBack 必须对 `<name:value>` 的 occurrence 使用同一套字节
//! 边界，否则物化位置无法稳定指回冻结原文。

use std::collections::BTreeMap;
use std::ops::Range;

/// 一个简单标签中值部分的冻结文本跨度。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SimpleTagSpan<'a> {
    name: &'a str,
    value: &'a str,
    value_range: Range<usize>,
    occurrence: usize,
}

impl<'a> SimpleTagSpan<'a> {
    pub(crate) const fn name(&self) -> &'a str {
        self.name
    }

    pub(crate) const fn value(&self) -> &'a str {
        self.value
    }

    pub(crate) fn value_range(&self) -> Range<usize> {
        self.value_range.clone()
    }

    pub(crate) const fn occurrence(&self) -> usize {
        self.occurrence
    }
}

/// 按文本顺序扫描 `<name:value>`，并按标签名独立计算零起 occurrence。
///
/// 该语法只服务 Lua Document 的显式 `note_tag/comment_tag`：使用第一个冒号分隔名称和值；没有冒号、
/// 名称为空或缺少右尖括号的片段不会成为标签。后续扫描从已识别的右尖括号之后继续。
pub(crate) fn simple_tag_spans(text: &str) -> Vec<SimpleTagSpan<'_>> {
    let mut result = Vec::new();
    let mut occurrences = BTreeMap::<&str, usize>::new();
    let mut offset = 0;

    while let Some(relative_open) = text[offset..].find('<') {
        let body_start = offset + relative_open + 1;
        let Some(relative_close) = text[body_start..].find('>') else {
            break;
        };
        let body_end = body_start + relative_close;
        offset = body_end + 1;

        let body = &text[body_start..body_end];
        let Some(colon) = body.find(':') else {
            continue;
        };
        let name = &body[..colon];
        if name.is_empty() {
            continue;
        }
        let value_start = body_start + colon + 1;
        let value = &text[value_start..body_end];
        let occurrence = occurrences.entry(name).or_default();
        result.push(SimpleTagSpan {
            name,
            value,
            value_range: value_start..body_end,
            occurrence: *occurrence,
        });
        *occurrence += 1;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_rules_occurrences_and_exact_value_spans() {
        let text = "前缀<Tag:第一><Other:a:b><Tag:第二\n行><:忽略><NoValue>尾";
        let tags = simple_tag_spans(text);

        assert_eq!(tags.len(), 3);
        assert_eq!(
            (tags[0].name(), tags[0].value(), tags[0].occurrence()),
            ("Tag", "第一", 0)
        );
        assert_eq!(
            (tags[1].name(), tags[1].value(), tags[1].occurrence()),
            ("Other", "a:b", 0)
        );
        assert_eq!(
            (tags[2].name(), tags[2].value(), tags[2].occurrence()),
            ("Tag", "第二\n行", 1)
        );
        for tag in tags {
            assert_eq!(&text[tag.value_range()], tag.value());
        }
    }

    #[test]
    fn first_closing_bracket_terminates_a_value_that_contains_another_opening() {
        let tags = simple_tag_spans("<A:一><B:未闭合<C:三>");

        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].name(), "A");
        assert_eq!(tags[1].name(), "B");
        assert_eq!(tags[1].value(), "未闭合<C:三");
    }
}
