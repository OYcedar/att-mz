//! ATT 多引擎文档、示例与总 Skill 的机器可验证契约。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use mlua::Lua;
use serde_json::Value as JsonValue;
use toml::Value as TomlValue;

const LOCALES: [&str; 10] = [
    "ar", "zh-Hans", "zh-Hant", "en", "fr", "ru", "es", "ja", "ko", "vi",
];
const PROMPT_ENGINES: [&str; 2] = ["rpg_maker", "generic"];

#[test]
fn all_current_markdown_links_resolve() {
    let root = workspace_root();
    for document in current_markdown_files() {
        let source = read_utf8(&document);
        for (line, target) in local_markdown_links(&source) {
            let (path_part, fragment) = target
                .split_once('#')
                .map_or((target.as_str(), None), |(path, anchor)| {
                    (path, Some(anchor))
                });
            let target_path = if path_part.is_empty() {
                document.clone()
            } else {
                document
                    .parent()
                    .expect("Markdown 文档必须有父目录")
                    .join(path_part)
            };

            assert!(
                !is_absolute_or_escaping(&target_path, root),
                "{}:{} 的本地链接不得离开仓库：{}",
                display_relative(&document),
                line,
                target
            );
            assert!(
                target_path.exists(),
                "{}:{} 链接的文件不存在：{}",
                display_relative(&document),
                line,
                display_relative(&target_path)
            );

            if let Some(fragment) = fragment.filter(|value| !value.is_empty()) {
                assert!(
                    target_path.is_file(),
                    "{}:{} 的锚点必须指向 Markdown 文件：{}",
                    display_relative(&document),
                    line,
                    target
                );
                let anchors = markdown_heading_anchors(&read_utf8(&target_path));
                assert!(
                    anchors.contains(fragment),
                    "{}:{} 的锚点不存在：{}；可用锚点为 {:?}",
                    display_relative(&document),
                    line,
                    target,
                    anchors
                );
            }
        }
    }
}

#[test]
fn documentation_has_one_navigation_and_three_independent_engine_domains() {
    let readme = read_utf8(&workspace_root().join("README.md"));
    let navigation = read_utf8(&workspace_root().join("docs/README.md"));

    for required in ["`mv`", "`mz`", "`generic`", "docs/README.md", "互不共享"] {
        assert!(readme.contains(required), "README.md 缺少 {required:?}");
    }

    for required in [
        "guides/translation-project.md",
        "rpg-maker/README.md",
        "generic/README.md",
        "translation/README.md",
        "lua/README.md",
        "runtime/README.md",
    ] {
        assert!(
            navigation.contains(required),
            "docs/README.md 缺少总导航目标 {required:?}"
        );
    }
}

#[test]
fn generic_jsonl_contract_and_example_use_the_minimal_shape() {
    let contract = read_utf8(&workspace_root().join("docs/generic/jsonl.md"));
    for required in [
        "Group 只允许以下字段",
        "`id`",
        "`kind`",
        "`units`",
        "Unit 只允许",
        "`text`",
        "唯一会被翻译的字段",
        "空白物理行",
        "无效 UTF-8",
        "空输入目录和空 JSONL 文件合法",
        "扩展名精确为小写 `.jsonl`",
        "纯空白字符串按原值是合法身份",
    ] {
        assert!(
            contract.contains(required),
            "Generic JSONL 规格缺少 {required:?}"
        );
    }
    for forbidden in [
        "\"translate\"",
        "\"context\"",
        "\"role\"",
        "\"metadata\"",
        "\"version\"",
    ] {
        assert!(
            !contract.contains(&format!("{forbidden}:")),
            "Generic JSONL 规格不得把 {forbidden} 定义为字段"
        );
    }

    let example_path = workspace_root().join("docs/generic/examples/sample.jsonl");
    let example = read_utf8(&example_path);
    assert!(!example.is_empty(), "Generic 示例应至少包含一个 Group");
    for (line_index, line) in example.lines().enumerate() {
        assert!(!line.trim().is_empty(), "JSONL 示例不得包含空白物理行");
        let value: JsonValue = serde_json::from_str(line).unwrap_or_else(|error| {
            panic!(
                "{}:{} 必须是一行有效 JSON：{error}",
                display_relative(&example_path),
                line_index + 1
            )
        });
        let group = value
            .as_object()
            .unwrap_or_else(|| panic!("JSONL 第 {} 行必须是 object", line_index + 1));
        assert_eq!(
            group.keys().map(String::as_str).collect::<BTreeSet<_>>(),
            BTreeSet::from(["id", "kind", "units"]),
            "JSONL Group 只能使用当前契约的三个字段"
        );
        let units = group["units"].as_array().expect("JSONL units 必须是数组");
        assert!(!units.is_empty(), "JSONL Group 至少包含一个 Unit");
        for unit in units {
            let unit = unit.as_object().expect("JSONL Unit 必须是 object");
            assert_eq!(
                unit.keys().map(String::as_str).collect::<BTreeSet<_>>(),
                BTreeSet::from(["id", "text"]),
                "JSONL Unit 只能使用当前契约的两个字段"
            );
            assert!(unit["id"].is_string());
            assert!(unit["text"].is_string());
        }
    }
}

