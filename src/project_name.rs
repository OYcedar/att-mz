use std::fmt;
use std::str::FromStr;

/// 由 CLI 边界验证后供各引擎项目共同使用的稳定项目名称。
///
/// 项目名称会用于选择项目并成为目录名，因此构造时一次建立两种用途共同需要的
/// Windows 文件名不变量。名称按用户输入原样保存，不做大小写或 Unicode 归一化。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProjectName(String);

impl ProjectName {
    pub(crate) fn as_str(&self) -> &str {
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
    if value.eq_ignore_ascii_case(".att-locks") {
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
            ".att-game",
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
            ".att-locks",
            ".ATT-LOCKS",
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
