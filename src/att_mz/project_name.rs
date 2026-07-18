use std::fmt;
use std::str::FromStr;

/// 由 CLI 边界验证后在 MZ 命令域内使用的项目名称。
///
/// 项目名称会被用于稳定选择项目，也可能成为文件名的一部分，因此该类型
/// 在构造时一次建立这两类用途共同需要的不变量。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectName(String);

impl ProjectName {
    /// 返回经验证的原始名称，不做归一化或大小写折叠。
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ProjectName {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate(value)?;
        Ok(Self(value.to_owned()))
    }
}

impl fmt::Display for ProjectName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

fn validate(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("项目名称不能为空".to_owned());
    }

    if value.trim() != value {
        return Err("项目名称不能包含首尾空白".to_owned());
    }

    if matches!(value, "." | "..") {
        return Err("项目名称不能是点目录".to_owned());
    }

    if value.ends_with('.') {
        return Err("项目名称不能以点结尾".to_owned());
    }

    if value
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(".att-"))
    {
        return Err("项目名称不能使用 ATT 根保留命名空间".to_owned());
    }

    if value.chars().any(char::is_control) {
        return Err("项目名称不能包含控制字符".to_owned());
    }

    if value.chars().any(|character| {
        matches!(
            character,
            '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*'
        )
    }) {
        return Err("项目名称包含路径分隔符或 Windows 禁用字符".to_owned());
    }

    let stem = value.split('.').next().unwrap_or(value);
    if is_windows_reserved_device_name(stem) {
        return Err("项目名称不能使用 Windows 保留设备名".to_owned());
    }

    Ok(())
}

fn is_windows_reserved_device_name(value: &str) -> bool {
    if !value.is_ascii() {
        return false;
    }

    if ["CON", "PRN", "AUX", "NUL"]
        .iter()
        .any(|reserved| value.eq_ignore_ascii_case(reserved))
    {
        return true;
    }

    let bytes = value.as_bytes();
    bytes.len() == 4
        && matches!(bytes[3], b'1'..=b'9')
        && (value[..3].eq_ignore_ascii_case("COM") || value[..3].eq_ignore_ascii_case("LPT"))
}

#[cfg(test)]
mod tests {
    use super::ProjectName;

    #[test]
    fn accepts_unicode_internal_spaces_and_meaningful_dots() {
        for value in [
            "游戏 一",
            "Project Alpha",
            "release.v1",
            ".hidden",
            "COM10",
            "a中",
            "aaé",
        ] {
            let name = value.parse::<ProjectName>().expect("名称应该合法");

            assert_eq!(name.as_str(), value);
            assert_eq!(name.to_string(), value);
        }
    }

    #[test]
    fn rejects_unsafe_or_ambiguous_names() {
        for value in [
            "",
            " ",
            " alice",
            "alice ",
            ".",
            "..",
            "alice.",
            ".att-project-locks",
            ".ATT-dirpub-locks",
            ".att-private",
            "a/b",
            "a\\b",
            "a<b",
            "a>b",
            "a:b",
            "a\"b",
            "a|b",
            "a?b",
            "a*b",
            "a\0b",
            "CON",
            "con.txt",
            "PrN.backup",
            "AUX",
            "nul.data",
            "COM1",
            "com9.log",
            "LPT1",
            "lpt9.txt",
        ] {
            assert!(
                value.parse::<ProjectName>().is_err(),
                "{value:?} 应该被拒绝"
            );
        }
    }
}
