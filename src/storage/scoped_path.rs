//! 跨平台保持逐字逻辑身份的受检相对路径与目录项匹配。

use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::os::windows::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use crate::windows_path::{WindowsOrdinalCaseKey, WindowsOrdinalCaseKeyError};

/// 未发布目录候选内的受检相对路径。
///
/// `new` 面向外部逻辑协议，因而只接受 `/`，并逐字拒绝平台可能替调用方
/// 归一化的反斜杠、空段、点段和重复分隔符。`from_internal_path` 只供已经受信的
/// 宿主布局或文件系统枚举结果组合本机路径，并保留原始 Windows UTF-16 名称。
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct ScopedDirectoryPath(PathBuf);

impl ScopedDirectoryPath {
    pub(crate) fn new(path: PathBuf) -> Result<Self, ScopedDirectoryPathError> {
        let Some(raw) = path.to_str() else {
            return Err(ScopedDirectoryPathError { path });
        };
        if raw.is_empty()
            || raw.starts_with('/')
            || raw.ends_with('/')
            || raw.contains("//")
            || raw.contains(['\\', ':'])
            || raw.chars().any(char::is_control)
            || raw.split('/').any(|segment| {
                segment.is_empty() || segment.ends_with(['.', ' ']) || matches!(segment, "." | "..")
            })
        {
            return Err(ScopedDirectoryPathError { path });
        }
        Ok(Self(path))
    }

    /// 只接受由宿主从已验证逻辑路径与固定布局前缀组合出的本机路径。
    pub(crate) fn from_internal_path(path: PathBuf) -> Result<Self, ScopedDirectoryPathError> {
        let mut components = path.components();
        let Some(Component::Normal(root)) = components.next() else {
            return Err(ScopedDirectoryPathError { path });
        };
        if invalid_internal_component(root)
            || components.any(|component| {
                !matches!(component, Component::Normal(name) if !invalid_internal_component(name))
            })
        {
            return Err(ScopedDirectoryPathError { path });
        }
        Ok(Self(path))
    }

    pub(crate) fn as_path(&self) -> &Path {
        &self.0
    }

    pub(crate) fn first_component(&self) -> &OsStr {
        match self.0.components().next() {
            Some(Component::Normal(root)) => root,
            _ => unreachable!("ScopedDirectoryPath 已建立普通相对路径不变量"),
        }
    }

    pub(crate) fn is_top_level(&self) -> bool {
        self.0.components().count() == 1
    }
}

fn invalid_internal_component(component: &OsStr) -> bool {
    let units = component.encode_wide().collect::<Vec<_>>();
    units.is_empty()
        || units.contains(&u16::from(b':'))
        || char::decode_utf16(units.iter().copied()).any(|unit| unit.is_ok_and(char::is_control))
        || matches!(units.last(), Some(unit) if *unit == u16::from(b'.') || *unit == u16::from(b' '))
        || matches!(units.as_slice(), [unit] if *unit == u16::from(b'.'))
        || matches!(
            units.as_slice(),
            [first, second]
                if *first == u16::from(b'.') && *second == u16::from(b'.')
        )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ScopedDirectoryPathError {
    path: PathBuf,
}

impl fmt::Display for ScopedDirectoryPathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "候选编辑路径必须是使用 /、无空段、控制字符、ADS、当前段或父级逃逸的安全相对路径：{}",
            self.path.display()
        )
    }
}

impl Error for ScopedDirectoryPathError {}

/// 精确名称不存在，但同一目录中存在仅大小写不同的真实目录项。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExactPathCaseMismatch {
    requested: PathBuf,
    actual: PathBuf,
}

impl ExactPathCaseMismatch {
    pub(crate) fn requested(&self) -> &Path {
        &self.requested
    }

    pub(crate) fn actual(&self) -> &Path {
        &self.actual
    }
}

impl fmt::Display for ExactPathCaseMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "请求路径 {} 的大小写与真实目录项 {} 不一致",
            self.requested.display(),
            self.actual.display()
        )
    }
}

impl Error for ExactPathCaseMismatch {}

/// 精确目录项解析不能建立 Windows 名称身份。
#[derive(Debug)]
pub(crate) enum ExactDirectoryEntryResolutionError {
    CaseMismatch(ExactPathCaseMismatch),
    CaseKey {
        path: PathBuf,
        source: WindowsOrdinalCaseKeyError,
    },
}

impl fmt::Display for ExactDirectoryEntryResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CaseMismatch(source) => source.fmt(formatter),
            Self::CaseKey { path, source } => write!(
                formatter,
                "无法建立目录项 {} 的 Windows 非大小写身份：{source}",
                path.display()
            ),
        }
    }
}

impl Error for ExactDirectoryEntryResolutionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CaseMismatch(source) => Some(source),
            Self::CaseKey { source, .. } => Some(source),
        }
    }
}

