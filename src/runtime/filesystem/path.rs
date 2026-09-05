//! 候选与受限编辑共用的 Windows 名称和相对路径校验。

use super::error::SystemFileSystemError;
use super::workspace::PUBLICATION_DIRECTORY_NAME;
use crate::diagnostic::FileSystemPathViolation;
use crate::windows_path::WindowsOrdinalCaseKey;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Component, Path};

pub(super) fn validate_relative_windows_path(path: &Path) -> Result<(), SystemFileSystemError> {
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(SystemFileSystemError::InvalidPath {
                path: path.to_path_buf(),
                violation: FileSystemPathViolation::OutsideScope,
            });
        };
        validate_windows_name(name, path)?;
    }
    Ok(())
}

pub(super) fn validate_windows_name(
    name: &OsStr,
    full_path: &Path,
) -> Result<(), SystemFileSystemError> {
    let wide: Vec<u16> = name.encode_wide().collect();
    if wide.is_empty()
        || matches!(wide.last(), Some(unit) if *unit == b'.' as u16 || *unit == b' ' as u16)
    {
        return Err(SystemFileSystemError::InvalidPath {
            path: full_path.to_path_buf(),
            violation: FileSystemPathViolation::InvalidWindowsName,
        });
    }
    if wide.iter().any(|unit| {
        matches!(
            *unit,
            0 | 1..=31 | 34 | 42 | 47 | 58 | 60 | 62 | 63 | 92 | 124
        )
    }) {
        return Err(SystemFileSystemError::InvalidPath {
            path: full_path.to_path_buf(),
            violation: FileSystemPathViolation::InvalidWindowsName,
        });
    }
    let base_end = wide
        .iter()
        .position(|unit| *unit == u16::from(b'.'))
        .unwrap_or(wide.len());
    let base = &wide[..base_end];
    let reserved_device = ["CON", "PRN", "AUX", "NUL"]
        .into_iter()
        .any(|name| wide_eq_ascii_ignore_case(base, name))
        || (base.len() == 4
            && (wide_eq_ascii_ignore_case(&base[..3], "COM")
                || wide_eq_ascii_ignore_case(&base[..3], "LPT"))
            && matches!(base[3], unit if unit >= u16::from(b'1') && unit <= u16::from(b'9')));
    if reserved_device || wide_eq_ascii_ignore_case(&wide, PUBLICATION_DIRECTORY_NAME) {
        return Err(SystemFileSystemError::InvalidPath {
            path: full_path.to_path_buf(),
            violation: FileSystemPathViolation::ReservedWindowsName,
        });
    }
    Ok(())
}

fn wide_eq_ascii_ignore_case(wide: &[u16], ascii: &str) -> bool {
    wide.len() == ascii.len()
        && wide.iter().zip(ascii.bytes()).all(|(unit, byte)| {
            *unit <= u16::from(u8::MAX) && (*unit as u8).eq_ignore_ascii_case(&byte)
        })
}

pub(super) fn windows_ordinal_case_key(
    value: &OsStr,
    path: &Path,
) -> Result<WindowsOrdinalCaseKey, SystemFileSystemError> {
    WindowsOrdinalCaseKey::from_os_str(value).map_err(|source| {
        SystemFileSystemError::WindowsOrdinalCaseKey {
            path: path.to_path_buf(),
            source,
        }
    })
}

pub(super) fn windows_ordinal_case_key_from_utf16(
    value: &[u16],
    path: &Path,
) -> Result<WindowsOrdinalCaseKey, SystemFileSystemError> {
    WindowsOrdinalCaseKey::from_utf16(value).map_err(|source| {
        SystemFileSystemError::WindowsOrdinalCaseKey {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(test)]
mod tests;
