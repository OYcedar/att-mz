//! 跨领域复用的自然运行序号。

use std::fmt;
use std::num::NonZeroU64;

/// 一次项目命令的可读运行序号。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct RunId(NonZeroU64);

impl RunId {
    pub(crate) const fn from_sequence(value: NonZeroU64) -> Self {
        Self(value)
    }

    pub(crate) const fn sequence(self) -> NonZeroU64 {
        self.0
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        let digits = value.strip_prefix("run-")?;
        if digits.len() < 6 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let sequence = digits.parse::<u64>().ok().and_then(NonZeroU64::new)?;
        let run_id = Self(sequence);
        (run_id.to_string() == value).then_some(run_id)
    }

    #[cfg(test)]
    pub(crate) fn for_test(value: u64) -> Self {
        Self(NonZeroU64::new(value).expect("测试运行序号必须大于零"))
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "run-{:06}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_id_uses_zero_padded_natural_text() {
        assert_eq!(RunId::for_test(1).to_string(), "run-000001");
        assert_eq!(RunId::for_test(1_000_000).to_string(), "run-1000000");
        assert_eq!(RunId::parse("run-000001"), Some(RunId::for_test(1)));
        assert_eq!(RunId::parse("run-1"), None);
        assert_eq!(RunId::parse("run-000000"), None);
    }
}
