//! Generic 任意长文本身份的可取消、精确索引。
//!
//! 标准 `HashMap<String, _>` 会在一次不可中断的 `Hash`/`Eq` 调用中扫描整段文本。本模块
//! 只把固定 32 字节指纹交给标准 HashMap，并在同指纹 bucket 内分块精确比较原值。SHA-256
//! 只负责缩小候选范围；碰撞不会改变相等语义。

use std::collections::HashMap;
use std::num::NonZeroUsize;

use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};

const IDENTITY_CANCELLATION_CHECK_BYTES: NonZeroUsize =
    NonZeroUsize::new(64 * 1024).expect("Generic 身份取消检查块大小必须非零");

pub(super) struct FingerprintBucketMap<K, V> {
    buckets: HashMap<Sha256Fingerprint, Vec<(K, V)>>,
    len: usize,
}

impl<K, V> FingerprintBucketMap<K, V> {
    pub(super) fn new() -> Self {
        Self {
            buckets: HashMap::new(),
            len: 0,
        }
    }

    pub(super) fn with_capacity(capacity: usize) -> Self {
        Self {
            buckets: HashMap::with_capacity(capacity),
            len: 0,
        }
    }

    pub(super) const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub(super) fn insert_with<E>(
        &mut self,
        fingerprint: Sha256Fingerprint,
        key: K,
        value: V,
        mut keys_equal: impl FnMut(&K, &K) -> Result<bool, E>,
    ) -> Result<Option<V>, E> {
        let bucket = self.buckets.entry(fingerprint).or_default();
        for (stored_key, stored_value) in bucket.iter_mut() {
            if keys_equal(stored_key, &key)? {
                return Ok(Some(std::mem::replace(stored_value, value)));
            }
        }
        bucket.push((key, value));
        self.len += 1;
        Ok(None)
    }

    pub(super) fn get_with<'a, Q: ?Sized, E>(
        &'a self,
        fingerprint: Sha256Fingerprint,
        key: &Q,
        mut keys_equal: impl FnMut(&K, &Q) -> Result<bool, E>,
    ) -> Result<Option<&'a V>, E> {
        let Some(bucket) = self.buckets.get(&fingerprint) else {
            return Ok(None);
        };
        for (stored_key, value) in bucket {
            if keys_equal(stored_key, key)? {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }

    pub(super) fn contains_with<Q: ?Sized, E>(
        &self,
        fingerprint: Sha256Fingerprint,
        key: &Q,
        keys_equal: impl FnMut(&K, &Q) -> Result<bool, E>,
    ) -> Result<bool, E> {
        self.get_with(fingerprint, key, keys_equal)
            .map(|value| value.is_some())
    }

    pub(super) fn remove_with<Q: ?Sized, E>(
        &mut self,
        fingerprint: Sha256Fingerprint,
        key: &Q,
        mut keys_equal: impl FnMut(&K, &Q) -> Result<bool, E>,
    ) -> Result<Option<V>, E> {
        let Some(bucket) = self.buckets.get_mut(&fingerprint) else {
            return Ok(None);
        };
        let mut found = None;
        for (index, (stored_key, _)) in bucket.iter().enumerate() {
            if keys_equal(stored_key, key)? {
                found = Some(index);
                break;
            }
        }
        let Some(index) = found else {
            return Ok(None);
        };
        let (_, value) = bucket.remove(index);
        let remove_bucket = bucket.is_empty();
        self.len -= 1;
        if remove_bucket {
            self.buckets.remove(&fingerprint);
        }
        Ok(Some(value))
    }
}

