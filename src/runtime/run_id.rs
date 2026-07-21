//! Windows 生产运行身份生成器。

use super::windows::{WindowsFsError, secure_uuid_v4};
use crate::observability::RunId;

/// 使用 Windows 系统安全随机源建立 UUID v4 运行身份。
pub(crate) fn generate_run_id() -> Result<RunId, WindowsFsError> {
    secure_uuid_v4("生成运行 ID").map(RunId::from_uuid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_distinct_version_four_ids() {
        let first = generate_run_id().expect("运行身份生成应该成功");
        let second = generate_run_id().expect("运行身份生成应该成功");

        assert_ne!(first, second);
        assert_eq!(first.as_uuid().get_version_num(), 4);
        assert_eq!(second.as_uuid().get_version_num(), 4);
        assert_eq!(first.as_uuid().get_variant(), uuid::Variant::RFC4122);
        assert_eq!(second.as_uuid().get_variant(), uuid::Variant::RFC4122);
    }
}
