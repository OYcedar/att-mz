//! Windows 文件名 ordinal 非大小写敏感身份。
//!
//! 这里集中持有 Win32 UTF-16 code unit uppercase 规则，避免存储、运行时与领域层
//! 分别用 Unicode lowercase 或其他近似规则解释同一条 Windows 路径。它不执行
//! Unicode case folding、规范化或兼容折叠。

use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::io;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Globalization::{LCMAP_UPPERCASE, LCMapStringEx, LOCALE_NAME_INVARIANT};

/// 一段 Windows 名称在 ordinal ignore-case 规则下的稳定身份。
///
/// key 相等当且仅当 `CompareStringOrdinal(..., TRUE)` 判定相等。Win32 ordinal
/// 比较逐 UTF-16 code unit 工作，因此 surrogate 单元必须原样保留；若把合法
/// surrogate pair 整体交给 `LCMapStringEx`，它会错误折叠 NTFS 可并存的补充平面
/// 字符。
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct WindowsOrdinalCaseKey(Vec<u16>);

impl WindowsOrdinalCaseKey {
    pub(crate) fn from_os_str(value: &OsStr) -> Result<Self, WindowsOrdinalCaseKeyError> {
        Self::from_utf16(&value.encode_wide().collect::<Vec<_>>())
    }

    pub(crate) fn from_utf16(input: &[u16]) -> Result<Self, WindowsOrdinalCaseKeyError> {
        if input.is_empty() {
            return Ok(Self(Vec::new()));
        }
        i32::try_from(input.len()).map_err(|_| WindowsOrdinalCaseKeyError::InputTooLarge {
            maximum: i32::MAX as u64,
            observed: input.len() as u64,
        })?;

        let mut output = Vec::with_capacity(input.len());
        let mut offset = 0;
        while offset < input.len() {
            if is_surrogate(input[offset]) {
                output.push(input[offset]);
                offset += 1;
                continue;
            }
            let run_start = offset;
            while offset < input.len() && !is_surrogate(input[offset]) {
                offset += 1;
            }
            map_non_surrogate_run(&input[run_start..offset], &mut output)?;
        }
        Ok(Self(output))
    }

    #[cfg(test)]
    pub(crate) fn units(&self) -> &[u16] {
        &self.0
    }
}

fn is_surrogate(unit: u16) -> bool {
    (0xd800..=0xdfff).contains(&unit)
}

fn map_non_surrogate_run(
    input: &[u16],
    output: &mut Vec<u16>,
) -> Result<(), WindowsOrdinalCaseKeyError> {
    let input_len =
        i32::try_from(input.len()).map_err(|_| WindowsOrdinalCaseKeyError::InputTooLarge {
            maximum: i32::MAX as u64,
            observed: input.len() as u64,
        })?;
    // SAFETY: 输入切片在调用期间有效；第一次调用只查询所需长度，不写入目标缓冲区。
    let required = unsafe {
        LCMapStringEx(
            LOCALE_NAME_INVARIANT,
            LCMAP_UPPERCASE,
            input.as_ptr(),
            input_len,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
            std::ptr::null(),
            0,
        )
    };
    if required == 0 {
        return Err(WindowsOrdinalCaseKeyError::WindowsApi {
            phase: WindowsOrdinalCaseKeyPhase::Measure,
            source: io::Error::last_os_error(),
        });
    }
    let output_start = output.len();
    output.resize(output_start + required as usize, 0);
    // SAFETY: 目标缓冲区使用上一次系统调用给出的精确长度；输入与输出在调用期间有效。
    let written = unsafe {
        LCMapStringEx(
            LOCALE_NAME_INVARIANT,
            LCMAP_UPPERCASE,
            input.as_ptr(),
            input_len,
            output[output_start..].as_mut_ptr(),
            required,
            std::ptr::null(),
            std::ptr::null(),
            0,
        )
    };
    if written == 0 {
        output.truncate(output_start);
        return Err(WindowsOrdinalCaseKeyError::WindowsApi {
            phase: WindowsOrdinalCaseKeyPhase::Map,
            source: io::Error::last_os_error(),
        });
    }
    output.truncate(output_start + written as usize);
    Ok(())
}

/// 建立 Windows 非大小写身份时的精确失败。
#[derive(Debug)]
pub(crate) enum WindowsOrdinalCaseKeyError {
    InputTooLarge {
        maximum: u64,
        observed: u64,
    },
    WindowsApi {
        phase: WindowsOrdinalCaseKeyPhase,
        source: io::Error,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WindowsOrdinalCaseKeyPhase {
    Measure,
    Map,
}

impl fmt::Display for WindowsOrdinalCaseKeyPhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Measure => "测量输出长度",
            Self::Map => "映射名称",
        })
    }
}

impl fmt::Display for WindowsOrdinalCaseKeyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputTooLarge { maximum, observed } => write!(
                formatter,
                "Windows 名称包含 {observed} 个 UTF-16 单元，超过 Win32 API 上限 {maximum}"
            ),
            Self::WindowsApi { phase, source } => {
                write!(
                    formatter,
                    "Win32 在{phase}时无法建立 ordinal case key：{source}"
                )
            }
        }
    }
}

impl Error for WindowsOrdinalCaseKeyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::WindowsApi { source, .. } => Some(source),
            Self::InputTooLarge { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinal_key_matches_windows_bmp_equivalence_matrix() {
        fn key(value: &str) -> WindowsOrdinalCaseKey {
            WindowsOrdinalCaseKey::from_os_str(OsStr::new(value)).unwrap()
        }

        assert_eq!(key("K"), key("k"));
        assert_ne!(key("\u{212a}"), key("K"));
        assert_ne!(key("\u{212a}"), key("k"));
        assert_eq!(key("\u{03a3}"), key("\u{03c3}"));
        assert_ne!(key("\u{03c2}"), key("\u{03a3}"));
        assert_eq!(key("\u{00c5}"), key("\u{00e5}"));
        assert_ne!(key("\u{212b}"), key("\u{00c5}"));
        assert_ne!(key("\u{017f}"), key("S"));
        assert_ne!(key("\u{00df}"), key("\u{1e9e}"));
        assert_ne!(key("\u{00df}"), key("SS"));
    }

    #[test]
    fn supplementary_and_unpaired_surrogates_remain_code_unit_identities() {
        let deseret_upper = WindowsOrdinalCaseKey::from_os_str(OsStr::new("\u{10400}")).unwrap();
        let deseret_lower = WindowsOrdinalCaseKey::from_os_str(OsStr::new("\u{10428}")).unwrap();
        assert_ne!(deseret_upper, deseret_lower);

        assert_eq!(
            WindowsOrdinalCaseKey::from_utf16(&[0xd800, u16::from(b'k'), 0xdc00])
                .unwrap()
                .units(),
            &[0xd800, u16::from(b'K'), 0xdc00]
        );
        assert_ne!(
            WindowsOrdinalCaseKey::from_utf16(&[0xd800]).unwrap(),
            WindowsOrdinalCaseKey::from_utf16(&[0xdc00]).unwrap()
        );
    }

    #[test]
    fn empty_and_embedded_nul_identities_use_explicit_lengths() {
        assert!(
            WindowsOrdinalCaseKey::from_os_str(OsStr::new(""))
                .unwrap()
                .units()
                .is_empty()
        );
        assert_eq!(
            WindowsOrdinalCaseKey::from_utf16(&[u16::from(b'k'), 0, u16::from(b's')])
                .unwrap()
                .units(),
            &[u16::from(b'K'), 0, u16::from(b'S')]
        );
    }
}
