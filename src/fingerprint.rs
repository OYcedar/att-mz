//! 跨职责共享的固定 SHA-256 指纹与无歧义 framing。
//!
//! 调用方仍然拥有字段的领域含义、顺序和取舍；本模块只保证每个 domain 和字段
//! 都以显式标签、固定长度编码及原始字节进入哈希，避免字符串拼接产生边界歧义。

use std::error::Error;
use std::fmt;

use sha2::{Digest, Sha256};

pub(crate) const SHA256_FINGERPRINT_BYTES: usize = 32;

/// 一个已经完成计算并具有固定宽度的 SHA-256 指纹。
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct Sha256Fingerprint([u8; SHA256_FINGERPRINT_BYTES]);

impl Sha256Fingerprint {
    pub(crate) const fn from_bytes(bytes: [u8; SHA256_FINGERPRINT_BYTES]) -> Self {
        Self(bytes)
    }

    pub(crate) fn from_slice(bytes: &[u8]) -> Result<Self, InvalidSha256FingerprintLength> {
        let bytes = <[u8; SHA256_FINGERPRINT_BYTES]>::try_from(bytes).map_err(|_| {
            InvalidSha256FingerprintLength {
                actual: bytes.len(),
            }
        })?;
        Ok(Self(bytes))
    }

    pub(crate) const fn as_bytes(&self) -> &[u8; SHA256_FINGERPRINT_BYTES] {
        &self.0
    }

    pub(crate) const fn into_bytes(self) -> [u8; SHA256_FINGERPRINT_BYTES] {
        self.0
    }
}

impl fmt::Debug for Sha256Fingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Sha256Fingerprint(")?;
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        formatter.write_str(")")
    }
}

/// 外部持久值不能恢复为固定 SHA-256 指纹。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InvalidSha256FingerprintLength {
    actual: usize,
}

impl InvalidSha256FingerprintLength {
    pub(crate) const fn actual(self) -> usize {
        self.actual
    }
}

impl fmt::Display for InvalidSha256FingerprintLength {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "SHA-256 指纹必须是 {SHA256_FINGERPRINT_BYTES} 字节，实际为 {} 字节",
            self.actual
        )
    }
}

impl Error for InvalidSha256FingerprintLength {}

/// 使用 domain separation 和逐字段 framing 建立 SHA-256 指纹。
pub(crate) struct Sha256FramedHasher {
    hasher: Sha256,
}

impl Sha256FramedHasher {
    /// 为一种确定的领域指纹建立独立哈希空间。
    pub(crate) fn new(domain: &[u8]) -> Self {
        let mut value = Self {
            hasher: Sha256::new(),
        };
        value.frame(0, domain);
        value
    }

    /// 按 `tag + u64 big-endian length + bytes` 写入一个完整字段。
    pub(crate) fn frame(&mut self, tag: u8, bytes: &[u8]) -> &mut Self {
        let length = u64::try_from(bytes.len()).expect("本目标平台的 usize 必须能表示为 u64");
        self.hasher.update([tag]);
        self.hasher.update(length.to_be_bytes());
        self.hasher.update(bytes);
        self
    }

    pub(crate) fn finish(self) -> Sha256Fingerprint {
        Sha256Fingerprint::from_bytes(self.hasher.finalize().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framing_distinguishes_domains_tags_boundaries_and_order() {
        fn fingerprint(domain: &[u8], fields: &[(u8, &[u8])]) -> Sha256Fingerprint {
            let mut hasher = Sha256FramedHasher::new(domain);
            for (tag, bytes) in fields {
                hasher.frame(*tag, bytes);
            }
            hasher.finish()
        }

        let reference = fingerprint(b"translation", &[(1, b"ab"), (2, b"c")]);
        assert_ne!(reference, fingerprint(b"source", &[(1, b"ab"), (2, b"c")]));
        assert_ne!(
            reference,
            fingerprint(b"translation", &[(2, b"ab"), (1, b"c")])
        );
        assert_ne!(
            reference,
            fingerprint(b"translation", &[(1, b"a"), (2, b"bc")])
        );
        assert_ne!(
            reference,
            fingerprint(b"translation", &[(1, b"c"), (2, b"ab")])
        );
    }

    #[test]
    fn digest_storage_requires_exactly_thirty_two_bytes() {
        let bytes = [7_u8; SHA256_FINGERPRINT_BYTES];
        let fingerprint = Sha256Fingerprint::from_slice(&bytes).expect("32 字节应合法");

        assert_eq!(fingerprint.as_bytes(), &bytes);
        assert_eq!(fingerprint.into_bytes(), bytes);
        assert_eq!(
            Sha256Fingerprint::from_slice(&bytes[..31])
                .expect_err("短指纹必须拒绝")
                .actual(),
            31
        );
        assert!(format!("{fingerprint:?}").contains("07070707"));
    }
}
