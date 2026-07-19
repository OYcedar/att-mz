//! RPG Maker 占位符从翻译规划、响应验收到写回检查共同使用的保留信封协议。

use std::error::Error;
use std::fmt;

pub(crate) const PREFIX: &str = "⟦ATT_";
pub(crate) const SUFFIX: &str = "⟧";

/// 使用保留信封包装一段由调用方建立的受信 payload。
pub(crate) fn envelope(payload: &str) -> String {
    format!("{PREFIX}{payload}{SUFFIX}")
}

/// 判断文本是否进入了 ATT token 的保留命名空间。
pub(crate) fn contains_reserved_prefix(text: &str) -> bool {
    text.contains(PREFIX)
}

/// 扫描文本中的全部完整 ATT token 信封。
///
/// 信封内部的 payload 不在这里解释；响应验收器会把完整信封与 Planner
/// 建立的精确 token 集合比较。只要出现未闭合保留前缀，就不能继续把文本
/// 当作普通自然语言处理。
pub(crate) fn scan_envelopes(text: &str) -> Result<Vec<&str>, PlaceholderTokenScanError> {
    let mut envelopes = Vec::new();
    let mut cursor = 0usize;

    while let Some(relative_start) = text[cursor..].find(PREFIX) {
        let start = cursor + relative_start;
        let payload_start = start + PREFIX.len();
        let Some(relative_end) = text[payload_start..].find(SUFFIX) else {
            return Err(PlaceholderTokenScanError::UnclosedReservedPrefix {
                fragment: text[start..].to_owned(),
            });
        };
        let end = payload_start + relative_end + SUFFIX.len();
        envelopes.push(&text[start..end]);
        cursor = end;
    }

    Ok(envelopes)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PlaceholderTokenScanError {
    UnclosedReservedPrefix { fragment: String },
}

impl PlaceholderTokenScanError {
    pub(crate) fn into_fragment(self) -> String {
        match self {
            Self::UnclosedReservedPrefix { fragment } => fragment,
        }
    }
}

impl fmt::Display for PlaceholderTokenScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnclosedReservedPrefix { fragment } => {
                write!(formatter, "ATT token 保留前缀未闭合：{fragment:?}")
            }
        }
    }
}

impl Error for PlaceholderTokenScanError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_format_remains_stable() {
        assert_eq!(
            envelope("ACTOR_NAME_WHOLE_0000"),
            "⟦ATT_ACTOR_NAME_WHOLE_0000⟧"
        );
    }

    #[test]
    fn scanner_returns_every_complete_envelope_in_text_order() {
        let text = "前⟦ATT_A_WHOLE_0000⟧⟦ATT_B_END_0001⟧后";

        assert_eq!(
            scan_envelopes(text).expect("完整信封应该可扫描"),
            ["⟦ATT_A_WHOLE_0000⟧", "⟦ATT_B_END_0001⟧"]
        );
    }

    #[test]
    fn scanner_rejects_an_unclosed_reserved_prefix() {
        assert_eq!(
            scan_envelopes("自然文本⟦ATT_BROKEN").expect_err("未闭合前缀必须失败"),
            PlaceholderTokenScanError::UnclosedReservedPrefix {
                fragment: "⟦ATT_BROKEN".to_owned(),
            }
        );
    }

    #[test]
    fn near_miss_natural_text_does_not_enter_the_reserved_namespace() {
        for text in ["ATT", "⟦ATTENTION⟧", "⟦ATT-自然文本⟧"] {
            assert!(!contains_reserved_prefix(text));
            assert!(
                scan_envelopes(text)
                    .expect("近似自然文本不是协议 token")
                    .is_empty()
            );
        }
    }
}
