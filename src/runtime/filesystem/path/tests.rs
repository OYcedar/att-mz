use super::*;
use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStringExt;
use std::path::Path;

#[test]
fn windows_name_validation_rejects_devices_ads_and_reserved_namespace() {
    for name in [
        "CON",
        "nul.txt",
        "file:ads",
        "trailing.",
        ".directory-publish",
    ] {
        assert!(validate_windows_name(OsStr::new(name), Path::new(name)).is_err());
    }
    validate_windows_name(OsStr::new("剧情 数据.json"), Path::new("剧情 数据.json"))
        .expect("Unicode 普通名称应该合法");

    let units = [
        u16::from(b'N'),
        u16::from(b'U'),
        u16::from(b'L'),
        u16::from(b'.'),
        0xd800,
    ];
    let name = OsString::from_wide(&units);
    assert!(
        validate_windows_name(&name, Path::new(&name)).is_err(),
        "孤立 surrogate 不得绕过设备名或发布保留命名空间"
    );
}