#[test]
fn generic_dynamic_pipeline_and_independent_projects_are_documented() {
    let extraction = read_utf8(&workspace_root().join("docs/generic/extraction.md"));
    for required in [
        "不冻结或复制 Generic 输入",
        "一个数据库事务",
        "文件改名或移到其他 JSONL",
        "只改变一个 Unit ID",
        "只清除实际受影响的 Group",
        "明确要求重新 Extract",
    ] {
        assert!(
            extraction.contains(required),
            "Generic 动态 Extract 规格缺少 {required:?}"
        );
    }

    let translation = read_utf8(&workspace_root().join("docs/generic/translation.md"));
    for required in [
        "不包含文件、kind、Group 或 ID",
        "已经有多种不同 Current",
        "不跨越 JSONL 文件",
        "全部 Unit 按原顺序参与语境",
        "只有代表项带临时数字 ID",
        "每个 value 为字符串",
    ] {
        assert!(
            translation.contains(required),
            "Generic Translate 规格缺少 {required:?}"
        );
    }

    let write_back = read_utf8(&workspace_root().join("docs/generic/write-back.md"));
    for required in [
        "<projects.root>/generic/<name>/write_back/",
        "永远不修改外部输入目录",
        "Current Unit 用译文替换 `text`",
        "其他 Unit 保留当前原文",
        "确认除 `text` 外的全部事实",
        "成功输出保持",
    ] {
        assert!(
            write_back.contains(required),
            "Generic WriteBack 规格缺少 {required:?}"
        );
    }

    let guide = read_utf8(&workspace_root().join("docs/guides/translation-project.md"));
    for required in [
        "建立两个独立项目",
        "数据库、译文状态、日志、模型任务记录和输出都分别保存",
        "外部操作者负责把未覆盖内容转换成 Generic JSONL",
    ] {
        assert!(guide.contains(required), "混合项目指南缺少 {required:?}");
    }

    let configuration = read_utf8(&workspace_root().join("docs/runtime/configuration.md"));
    for required in [
        "[[translation.profiles]]",
        "MV、MZ 和 Generic 共用 Profile 定义",
        "每个项目分别保存最近采用的 ID",
    ] {
        assert!(
            configuration.contains(required),
            "公共 Profile 规格缺少 {required:?}"
        );
    }
}

#[test]
fn atomic_lua_is_documented_as_a_restricted_database_transaction() {
    let contract = read_utf8(&workspace_root().join("docs/lua/README.md"));
    for required in [
        "ctx.db.NULL",
        "ctx.db.blob(bytes)",
        "ctx.db.query(sql, parameters)",
        "ctx.db.execute(sql, parameters)",
        "ctx.translation.set(locator, translation)",
        "ctx.translation.clear(locator)",
        "`warn`",
        "BEGIN IMMEDIATE",
        "foreign_key_check",
        "quick_check",
        "outcome_unknown",
        "`lua.print`",
        "不自动把 SQL、参数、查询结果",
    ] {
        assert!(
            contract.contains(required),
            "原子数据库 Lua 规格缺少 {required:?}"
        );
    }
    for forbidden in [
        "`io`",
        "`os`",
        "`package`",
        "`require`",
        "`loadfile`",
        "`dofile`",
        "`debug`",
        "`warn`",
    ] {
        assert!(
            contract.contains(forbidden),
            "Lua VM 禁用能力清单缺少 {forbidden}"
        );
    }
    for obsolete in [
        "ctx.phase",
        "ctx.llm",
        "ctx.write_back",
        "ctx.output",
        "ctx.translations",
        "ctx.standard",
        "Managed",
    ] {
        assert!(
            !contract.contains(obsolete),
            "原子数据库 Lua 规格不得保留旧阶段能力 {obsolete:?}"
        );
    }

    let examples = workspace_root().join("docs/lua/examples");
    let expected = [
        (
            "generic-override.lua",
            &["ctx.translation.set", "group_id", "unit_id"][..],
        ),
        ("project-note.lua", &["ctx.db.execute", "CREATE TABLE"][..]),
        ("rollback.lua", &["error("][..]),
    ];
    for (file, markers) in expected {
        let path = examples.join(file);
        let source = read_utf8(&path);
        for marker in markers {
            assert!(
                source.contains(marker),
                "{} 缺少示例行为 {marker:?}",
                display_relative(&path)
            );
        }
        Lua::new()
            .load(&source)
            .set_name(file)
            .into_function()
            .unwrap_or_else(|error| panic!("{file} 必须是可编译的 Lua 5.4：{error}"));
    }
}