/// 在已经枚举的直接目录项中解析逐字名称；缺失返回 `None`，大小写别名显式失败。
pub(crate) fn resolve_exact_directory_entry<I, P>(
    parent: &Path,
    expected_name: &str,
    entries: I,
) -> Result<Option<PathBuf>, ExactDirectoryEntryResolutionError>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let requested = parent.join(expected_name);
    let expected_key =
        WindowsOrdinalCaseKey::from_os_str(OsStr::new(expected_name)).map_err(|source| {
            ExactDirectoryEntryResolutionError::CaseKey {
                path: requested.clone(),
                source,
            }
        })?;
    let mut case_aliases = Vec::new();
    for entry in entries {
        let entry = entry.as_ref();
        let Some(name) = entry.file_name() else {
            continue;
        };
        if name == OsStr::new(expected_name) {
            return Ok(Some(entry.to_path_buf()));
        }
        let actual_key = WindowsOrdinalCaseKey::from_os_str(name).map_err(|source| {
            ExactDirectoryEntryResolutionError::CaseKey {
                path: entry.to_path_buf(),
                source,
            }
        })?;
        if actual_key == expected_key {
            case_aliases.push(entry.to_path_buf());
        }
    }
    case_aliases.sort();
    if let Some(actual) = case_aliases.into_iter().next() {
        return Err(ExactDirectoryEntryResolutionError::CaseMismatch(
            ExactPathCaseMismatch { requested, actual },
        ));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    #[test]
    fn logical_scoped_paths_do_not_depend_on_platform_normalization() {
        for path in ["data", "data/Actors.json", "js/plugins.js"] {
            assert_eq!(
                ScopedDirectoryPath::new(path.into()).unwrap().as_path(),
                Path::new(path)
            );
        }

        for path in [
            "data/file.",
            "data/file ",
            "data/cache./file",
            "data/cache /file",
        ] {
            assert!(
                ScopedDirectoryPath::from_internal_path(path.into()).is_err(),
                "内部受信构造也必须拒绝 Windows 会归一化的段尾：{path:?}"
            );
        }
        for path in [
            "",
            "/data/file",
            "data/",
            "data//file",
            r"data\file",
            "data/./file",
            "data/../file",
            "data/file:stream",
            "data/file.",
            "data/file ",
            "data/cache./file",
            "data/cache /file",
            "data/line\nfeed",
        ] {
            assert!(ScopedDirectoryPath::new(path.into()).is_err(), "{path:?}");
        }
    }

    #[test]
    fn internal_scoped_paths_preserve_unpaired_windows_surrogates() {
        let high = OsString::from_wide(&[
            u16::from(b'h'),
            u16::from(b'i'),
            u16::from(b'g'),
            u16::from(b'h'),
            0xd800,
        ]);
        let low = OsString::from_wide(&[
            u16::from(b'l'),
            u16::from(b'o'),
            u16::from(b'w'),
            0xdc00,
            u16::from(b'.'),
            u16::from(b'b'),
            u16::from(b'i'),
            u16::from(b'n'),
        ]);
        let path = PathBuf::from("logs").join(&high).join(&low);

        let scoped =
            ScopedDirectoryPath::from_internal_path(path).expect("内部路径必须保留原始 UTF-16");
        let components = scoped
            .as_path()
            .components()
            .map(Component::as_os_str)
            .collect::<Vec<_>>();

        assert_eq!(
            components[1].encode_wide().collect::<Vec<_>>(),
            high.encode_wide().collect::<Vec<_>>()
        );
        assert_eq!(
            components[2].encode_wide().collect::<Vec<_>>(),
            low.encode_wide().collect::<Vec<_>>()
        );
    }

    #[test]
    fn exact_entry_matching_rejects_case_aliases() {
        let parent = Path::new("C:/game/data");
        let entries = [parent.join("actors.json"), parent.join("Items.json")];
        let ExactDirectoryEntryResolutionError::CaseMismatch(mismatch) =
            resolve_exact_directory_entry(parent, "Actors.json", &entries)
                .expect_err("大小写别名必须显式失败")
        else {
            panic!("应保留大小写冲突事实");
        };
        assert_eq!(mismatch.requested(), parent.join("Actors.json"));
        assert_eq!(mismatch.actual(), parent.join("actors.json"));
        assert_eq!(
            resolve_exact_directory_entry(parent, "Items.json", &entries).unwrap(),
            Some(parent.join("Items.json"))
        );
        assert_eq!(
            resolve_exact_directory_entry(parent, "Missing.json", &entries).unwrap(),
            None
        );
    }

    #[test]
    fn exact_entry_matching_uses_windows_ordinal_not_unicode_case_folding() {
        let parent = Path::new("C:/game/data");
        let kelvin = parent.join("\u{212a}.json");

        assert_eq!(
            resolve_exact_directory_entry(parent, "K.json", [&kelvin]).unwrap(),
            None,
            "Kelvin 符号与 ASCII K 在 Windows ordinal 语义下不是别名"
        );

        let lower = parent.join("k.json");
        assert!(matches!(
            resolve_exact_directory_entry(parent, "K.json", [&kelvin, &lower]),
            Err(ExactDirectoryEntryResolutionError::CaseMismatch(ref mismatch))
                if mismatch.actual() == lower
        ));
    }
}
