//! 跨领域复用的运行身份。

use std::fmt;

use uuid::Uuid;

/// 一次命令运行的全局唯一身份。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct RunId(Uuid);

impl RunId {
    pub(crate) const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    #[cfg(test)]
    pub(crate) const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_id_uses_canonical_uuid_text() {
        let id = RunId::from_uuid(
            Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").expect("测试 UUID 应合法"),
        );

        assert_eq!(id.to_string(), "550e8400-e29b-41d4-a716-446655440000");
    }
}
