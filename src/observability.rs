//! 跨领域复用的审计身份。
//!
//! 运行身份、事件身份与操作身份分别表达一次命令、一条物理审计记录，以及一对
//! 意图与终态之间的稳定关联。

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

/// 一条审计事件的全局唯一身份。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct EventId(Uuid);

impl EventId {
    pub(crate) const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// 一对审计意图与终态共同使用的稳定操作身份。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct OperationId(Uuid);

impl OperationId {
    pub(crate) const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }
}

impl fmt::Display for OperationId {
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

    #[test]
    fn audit_ids_use_canonical_uuid_text() {
        let uuid =
            Uuid::parse_str("550e8400-e29b-41d4-a716-446655440001").expect("测试 UUID 应合法");

        assert_eq!(EventId::from_uuid(uuid).to_string(), uuid.to_string());
        assert_eq!(OperationId::from_uuid(uuid).to_string(), uuid.to_string());
    }
}