#[test]
fn shared_translation_examples_and_prompt_locales_keep_the_current_protocol() {
    for path in collect_files_with_extension(&workspace_root().join("docs"), "toml") {
        let source = read_utf8(&path);
        toml::from_str::<TomlValue>(&source)
            .unwrap_or_else(|error| panic!("{} 必须是有效 TOML：{error}", display_relative(&path)));
    }
    toml::from_str::<TomlValue>(&read_utf8(&workspace_root().join("config.example.toml")))
        .expect("config.example.toml 必须是有效 TOML");

    for engine in PROMPT_ENGINES {
        for locale in LOCALES {
            let locale_root = workspace_root().join("prompts").join(engine).join(locale);
            let system_path = locale_root.join("system.md");
            let thinking_path = locale_root.join("thinking.md");
            let system = read_utf8(&system_path);
            let thinking = read_utf8(&thinking_path);
            assert!(
                !system.trim().is_empty(),
                "{} 不得为空",
                system_path.display()
            );
            assert!(
                !thinking.trim().is_empty(),
                "{} 不得为空",
                thinking_path.display()
            );
            assert_eq!(
                prompt_template_variables(&system),
                BTreeSet::from(["source_language", "target_language"]),
                "{} 只能使用公共语言变量",
                display_relative(&system_path)
            );
            assert!(
                prompt_template_variables(&thinking).is_empty(),
                "{} 不得使用模板变量",
                display_relative(&thinking_path)
            );
        }
    }

    let generic = read_utf8(&workspace_root().join("prompts/generic/en/system.md"));
    let rpg_maker = read_utf8(&workspace_root().join("prompts/rpg_maker/en/system.md"));
    let protocol = read_utf8(&workspace_root().join("docs/translation/prompts.md"));
    let placeholders = read_utf8(&workspace_root().join("docs/translation/placeholders.md"));
    let rpg_rules = read_utf8(&workspace_root().join("docs/rpg-maker/rules.md"));
    assert!(
        generic.contains(r#"{"1":"Translation\nSecond line"}"#),
        "Generic Prompt 必须要求字符串 value"
    );
    assert!(
        rpg_maker.contains("Every value must be an array of strings"),
        "RPG Maker Prompt 必须要求字符串数组 value"
    );
    for required in [
        "按原始顺序保留全部 key",
        "重复、非法、未知和缺少的 ID",
        "每个 ID 独立验收",
        "其他合法 ID 可以保存",
    ] {
        assert!(
            protocol.contains(required),
            "公共响应协议缺少逐 ID Partial 语义 {required:?}"
        );
    }
    for required in [
        "捕获本身仍是可",
        "翻译的 NaturalText",
        "捕获前后的字节分别成为不透明 wrapper",
        "实际保护跨度重叠",
    ] {
        assert!(
            placeholders.contains(required),
            "公共 Placeholder 规格缺少 wrapper 保护语义 {required:?}"
        );
    }
    for required in [
        "建立在[公共 Placeholder 规格]",
        "本文只补充 MV/MZ 作用域",
        "控制符和形状规则",
    ] {
        assert!(
            rpg_rules.contains(required),
            "RPG Maker Placeholder 文档必须服从公共规格并只拥有引擎差异：{required:?}"
        );
    }
}

#[test]
fn total_skill_routes_execution_to_documents_without_copying_product_protocols() {
    let skill_root = workspace_root().join("skills/translate-with-att");
    let skill_path = skill_root.join("SKILL.md");
    let metadata_path = skill_root.join("agents/openai.yaml");
    let skill = read_utf8(&skill_path);
    let metadata = read_utf8(&metadata_path);

    assert!(skill.lines().count() < 500, "总 Skill 应保持简短");
    for required in [
        "只读",
        "执行",
        "协作者",
        "实际使用的 `att.exe`",
        "`docs/README.md`",
        "MV/MZ",
        "`generic`",
        "彼此独立",
        "外部操作者或工具",
        "同一文本",
        "Lua 文档精确修订",
        "Partial",
        "outcome unknown",
    ] {
        assert!(skill.contains(required), "总 Skill 缺少引导 {required:?}");
    }
    for forbidden in [
        "att mv init",
        "att mz init",
        "att generic init",
        r#"{"id":"#,
        "CREATE TABLE",
        "SELECT ",
    ] {
        assert!(
            !skill.contains(forbidden),
            "命令、JSONL 或数据库协议只能由文档负责，Skill 不得复制 {forbidden:?}"
        );
    }

    for required in [
        "display_name: \"ATT 翻译任务\"",
        "short_description:",
        "default_prompt:",
        "$translate-with-att",
    ] {
        assert!(
            metadata.contains(required),
            "agents/openai.yaml 缺少 {required:?}"
        );
    }
    assert!(
        !skill_root.join("README.md").exists(),
        "Skill 目录不应包含辅助 README"
    );
}

fn prompt_template_variables(source: &str) -> BTreeSet<&str> {
    let mut variables = BTreeSet::new();
    let mut remainder = source;
    while let Some(start) = remainder.find("{{") {
        remainder = &remainder[start + 2..];
        let Some(end) = remainder.find("}}") else {
            panic!("Prompt 存在未闭合模板变量");
        };
        variables.insert(&remainder[..end]);
        remainder = &remainder[end + 2..];
    }
    variables
}

fn local_markdown_links(source: &str) -> Vec<(usize, String)> {
    let mut links = Vec::new();
    let mut in_fence = false;
    for (line_index, line) in source.lines().enumerate() {
        if line.trim_start().starts_with("```") {
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
                .and_then(|value| value.strip_suffix('>'))
                .unwrap_or_else(|| {
                    raw_target
                        .split_ascii_whitespace()
                        .next()
                        .unwrap_or_default()
                });
            if !target.is_empty()
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

fn markdown_heading_anchors(source: &str) -> BTreeSet<String> {
    let mut anchors = BTreeSet::new();
    let mut occurrences = BTreeMap::<String, usize>::new();
    let mut in_fence = false;

    for line in source.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }

        let trimmed = line.trim_start();
        let level = trimmed.bytes().take_while(|byte| *byte == b'#').count();
        if !(1..=6).contains(&level)
            || !trimmed
                .as_bytes()
                .get(level)
                .is_some_and(u8::is_ascii_whitespace)
        {
            continue;
        }
        let heading = trimmed[level..].trim().trim_end_matches('#').trim_end();
        let base = markdown_heading_slug(heading);
        if base.is_empty() {
            continue;
        }
        let occurrence = occurrences.entry(base.clone()).or_default();
        let anchor = if *occurrence == 0 {
            base
        } else {
            format!("{base}-{occurrence}")
        };
        *occurrence += 1;
        anchors.insert(anchor);
    }
    anchors
}

fn markdown_heading_slug(heading: &str) -> String {
    let mut slug = String::new();
    for character in heading.chars() {
        if character.is_alphanumeric() || matches!(character, '-' | '_') {
            slug.extend(character.to_lowercase());
        } else if character.is_whitespace() {
            slug.push('-');
        }
    }
    slug
}

fn current_markdown_files() -> Vec<PathBuf> {
    let root = workspace_root();
    let mut files = vec![root.join("AGENTS.md"), root.join("README.md")];
    files.extend(collect_files_with_extension(&root.join("docs"), "md"));
    files.extend(collect_files_with_extension(&root.join("skills"), "md"));
    files.sort();
    files.dedup();
    files
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

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn display_relative(path: &Path) -> String {
    path.strip_prefix(workspace_root())
        .unwrap_or(path)
        .display()
        .to_string()
}
