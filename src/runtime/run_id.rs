//! Windows 生产运行身份生成器。

use super::windows::{WindowsFsError, secure_uuid_v4};
use crate::observability::{RunId, RunIdGenerator};

/// 使用 Windows 系统安全随机源建立 UUID v4 运行身份。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct WindowsRunIdGenerator;

impl RunIdGenerator for WindowsRunIdGenerator {
    type Error = WindowsFsError;

    fn generate(&self) -> Result<RunId, Self::Error> {
        secure_uuid_v4("生成运行 ID").map(RunId::from_uuid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_distinct_version_four_ids() {
        let generator = WindowsRunIdGenerator;
        let first = generator.generate().expect("运行身份生成应该成功");
        let second = generator.generate().expect("运行身份生成应该成功");

        assert_ne!(first, second);
        assert_eq!(first.as_uuid().get_version_num(), 4);
        assert_eq!(second.as_uuid().get_version_num(), 4);
        assert_eq!(first.as_uuid().get_variant(), uuid::Variant::RFC4122);
        assert_eq!(second.as_uuid().get_variant(), uuid::Variant::RFC4122);
    }
}
