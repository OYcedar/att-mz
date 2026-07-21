//! RPG Maker 作者文档与可复制示例的机器可验证契约。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use mlua::Lua;
use toml::Value as TomlValue;

const CONTRACT_DOCUMENTS: [&str; 4] = ["rules.md", "terminology.md", "lua.md", "lua-cookbook.md"];

const TOML_EXAMPLES: [&str; 4] = [
    "mv-dialogue.toml",
    "extract-rules.toml",
    "placeholders.toml",
    "terminology.toml",
];

const LUA_EXAMPLES: [(&str, &[&str]); 4] = [
    (
        "lua-standard-data-file.lua",
        &["ctx.rpg_maker.data_file", "ctx.extract.replace_standard"],
    ),
    (
        "lua-translate-state.lua",
        &[
            "ctx.translation.prepare",
            ":is_current(",
            ":accept(",
            "ctx.db.begin(",
            "ctx.db.commit(",
        ],
    ),
    (
        "lua-idempotent-write-back.lua",
        &["ctx.write_back", "ctx.output"],
    ),
    (
        "lua-complex-protocol.lua",
        &[
            "ctx.phase",
            "ctx.translation.prepare",
            "ctx.write_back",
            "ctx.db",
        ],
    ),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExampleKind {
    Valid,
    Invalid,
    Illustrative,
}

impl ExampleKind {
    fn parse(line: &str) -> Option<Self> {
        match line.trim() {
            "<!-- att-example: valid -->" => Some(Self::Valid),
            "<!-- att-example: invalid -->" => Some(Self::Invalid),
            "<!-- att-example: illustrative -->" => Some(Self::Illustrative),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct FencedExample {
    kind: ExampleKind,
    language: String,
    body: String,
    opening_line: usize,
}

#[test]
fn rpg_maker_markdown_local_links_resolve_to_existing_files() {
    let documentation_root = documentation_root();
    let markdown_files = collect_files_with_extension(&documentation_root, "md");
    assert!(
        !markdown_files.is_empty(),
        "RPG Maker 文档目录至少应包含一个 Markdown 文件"
    );

    let mut failures = Vec::new();
    for markdown_path in markdown_files {
        let source = read_utf8(&markdown_path);
        for (line_number, target) in local_markdown_links(&source) {
            let file_target = target.split('#').next().unwrap_or_default();
            if file_target.is_empty() {
                continue;
            }
            if file_target.contains('%') {
                failures.push(format!(
                    "{}:{line_number}: 本地链接必须直接使用 UTF-8 路径，不能依赖百分号解码：{target}",
                    display_relative(&markdown_path)
                ));
                continue;
            }

            let linked_path = markdown_path
                .parent()
                .expect("Markdown 文件始终有父目录")
                .join(file_target);
            if is_absolute_or_escaping(&linked_path, workspace_root()) {
                failures.push(format!(
                    "{}:{line_number}: 本地链接不得逃出项目工作区：{target}",
                    display_relative(&markdown_path)
                ));
            } else if !linked_path.exists() {
                failures.push(format!(
                    "{}:{line_number}: 本地链接目标不存在：{target}",
                    display_relative(&markdown_path)
                ));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "RPG Maker 文档包含无效本地链接：\n{}",
        failures.join("\n")
    );
}

#[test]
fn normative_fences_are_classified_and_valid_inputs_compile() {
    let documentation_root = documentation_root();
    let mut totals = BTreeMap::from([
        ("valid", 0_usize),
        ("invalid", 0_usize),
        ("illustrative", 0_usize),
    ]);

    for file_name in CONTRACT_DOCUMENTS {
        let path = documentation_root.join(file_name);
        let source = read_utf8(&path);
        let examples = parse_classified_fences(&path, &source);
        assert!(
            !examples.is_empty(),
            "{} 至少应有一个机器分类的规范代码块",
            display_relative(&path)
        );

        for example in examples {
            let total_key = match example.kind {
                ExampleKind::Valid => "valid",
                ExampleKind::Invalid => "invalid",
                ExampleKind::Illustrative => "illustrative",
            };
            *totals.get_mut(total_key).expect("计数键已预先建立") += 1;

            if example.kind != ExampleKind::Valid {
                continue;
            }
            match example.language.as_str() {
                "toml" => validate_toml_example(&path, &example),
                "lua" => compile_lua(
                    &example.body,
                    &format!("{}:{}", display_relative(&path), example.opening_line),
                ),
                _ => {}
            }
        }
    }

    for (kind, total) in totals {
        assert_ne!(total, 0, "规范文档至少应包含一个 {kind} 代码块");
    }
}

#[test]
fn cookbook_lua_files_are_utf8_compilable_and_cover_the_supported_workflows() {
    let examples_root = documentation_root().join("examples");
    let readme = examples_root.join("README.md");
    let readme_source = read_utf8(&readme);

    for (file_name, required_fragments) in LUA_EXAMPLES {
        assert!(
            readme_source.contains(file_name),
            "examples/README.md 必须链接或列出 {file_name}"
        );
        let path = examples_root.join(file_name);
        let source = read_utf8(&path);
        assert!(
            !source.trim().is_empty(),
            "{} 不能是空脚本",
            display_relative(&path)
        );
        compile_lua(&source, &display_relative(&path));

        for &fragment in required_fragments {
            assert!(
                source.contains(fragment),
                "{} 必须演示当前 API 形状 {fragment:?}",
                display_relative(&path)
            );
        }
    }
}

#[test]
fn complete_toml_examples_are_utf8_parseable_and_listed_in_the_manifest() {
    let examples_root = documentation_root().join("examples");
    let readme_source = read_utf8(&examples_root.join("README.md"));

    for file_name in TOML_EXAMPLES {
        assert!(
            readme_source.contains(file_name),
            "examples/README.md 必须链接或列出 {file_name}"
        );
        let path = examples_root.join(file_name);
        let source = read_utf8(&path);
        toml::from_str::<TomlValue>(&source).unwrap_or_else(|error| {
            panic!(
                "{} 必须能由项目当前 TOML 语法解析器读取：{error}",
                display_relative(&path)
            )
        });
    }
}

fn validate_toml_example(path: &Path, example: &FencedExample) {
    let parsed = toml::from_str::<TomlValue>(&example.body).unwrap_or_else(|error| {
        panic!(
            "{}:{} 标为 valid 的 TOML 必须能由项目当前 TOML 解析器读取：{error}",
            display_relative(path),
            example.opening_line
        )
    });
    let table = parsed.as_table().unwrap_or_else(|| {
        panic!(
            "{}:{} 标为 valid 的 TOML 根必须是 table",
            display_relative(path),
            example.opening_line
        )
    });

    let required_root = match path.file_name().and_then(|name| name.to_str()) {
        Some("rules.md") => Some("rule"),
        Some("terminology.md") => Some("term"),
        _ => None,
    };
    if let Some(required_root) = required_root {
        assert!(
            table.get(required_root).is_some_and(TomlValue::is_array),
            "{}:{} 的 valid TOML 必须包含数组根 {required_root:?}",
            display_relative(path),
            example.opening_line
        );
        assert_eq!(
            table.len(),
            1,
            "{}:{} 的完整规则文件不得在根部混入其他字段",
            display_relative(path),
            example.opening_line
        );
    }
}

fn compile_lua(source: &str, name: &str) {
    Lua::new()
        .load(source)
        .set_name(name)
        .into_function()
        .unwrap_or_else(|error| panic!("{name} 必须是可编译的 Lua 5.4 chunk：{error}"));
}

fn parse_classified_fences(path: &Path, source: &str) -> Vec<FencedExample> {
    let lines = source.lines().collect::<Vec<_>>();
    let mut examples = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        if ExampleKind::parse(line).is_some() {
            assert!(
                lines
                    .get(index + 1)
                    .is_some_and(|line| line.starts_with("```")),
                "{}:{} 的 att-example 标记必须紧邻它所分类的 fenced code block",
                display_relative(path),
                index + 1
            );
        }
        if !line.starts_with("```") || line == "```" {
            index += 1;
            continue;
        }

        let opening_line = index + 1;
        let kind = index
            .checked_sub(1)
            .and_then(|previous| ExampleKind::parse(lines[previous]))
            .unwrap_or_else(|| {
                panic!(
                    "{}:{opening_line} 的规范代码块缺少紧邻的 att-example 分类",
                    display_relative(path)
                )
            });
        let language = line.trim_start_matches('`').trim().to_owned();
        assert!(
            !language.is_empty(),
            "{}:{opening_line} 的代码块必须声明语言",
            display_relative(path)
        );

        index += 1;
        let body_start = index;
        while index < lines.len() && lines[index] != "```" {
            index += 1;
        }
        assert!(
            index < lines.len(),
            "{}:{opening_line} 的代码块没有闭合",
            display_relative(path)
        );
        let mut body = lines[body_start..index].join("\n");
        body.push('\n');
        examples.push(FencedExample {
            kind,
            language,
            body,
            opening_line,
        });
        index += 1;
    }
    examples
}

fn local_markdown_links(source: &str) -> Vec<(usize, String)> {
    let mut links = Vec::new();
    let mut in_fence = false;
    for (line_index, line) in source.lines().enumerate() {
        if line.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }

        let mut rest = line;
        while let Some(link_start) = rest.find("](") {
            rest = &rest[link_start + 2..];
            let Some(link_end) = rest.find(')') else {
                break;
            };
            let raw_target = rest[..link_end].trim();
            let target = raw_target
                .strip_prefix('<')
                .and_then(|target| target.strip_suffix('>'))
                .unwrap_or_else(|| {
                    raw_target
                        .split_ascii_whitespace()
                        .next()
                        .unwrap_or_default()
                });
            if !target.is_empty()
                && !target.starts_with('#')
                && !target.starts_with("http://")
                && !target.starts_with("https://")
                && !target.starts_with("mailto:")
            {
                links.push((line_index + 1, target.to_owned()));
            }
            rest = &rest[link_end + 1..];
        }
    }
    links
}

fn collect_files_with_extension(root: &Path, extension: &str) -> Vec<PathBuf> {
    fn visit(directory: &Path, extension: &str, output: &mut Vec<PathBuf>) {
        let mut entries = fs::read_dir(directory)
            .unwrap_or_else(|error| panic!("无法读取 {}：{error}", display_relative(directory)))
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_else(|error| panic!("无法枚举 {}：{error}", display_relative(directory)));
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let file_type = entry.file_type().unwrap_or_else(|error| {
                panic!("无法读取 {} 的文件类型：{error}", display_relative(&path))
            });
            if file_type.is_dir() {
                visit(&path, extension, output);
            } else if file_type.is_file()
                && path.extension().and_then(|value| value.to_str()) == Some(extension)
            {
                output.push(path);
            }
        }
    }

    let mut files = Vec::new();
    visit(root, extension, &mut files);
    files
}

fn is_absolute_or_escaping(path: &Path, root: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return true;
    };
    let mut depth = 0_usize;
    for component in relative.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => return true,
            Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    return true;
                }
                depth -= 1;
            }
            Component::Normal(_) => depth += 1,
        }
    }
    false
}

fn read_utf8(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("{} 必须存在且是 UTF-8：{error}", display_relative(path)))
}

fn documentation_root() -> PathBuf {
    workspace_root().join("docs/rpg-maker")
}

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn display_relative(path: &Path) -> String {
    path.strip_prefix(workspace_root())
        .unwrap_or(path)
        .display()
        .to_string()
}
