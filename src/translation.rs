//! 多个翻译引擎共享且语义一致的翻译能力。

/// 一条译文进入当前项目的受管入口。
///
/// 该闭集同时用于 Current 与 Rejected 状态；把 Current 候选转入 Rejected 时必须原样
/// 保留，导出方不能再从剩余表结构猜测来源。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TranslationOrigin {
    Automatic,
    Manual,
}

impl TranslationOrigin {
    pub(crate) const fn storage_name(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::Manual => "manual",
        }
    }

    pub(crate) fn from_storage_name(value: &str) -> Option<Self> {
        match value {
            "automatic" => Some(Self::Automatic),
            "manual" => Some(Self::Manual),
            _ => None,
        }
    }
}

pub(crate) mod candidate_validation;
pub(crate) mod placeholder;
pub(crate) mod placeholder_projection;
pub(crate) mod placeholder_token;
pub(crate) mod planning_resource;
pub(crate) mod profile;
pub(crate) mod task_planning;
pub(crate) mod task_record;
pub(crate) mod user_message;
