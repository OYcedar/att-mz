use super::*;

#[test]
fn scoped_paths_only_establish_generic_safe_relative_path_invariants() {
    for path in [
        "assets",
        "assets/catalog.json",
        "scripts",
        "scripts/modules/task.lua",
    ] {
        assert_eq!(
            ScopedDirectoryPath::new(PathBuf::from(path))
                .expect("受限候选路径应合法")
                .as_path(),
            Path::new(path)
        );
    }

    for path in [
        "",
        "../assets/file",
        "assets/../scripts/file",
        "assets/file:stream",
        "C:/assets/file",
        r"assets\catalog.json",
        "assets//catalog.json",
        "assets/catalog.json/",
        "assets/./catalog.json",
        "assets/catalog.json.",
        "assets/catalog.json ",
        "assets/cache./catalog.json",
        "assets/cache /catalog.json",
    ] {
        assert!(
            ScopedDirectoryPath::new(PathBuf::from(path)).is_err(),
            "路径必须拒绝：{path}"
        );
    }
}

#[test]
fn scoped_directory_scope_owns_allowed_top_level_directories() {
    let scope = ScopedDirectoryScope::new([OsString::from("assets"), OsString::from("scripts")])
        .expect("两个普通顶层目录应该可建立编辑范围");
    let assets =
        ScopedDirectoryPath::new(PathBuf::from("assets/image.png")).expect("范围内路径应该合法");
    let outside = ScopedDirectoryPath::new(PathBuf::from("other/catalog.json"))
        .expect("通用安全路径不负责业务范围");
    assert!(scope.contains(&assets));
    assert!(!scope.contains(&outside));
    assert!(scope.is_scope_root(
        &ScopedDirectoryPath::new(PathBuf::from("scripts")).expect("范围根路径应该合法")
    ));
    assert!(matches!(
        ScopedDirectoryScope::new(Vec::<OsString>::new()),
        Err(ScopedDirectoryScopeError::Empty)
    ));
    assert!(matches!(
        ScopedDirectoryScope::new([OsString::from("assets"), OsString::from("assets")]),
        Err(ScopedDirectoryScopeError::DuplicateRoot { .. })
    ));
}