pub(super) fn framed_identity_fingerprint_with_cancellation<'a, E>(
    domain: &'static [u8],
    fields: impl IntoIterator<Item = (u8, &'a [u8])>,
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Sha256Fingerprint, E> {
    ensure_running()?;
    let mut hasher = Sha256FramedHasher::new(domain);
    for (tag, bytes) in fields {
        hasher.try_frame_chunks(
            tag,
            bytes,
            IDENTITY_CANCELLATION_CHECK_BYTES,
            &mut ensure_running,
        )?;
    }
    ensure_running()?;
    Ok(hasher.finish())
}

pub(super) fn identity_bytes_equal_with_cancellation<E>(
    left: &[u8],
    right: &[u8],
    mut ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<bool, E> {
    ensure_running()?;
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left
        .chunks(IDENTITY_CANCELLATION_CHECK_BYTES.get())
        .zip(right.chunks(IDENTITY_CANCELLATION_CHECK_BYTES.get()))
    {
        ensure_running()?;
        if left != right {
            return Ok(false);
        }
    }
    ensure_running()?;
    Ok(true)
}

pub(crate) struct CancellableTextMap<K, V> {
    inner: FingerprintBucketMap<K, V>,
}

impl<K: AsRef<str>, V> CancellableTextMap<K, V> {
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: FingerprintBucketMap::with_capacity(capacity),
        }
    }

    pub(crate) fn insert_with_cancellation<E>(
        &mut self,
        key: K,
        value: V,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Option<V>, E> {
        let fingerprint = text_fingerprint_with_cancellation(key.as_ref(), &mut ensure_running)?;
        self.inner
            .insert_with(fingerprint, key, value, |left, right| {
                identity_bytes_equal_with_cancellation(
                    left.as_ref().as_bytes(),
                    right.as_ref().as_bytes(),
                    &mut ensure_running,
                )
            })
    }

    pub(crate) fn get_with_cancellation<E>(
        &self,
        key: &str,
        mut ensure_running: impl FnMut() -> Result<(), E>,
    ) -> Result<Option<&V>, E> {
        let fingerprint = text_fingerprint_with_cancellation(key, &mut ensure_running)?;
        self.inner.get_with(fingerprint, key, |stored, requested| {
            identity_bytes_equal_with_cancellation(
                stored.as_ref().as_bytes(),
                requested.as_bytes(),
                &mut ensure_running,
            )
        })
    }
}

fn text_fingerprint_with_cancellation<E>(
    text: &str,
    ensure_running: impl FnMut() -> Result<(), E>,
) -> Result<Sha256Fingerprint, E> {
    framed_identity_fingerprint_with_cancellation(
        b"att.generic.text-index",
        [(1, text.as_bytes())],
        ensure_running,
    )
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn collision_bucket_uses_exact_text_and_long_scans_remain_cancellable() {
        let collision = Sha256Fingerprint::from_bytes([7; 32]);
        let common_prefix = "x".repeat(IDENTITY_CANCELLATION_CHECK_BYTES.get() * 3);
        let first = format!("{common_prefix}a");
        let second = format!("{common_prefix}b");
        let mut map = FingerprintBucketMap::new();
        let exact = |left: &String, right: &String| {
            identity_bytes_equal_with_cancellation(left.as_bytes(), right.as_bytes(), || {
                Ok::<_, std::convert::Infallible>(())
            })
        };
        assert_eq!(
            map.insert_with(collision, first.clone(), 1, exact).unwrap(),
            None
        );
        assert_eq!(
            map.insert_with(collision, second.clone(), 2, exact)
                .unwrap(),
            None
        );
        assert_eq!(
            map.get_with(collision, &first, |left, right| {
                identity_bytes_equal_with_cancellation(left.as_bytes(), right.as_bytes(), || {
                    Ok::<_, std::convert::Infallible>(())
                })
            })
            .unwrap(),
            Some(&1)
        );
        assert_eq!(
            map.get_with(collision, &second, |left, right| {
                identity_bytes_equal_with_cancellation(left.as_bytes(), right.as_bytes(), || {
                    Ok::<_, std::convert::Infallible>(())
                })
            })
            .unwrap(),
            Some(&2)
        );
        assert_eq!(
            map.insert_with(collision, first.clone(), 3, exact).unwrap(),
            Some(1)
        );
        assert!(
            map.contains_with(collision, &first, |left, right| {
                identity_bytes_equal_with_cancellation(left.as_bytes(), right.as_bytes(), || {
                    Ok::<_, std::convert::Infallible>(())
                })
            })
            .unwrap()
        );
        let lookup_polls = Cell::new(0_usize);
        let cancelled_lookup = map.get_with(collision, &second, |left, right| {
            identity_bytes_equal_with_cancellation(left.as_bytes(), right.as_bytes(), || {
                let next = lookup_polls.get() + 1;
                lookup_polls.set(next);
                (next < 3).then_some(()).ok_or(())
            })
        });
        assert_eq!(cancelled_lookup, Err(()));
        assert_eq!(
            map.remove_with(collision, &first, |left, right| {
                identity_bytes_equal_with_cancellation(left.as_bytes(), right.as_bytes(), || {
                    Ok::<_, std::convert::Infallible>(())
                })
            })
            .unwrap(),
            Some(3)
        );
        assert_eq!(
            map.get_with(collision, &second, |left, right| {
                identity_bytes_equal_with_cancellation(left.as_bytes(), right.as_bytes(), || {
                    Ok::<_, std::convert::Infallible>(())
                })
            })
            .unwrap(),
            Some(&2)
        );
        assert_eq!(
            map.remove_with(collision, &second, |left, right| {
                identity_bytes_equal_with_cancellation(left.as_bytes(), right.as_bytes(), || {
                    Ok::<_, std::convert::Infallible>(())
                })
            })
            .unwrap(),
            Some(2)
        );
        assert!(map.is_empty());

        let exact_polls = Cell::new(0_usize);
        let cancelled_exact =
            identity_bytes_equal_with_cancellation(first.as_bytes(), second.as_bytes(), || {
                let next = exact_polls.get() + 1;
                exact_polls.set(next);
                (next < 3).then_some(()).ok_or(())
            });
        assert_eq!(cancelled_exact, Err(()));

        let polls = Cell::new(0_usize);
        let cancelled = text_fingerprint_with_cancellation(&first, || {
            let next = polls.get() + 1;
            polls.set(next);
            (next < 3).then_some(()).ok_or(())
        });
        assert_eq!(cancelled, Err(()));
    }
}
