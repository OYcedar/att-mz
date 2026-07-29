//! 可信 Lua 的纯 Lua 模块查找。
//!
//! `require` 仍由 Lua 5.4 负责缓存、循环检测和调用 searcher。本模块只接管
//! Windows 路径需要保持 Unicode 的文件查找，并把主程序快照目录与
//! `package.path` 分成两个独立 searcher。

use std::fmt::Write as _;
use std::fs::File;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use mlua::{Lua, MultiValue, Table, Value};

pub(super) fn configure_module_paths(lua: &Lua, script_directory: &Path) -> mlua::Result<()> {
    let package: Table = lua.globals().get("package")?;
    let current_searchers: Table = package.get("searchers")?;
    let preload: Value = current_searchers.raw_get(1)?;

    let searchers = lua.create_table()?;
    searchers.raw_set(1, preload)?;
    searchers.raw_set(
        2,
        create_main_directory_searcher(lua, script_directory.to_path_buf())?,
    )?;
    searchers.raw_set(3, create_package_path_searcher(lua, package.clone())?)?;
    package.set("searchers", searchers)?;
    package.set("searchpath", create_package_searchpath(lua)?)?;
    package.set("cpath", Value::Nil)?;
    package.set("loadlib", Value::Nil)
}

pub(super) fn script_directory(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

/// 生成不会把无效 UTF-16 放进 Lua string 的 chunk 名。
///
/// 有效 Unicode 路径原样显示；只有 Lua string 无法表达的 Windows 路径才逐个
/// UTF-16 code unit 编码。真实文件访问始终使用原始 `PathBuf`。
pub(super) fn safe_path_identity(path: &Path) -> String {
    if let Some(path) = path.to_str() {
        return format!("@{path}");
    }

    let mut identity = String::from("@att-utf16");
    for unit in path.as_os_str().encode_wide() {
        write!(&mut identity, "-{unit:04X}").expect("写入 String 不会失败");
    }
    identity
}

fn create_main_directory_searcher(
    lua: &Lua,
    script_directory: PathBuf,
) -> mlua::Result<mlua::Function> {
    lua.create_function(move |lua, module: mlua::LuaString| {
        let module = strict_utf8(&module, "Lua 模块名")?;
        load_first_module(lua, local_lua_module_candidates(&script_directory, &module))
    })
}

fn create_package_path_searcher(lua: &Lua, package: Table) -> mlua::Result<mlua::Function> {
    lua.create_function(move |lua, module: mlua::LuaString| {
        let module = strict_utf8(&module, "Lua 模块名")?;
        let templates = package_path(&package)?;
        load_first_module(
            lua,
            searchpath_candidates(&module, &templates, ".", std::path::MAIN_SEPARATOR_STR),
        )
    })
}

fn create_package_searchpath(lua: &Lua) -> mlua::Result<mlua::Function> {
    lua.create_function(
        |lua,
         (name, path, separator, replacement): (
            mlua::LuaString,
            mlua::LuaString,
            Option<mlua::LuaString>,
            Option<mlua::LuaString>,
        )| {
            let name = strict_utf8(&name, "package.searchpath 的模块名")?;
            let path = strict_utf8(&path, "package.searchpath 的路径模板")?;
            let separator = separator
                .as_ref()
                .map(|value| strict_utf8(value, "package.searchpath 的名称分隔符"))
                .transpose()?
                .unwrap_or_else(|| ".".to_owned());
            let replacement = replacement
                .as_ref()
                .map(|value| strict_utf8(value, "package.searchpath 的目录分隔符"))
                .transpose()?
                .unwrap_or_else(|| std::path::MAIN_SEPARATOR_STR.to_owned());

            let candidates = searchpath_candidates(&name, &path, &separator, &replacement);
            let mut diagnostics = String::new();
            for candidate in candidates {
                match File::open(&candidate) {
                    Ok(file) => {
                        drop(file);
                        let candidate = candidate
                            .to_str()
                            .expect("UTF-8 模板和模块名生成的路径必须保持 UTF-8");
                        return Ok(MultiValue::from_vec(vec![Value::String(
                            lua.create_string(candidate)?,
                        )]));
                    }
                    Err(error) => {
                        append_searchpath_diagnostic(&mut diagnostics, &candidate, &error);
                    }
                }
            }

            Ok(MultiValue::from_vec(vec![
                Value::Nil,
                Value::String(lua.create_string(diagnostics)?),
            ]))
        },
    )
}

fn load_first_module(lua: &Lua, candidates: Vec<PathBuf>) -> mlua::Result<MultiValue> {
    let mut diagnostics = String::new();
    for candidate in candidates {
        match std::fs::read(&candidate) {
            Ok(source) => {
                let chunk_name = safe_path_identity(&candidate);
                let loader_data = loader_data(&candidate);
                let loader = lua.load(source).set_name(&chunk_name).into_function()?;
                return Ok(MultiValue::from_vec(vec![
                    Value::Function(loader),
                    Value::String(lua.create_string(loader_data)?),
                ]));
            }
            Err(error) => {
                let path = loader_data(&candidate);
                let _ = write!(diagnostics, "\n\tno file '{path}' ({error})");
            }
        }
    }

    Ok(MultiValue::from_vec(vec![Value::String(
        lua.create_string(diagnostics)?,
    )]))
}

fn strict_utf8(value: &mlua::LuaString, subject: &str) -> mlua::Result<String> {
    value
        .to_str()
        .map(|value| value.to_owned())
        .map_err(|_| mlua::Error::runtime(format!("{subject}不是 UTF-8 字符串")))
}

fn package_path(package: &Table) -> mlua::Result<String> {
    let path: mlua::LuaString = package
        .get("path")
        .map_err(|error| mlua::Error::runtime(format!("无法读取 package.path：{error}")))?;
    strict_utf8(&path, "package.path ")
}

fn local_lua_module_candidates(script_directory: &Path, module: &str) -> Vec<PathBuf> {
    let module_path = PathBuf::from(module.replace('.', std::path::MAIN_SEPARATOR_STR));
    let mut direct = script_directory.join(&module_path);
    direct.set_extension("lua");
    vec![direct, script_directory.join(module_path).join("init.lua")]
}

fn searchpath_candidates(
    module: &str,
    templates: &str,
    separator: &str,
    replacement: &str,
) -> Vec<PathBuf> {
    let module_path = if separator.is_empty() {
        module.to_owned()
    } else {
        module.replace(separator, replacement)
    };
    templates
        .split(';')
        .map(|template| PathBuf::from(template.replace('?', &module_path)))
        .collect()
}

fn loader_data(path: &Path) -> String {
    path.to_str()
        .map(str::to_owned)
        .unwrap_or_else(|| safe_path_identity(path))
}

fn append_searchpath_diagnostic(
    diagnostics: &mut String,
    candidate: &Path,
    error: &std::io::Error,
) {
    diagnostics.push_str("\n\t");
    let path = loader_data(candidate);
    let _ = write!(diagnostics, "no file '{path}' ({error})");
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    use mlua::{LuaOptions, StdLib};

    use super::*;

    fn configured_lua(script_directory: &Path) -> Lua {
        // SAFETY: 测试与生产契约相同，只运行本测试构造的可信脚本。
        let lua = unsafe { Lua::unsafe_new_with(StdLib::ALL, LuaOptions::default()) };
        configure_module_paths(&lua, script_directory).unwrap();
        lua
    }

    fn lua_path(path: &Path) -> &str {
        path.to_str().expect("测试路径必须是有效 Unicode")
    }

    #[test]
    fn searchers_use_preload_then_main_directory_then_package_path() {
        let root = tempfile::tempdir().unwrap();
        let package_directory = root.path().join("外部 模块😀");
        std::fs::create_dir(&package_directory).unwrap();
        std::fs::write(root.path().join("shared.lua"), "return { origin = 'main' }").unwrap();
        std::fs::write(
            package_directory.join("shared.lua"),
            "return { origin = 'path' }",
        )
        .unwrap();
        std::fs::write(
            package_directory.join("path_only.lua"),
            "return { origin = 'path' }",
        )
        .unwrap();

        let lua = configured_lua(root.path());
        let package: Table = lua.globals().get("package").unwrap();
        package
            .set("path", format!("{}\\?.lua", lua_path(&package_directory)))
            .unwrap();
        lua.load(
            r#"
assert(#package.searchers == 3)
assert(package.cpath == nil)
assert(package.loadlib == nil)
package.preload.shared = function() return { origin = "preload" } end
assert(require("shared").origin == "preload")
package.loaded.shared = nil
package.preload.shared = nil
local from_main, main_loader_data = require("shared")
assert(from_main.origin == "main")
assert(string.find(main_loader_data, "shared.lua", 1, true) ~= nil)
local from_path, path_loader_data = require("path_only")
assert(from_path.origin == "path")
assert(string.find(path_loader_data, "外部 模块😀", 1, true) ~= nil)
"#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn main_directory_hit_does_not_read_invalid_package_path() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("local_only.lua"), "return 'local'").unwrap();

        let lua = configured_lua(root.path());
        let package: Table = lua.globals().get("package").unwrap();
        package
            .set("path", lua.create_string([0xff]).unwrap())
            .unwrap();

        let value: String = lua.load("return require('local_only')").eval().unwrap();
        assert_eq!(value, "local");

        let error = lua.load("return require('missing')").exec().unwrap_err();
        assert!(error.to_string().contains("package.path 不是 UTF-8 字符串"));
    }

    #[test]
    fn captured_package_path_is_dynamic_and_global_rebinding_does_not_replace_it() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        std::fs::write(first.join("dynamic.lua"), "return 'first'").unwrap();
        std::fs::write(second.join("dynamic.lua"), "return 'second'").unwrap();

        let lua = configured_lua(root.path());
        let package: Table = lua.globals().get("package").unwrap();
        lua.globals()
            .set("original_package", package.clone())
            .unwrap();
        lua.globals()
            .set("first_path", format!("{}\\?.lua", lua_path(&first)))
            .unwrap();
        lua.globals()
            .set("second_path", format!("{}\\?.lua", lua_path(&second)))
            .unwrap();
        lua.load(
            r#"
original_package.path = first_path
assert(require("dynamic") == "first")
original_package.path = second_path
assert(require("dynamic") == "first")
original_package.loaded.dynamic = nil
package = { path = "\255" }
assert(require("dynamic") == "second")
"#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn package_path_miss_continues_to_author_searcher() {
        let root = tempfile::tempdir().unwrap();
        let lua = configured_lua(root.path());
        lua.load(
            r#"
package.path = "missing\\?.lua"
package.searchers[4] = function(name)
  return function(loader_name, loader_data)
    assert(loader_name == name)
    assert(loader_data == "author-data")
    return { origin = "author" }
  end, "author-data"
end
local value, loader_data = require("author_module")
assert(value.origin == "author")
assert(loader_data == "author-data")
"#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn main_directory_and_package_path_support_init_lua_candidates() {
        let root = tempfile::tempdir().unwrap();
        let package_directory = root.path().join("外部 init 模块");
        std::fs::create_dir_all(root.path().join("main_init")).unwrap();
        std::fs::create_dir_all(package_directory.join("path_init")).unwrap();
        std::fs::write(root.path().join("main_init/init.lua"), "return 'main-init'").unwrap();
        std::fs::write(
            package_directory.join("path_init/init.lua"),
            "return 'path-init'",
        )
        .unwrap();

        let lua = configured_lua(root.path());
        let package: Table = lua.globals().get("package").unwrap();
        package
            .set(
                "path",
                format!(
                    "{}\\?.lua;{}\\?\\init.lua",
                    lua_path(&package_directory),
                    lua_path(&package_directory)
                ),
            )
            .unwrap();
        lua.load(
            r#"
assert(require("main_init") == "main-init")
assert(require("path_init") == "path-init")
"#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn package_searchpath_supports_unicode_long_paths_and_lua_54_return_shape() {
        let root = tempfile::tempdir().unwrap();
        let mut directory = root.path().join("中文 空格😀");
        while directory.as_os_str().encode_wide().count() < 280 {
            directory = directory.join("很长的目录段");
        }
        let module_directory = directory.join("nested");
        std::fs::create_dir_all(&module_directory).unwrap();
        let module = module_directory.join("module.lua");
        std::fs::write(&module, "return true").unwrap();

        let lua = configured_lua(root.path());
        lua.globals()
            .set(
                "search_template",
                format!("{}\\?.lua", lua_path(&directory)),
            )
            .unwrap();
        lua.globals()
            .set("expected_module", lua_path(&module))
            .unwrap();
        lua.load(
            r#"
local found, extra = package.searchpath("nested.module", search_template)
assert(found == expected_module)
assert(extra == nil)
local custom = package.searchpath("nested/module", search_template, "/", "\\")
assert(custom == expected_module)
local missing, diagnostic = package.searchpath("nested.missing", search_template)
assert(missing == nil)
assert(string.find(diagnostic, "nested\\missing.lua", 1, true) ~= nil)
"#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn package_searchpath_keeps_empty_templates_in_its_diagnostic() {
        let root = tempfile::tempdir().unwrap();
        let lua = configured_lua(root.path());
        let diagnostic: String = lua
            .load(
                r#"
local found, diagnostic = package.searchpath("module", ";missing;")
assert(found == nil)
return diagnostic
"#,
            )
            .eval()
            .unwrap();

        assert_eq!(diagnostic.matches("no file ''").count(), 2);
        assert!(diagnostic.contains("no file 'missing'"));

        let empty: String = lua
            .load(
                r#"
local found, diagnostic = package.searchpath("module", "")
assert(found == nil)
return diagnostic
"#,
            )
            .eval()
            .unwrap();
        assert!(empty.starts_with("\n\tno file ''"));
    }

    #[test]
    fn relative_package_path_is_resolved_from_process_current_directory() {
        let current_directory = std::env::current_dir().unwrap();
        let relative_root = tempfile::Builder::new()
            .prefix("att-lua-relative-")
            .tempdir_in(&current_directory)
            .unwrap();
        std::fs::write(
            relative_root.path().join("relative_module.lua"),
            "return 'from-cwd'",
        )
        .unwrap();
        let relative_root = relative_root
            .path()
            .strip_prefix(&current_directory)
            .expect("测试临时目录应位于进程 cwd")
            .to_path_buf();
        let script_directory = tempfile::tempdir().unwrap();
        let lua = configured_lua(script_directory.path());
        let package: Table = lua.globals().get("package").unwrap();
        package
            .set("path", format!("{}\\?.lua", lua_path(&relative_root)))
            .unwrap();

        let value: String = lua
            .load("return require('relative_module')")
            .eval()
            .unwrap();
        assert_eq!(value, "from-cwd");
    }

    #[test]
    fn candidate_construction_preserves_unc_and_custom_separator_semantics() {
        let candidates = searchpath_candidates(
            "目录.module",
            r"\\server\共享\?.lua;Z:\备用\?\init.lua",
            ".",
            "\\",
        );
        assert_eq!(
            candidates,
            [
                PathBuf::from(r"\\server\共享\目录\module.lua"),
                PathBuf::from(r"Z:\备用\目录\module\init.lua"),
            ]
        );

        let unchanged = searchpath_candidates("a.b", r"C:\?.lua", "", "\\");
        assert_eq!(unchanged, [PathBuf::from(r"C:\a.b.lua")]);
    }

    #[test]
    fn syntax_error_names_the_unicode_module_path() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("中文😀");
        std::fs::create_dir(&directory).unwrap();
        let module = directory.join("broken.lua");
        std::fs::write(&module, "local value =").unwrap();

        let lua = configured_lua(root.path());
        let package: Table = lua.globals().get("package").unwrap();
        package
            .set("path", format!("{}\\?.lua", lua_path(&directory)))
            .unwrap();
        let error = lua.load("require('broken')").exec().unwrap_err();
        let error = error.to_string();
        assert!(error.contains("中文😀"));
        assert!(error.contains("broken.lua"));
    }

    #[test]
    fn non_unicode_path_identity_preserves_each_utf16_code_unit() {
        let path = PathBuf::from(OsString::from_wide(&[
            0x0043, 0x003A, 0x005C, 0xD83D, 0xDE00, 0x005C, 0xD800, 0x005C, 0x0009,
        ]));

        let identity = safe_path_identity(&path);

        assert_eq!(
            identity,
            "@att-utf16-0043-003A-005C-D83D-DE00-005C-D800-005C-0009"
        );
        assert!(identity.is_ascii());
        assert!(!identity.chars().any(char::is_control));
    }
}
