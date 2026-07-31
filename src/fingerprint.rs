//! 跨职责共享的固定 SHA-256 指纹与无歧义 framing。
//!
//! 调用方仍然拥有字段的领域含义、顺序和取舍；本模块只保证每个 domain 和字段
//! 都以显式标签、固定长度编码及原始字节进入哈希，避免字符串拼接产生边界歧义。

use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;

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

    /// 输出固定 64 位、小写 ASCII 十六进制表示。
    pub(crate) fn hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(SHA256_FINGERPRINT_BYTES * 2);
        for byte in self.0 {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        encoded
    }
}

impl fmt::Debug for Sha256Fingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Sha256Fingerprint({})", self.hex())
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

    /// 保持单个 frame 语义，同时允许调用方在固定大小的数据块之间执行可失败检查。
    ///
    /// 检查失败时当前 hasher 已不完整，调用方必须丢弃它。方法直接接收完整 slice，
    /// 因而声明长度与实际写入长度始终一致，不向调用方开放可伪造的 framing 状态。
    pub(crate) fn try_frame_chunks<E>(
        &mut self,
        tag: u8,
        bytes: &[u8],
        chunk_size: NonZeroUsize,
        mut before_chunk: impl FnMut() -> Result<(), E>,
    ) -> Result<&mut Self, E> {
        let length = u64::try_from(bytes.len()).expect("本目标平台的 usize 必须能表示为 u64");
        self.hasher.update([tag]);
        self.hasher.update(length.to_be_bytes());
        for chunk in bytes.chunks(chunk_size.get()) {
            before_chunk()?;
            self.hasher.update(chunk);
        }
        Ok(self)
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
    fn chunked_frames_are_identical_to_single_update_frames() {
        let lengths = [
            0, 1, 7, 63, 64, 65, 4_095, 4_096, 4_097, 65_535, 65_536, 65_537, 131_111,
        ];
        let chunk_sizes = [1, 3, 64, 4_096, 65_536];

        for length in lengths {
            let mut state = 0x9e37_79b9_u32 ^ u32::try_from(length).unwrap();
            let bytes = (0..length)
                .map(|_| {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    (state >> 24) as u8
                })
                .collect::<Vec<_>>();
            let mut expected = Sha256FramedHasher::new(b"chunk-equivalence");
            expected.frame(7, &bytes);
            let expected = expected.finish();

            for chunk_size in chunk_sizes {
                let mut actual = Sha256FramedHasher::new(b"chunk-equivalence");
                actual
                    .try_frame_chunks(7, &bytes, NonZeroUsize::new(chunk_size).unwrap(), || {
                        Ok::<_, std::convert::Infallible>(())
                    })
                    .unwrap();
                assert_eq!(
                    actual.finish(),
                    expected,
                    "length={length}, chunk_size={chunk_size}"
                );
            }
        }
    }

    #[test]
    fn hexadecimal_projection_is_fixed_width_and_lowercase() {
        let mut bytes = [0; SHA256_FINGERPRINT_BYTES];
        bytes[0] = 0x0a;
        bytes[1] = 0xff;
        bytes[SHA256_FINGERPRINT_BYTES - 1] = 0x5c;
        let encoded = Sha256Fingerprint::from_bytes(bytes).hex();

        assert_eq!(encoded.len(), SHA256_FINGERPRINT_BYTES * 2);
        assert_eq!(&encoded[..4], "0aff");
        assert_eq!(&encoded[encoded.len() - 2..], "5c");
        assert!(encoded.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(encoded, encoded.to_ascii_lowercase());
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
