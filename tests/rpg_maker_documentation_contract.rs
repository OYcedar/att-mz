//! RPG Maker 作者文档与可复制示例的机器可验证契约。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use mlua::Lua;
use toml::Value as TomlValue;

const CONTRACT_DOCUMENTS: [&str; 4] = ["rules.md", "terminology.md", "lua.md", "lua-cookbook.md"];

const RPG_MAKER_SKILL_NAME: &str = "translate-rpg-maker-with-att";

const RPG_MAKER_SKILL_DESCRIPTION: &str = "使用 ATT 调查、初始化、提取、翻译、审核、写回、续作和诊断 RPG Maker MV/MZ 汉化项目。用于创建、继续、修复、检查或验证 ATT 项目，编写 Extract Rules、术语、Placeholder、Prompt 或可信 Lua，以及把试玩反馈追溯到责任阶段；不用于开发 ATT Rust 源码或不经过 ATT 的普通翻译。";

const RPG_MAKER_SKILL_FLOWS: [(&str, &[&str]); 8] = [
    (
        "## 流程 0：入口与续作路由",
        &[
            "docs/README.md",
            "docs/runtime/README.md",
            "docs/runtime/cli.md",
        ],
    ),
    (
        "## 流程 1：调查与初始化",
        &[
            "docs/rpg-maker/README.md",
            "docs/rpg-maker/init.md",
            "docs/runtime/configuration.md",
        ],
    ),
    (
        "## 流程 2：提取与覆盖证明",
        &[
            "docs/rpg-maker/README.md",
            "docs/rpg-maker/extraction.md",
            "docs/rpg-maker/rules.md",
            "docs/runtime/sqlite.md",
        ],
    ),
    (
        "## 流程 3：翻译资源准备",
        &[
            "docs/rpg-maker/terminology.md",
            "docs/rpg-maker/prompts.md",
            "docs/runtime/configuration.md",
            "docs/rpg-maker/rules.md",
            "docs/rpg-maker/translation.md",
        ],
    ),
    (
        "## 流程 4：翻译与质量审核",
        &[
            "docs/rpg-maker/translation.md",
            "docs/rpg-maker/terminology.md",
            "docs/rpg-maker/prompts.md",
            "docs/rpg-maker/rules.md",
            "docs/runtime/chat-completions.md",
        ],
    ),
    (
        "## 流程 5：写回与隔离试玩",
        &[
            "docs/rpg-maker/write-back.md",
            "docs/runtime/directory-publishing.md",
        ],
    ),
    (
        "## 流程 6：诊断、恢复与安全续作",
        &[
            "docs/runtime/cli.md",
            "docs/runtime/project-log.md",
            "docs/runtime/sqlite.md",
            "docs/runtime/directory-publishing.md",
            "docs/runtime/chat-completions.md",
        ],
    ),
    (
        "## 条件子流程：可信 Lua",
        &["docs/rpg-maker/lua.md", "docs/rpg-maker/lua-cookbook.md"],
    ),
];

const RPG_MAKER_SKILL_DOCUMENTS: [&str; 18] = [
    "docs/README.md",
    "docs/runtime/README.md",
    "docs/runtime/cli.md",
    "docs/runtime/configuration.md",
    "docs/runtime/project-log.md",
    "docs/runtime/directory-publishing.md",
    "docs/runtime/sqlite.md",
    "docs/runtime/chat-completions.md",
    "docs/rpg-maker/README.md",
    "docs/rpg-maker/init.md",
    "docs/rpg-maker/extraction.md",
    "docs/rpg-maker/rules.md",
    "docs/rpg-maker/terminology.md",
    "docs/rpg-maker/prompts.md",
    "docs/rpg-maker/translation.md",
    "docs/rpg-maker/write-back.md",
    "docs/rpg-maker/lua.md",
    "docs/rpg-maker/lua-cookbook.md",
];

const PROMPT_LOCALES: [&str; 10] = [
    "ar", "zh-Hans", "zh-Hant", "en", "fr", "ru", "es", "ja", "ko", "vi",
];

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

const PRODUCTION_EXAMPLE_BINDINGS: [(&str, &str, &str, &str); 9] = [
    (
        "config.example.toml",
        "src/application/config.rs",
        "include_str!(\"../../config.example.toml\")",
        "fn repository_example_is_valid_for_every_command()",
    ),
    (
        "mv-dialogue.toml",
        "src/rpg_maker/dialogue.rs",
        "include_str!(\"../../docs/rpg-maker/examples/mv-dialogue.toml\")",
        "fn documented_mv_dialogue_definition_uses_the_production_parser_and_compiler()",
    ),
    (
        "extract-rules.toml",
        "src/rpg_maker/extract/rules/definition.rs",
        "include_str!(\"../../../../docs/rpg-maker/examples/extract-rules.toml\")",
        "fn documented_extract_rules_use_the_production_parser_and_compiler()",
    ),
    (
        "placeholders.toml",
        "src/rpg_maker/translate/planning_resource.rs",
        "include_bytes!(\"../../../docs/rpg-maker/examples/placeholders.toml\")",
        "fn documented_placeholder_rules_use_the_production_parser_and_compiler()",
    ),
    (
        "terminology.toml",
        "src/rpg_maker/translate/planning_resource.rs",
        "include_bytes!(\"../../../docs/rpg-maker/examples/terminology.toml\")",
        "fn documented_terminology_uses_the_production_parser_and_compiler()",
    ),
    (
        "lua-standard-data-file.lua",
        "src/rpg_maker/lua/lua54.rs",
        "include_str!(\"../../../docs/rpg-maker/examples/lua-standard-data-file.lua\")",
        "async fn documented_custom_data_file_example_executes_in_the_real_vm()",
    ),
    (
        "lua-translate-state.lua",
        "src/rpg_maker/lua/lua54.rs",
        "include_str!(\"../../../docs/rpg-maker/examples/lua-translate-state.lua\")",
        "async fn documented_translate_state_and_idempotent_write_back_examples_execute()",
    ),
    (
        "lua-idempotent-write-back.lua",
        "src/rpg_maker/lua/lua54.rs",
        "include_str!(\"../../../docs/rpg-maker/examples/lua-idempotent-write-back.lua\")",
        "async fn documented_translate_state_and_idempotent_write_back_examples_execute()",
    ),
    (
        "lua-complex-protocol.lua",
        "src/rpg_maker/lua/lua54.rs",
        "include_str!(\"../../../docs/rpg-maker/examples/lua-complex-protocol.lua\")",
        "async fn documented_complex_protocol_executes_all_three_phases_with_persisted_sqlite_state()",
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigurationExampleKind {
    ProductionMinimalInit,
    Fragment,
}

impl ConfigurationExampleKind {
    fn parse(line: &str) -> Option<Self> {
        match line.trim() {
            "<!-- att-config-example: production-minimal-init -->" => {
                Some(Self::ProductionMinimalInit)
            }
            "<!-- att-config-example: fragment -->" => Some(Self::Fragment),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct ConfigurationExample {
    kind: ConfigurationExampleKind,
    body: String,
    opening_line: usize,
}

#[test]
fn current_markdown_local_links_resolve_to_existing_files_and_anchors() {
    let markdown_files = current_markdown_files();
    assert!(
        !markdown_files.is_empty(),
        "当前文档范围至少应包含一个 Markdown 文件"
    );

    let mut failures = Vec::new();
    for markdown_path in markdown_files {
        let source = read_utf8(&markdown_path);
        for (line_number, target) in local_markdown_links(&source) {
            let (file_target, anchor) = target
                .split_once('#')
                .map_or((target.as_str(), None), |(file, anchor)| {
                    (file, Some(anchor))
                });
            if target.contains('%') {
                failures.push(format!(
                    "{}:{line_number}: 本地链接必须直接使用 UTF-8 路径，不能依赖百分号解码：{target}",
                    display_relative(&markdown_path)
                ));
                continue;
            }

            let linked_path = if file_target.is_empty() {
                markdown_path.clone()
            } else {
                markdown_path
                    .parent()
                    .expect("Markdown 文件始终有父目录")
                    .join(file_target)
            };
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
            } else if let Some(anchor) = anchor {
                if anchor.is_empty() {
                    failures.push(format!(
                        "{}:{line_number}: Markdown 锚点不能为空：{target}",
                        display_relative(&markdown_path)
                    ));
                } else if linked_path.extension().and_then(|value| value.to_str()) != Some("md") {
                    failures.push(format!(
                        "{}:{line_number}: 只有 Markdown 目标支持锚点校验：{target}",
                        display_relative(&markdown_path)
                    ));
                } else {
                    let linked_source = read_utf8(&linked_path);
                    let anchors = markdown_heading_anchors(&linked_source);
                    if !anchors.contains(anchor) {
                        failures.push(format!(
                            "{}:{line_number}: Markdown 锚点不存在：{target}",
                            display_relative(&markdown_path)
                        ));
                    }
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "当前文档包含无效本地链接：\n{}",
        failures.join("\n")
    );
}

#[test]
fn rpg_maker_skill_has_the_minimal_portable_package_contract() {
    let skill_root = rpg_maker_skill_root();
    assert_eq!(
        directory_shape(&skill_root),
        BTreeSet::from([
            ("SKILL.md".to_owned(), "file"),
            ("agents".to_owned(), "directory"),
        ]),
        "RPG Maker Skill 根目录只能包含 SKILL.md 和 agents 目录"
    );
    assert_eq!(
        directory_shape(&skill_root.join("agents")),
        BTreeSet::from([("openai.yaml".to_owned(), "file")]),
        "RPG Maker Skill 的 agents 目录只能包含 openai.yaml"
    );

    let skill_path = skill_root.join("SKILL.md");
    let skill_source = read_utf8(&skill_path).replace("\r\n", "\n");
    let (frontmatter, body) = parse_skill_frontmatter(&skill_path, &skill_source);
    assert_eq!(
        frontmatter
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["description", "name"]),
        "SKILL.md frontmatter 必须且只能包含 name 与 description"
    );
    assert_eq!(
        frontmatter.get("name").map(String::as_str),
        Some(RPG_MAKER_SKILL_NAME),
        "SKILL.md frontmatter name 必须等于 Skill 目录名"
    );
    assert_eq!(
        frontmatter.get("description").map(String::as_str),
        Some(RPG_MAKER_SKILL_DESCRIPTION),
        "SKILL.md frontmatter description 必须保持已确认的触发边界"
    );
    assert_eq!(
        skill_root.file_name().and_then(|name| name.to_str()),
        frontmatter.get("name").map(String::as_str),
        "Skill 目录名必须等于 SKILL.md frontmatter name"
    );
    assert!(
        !body.lines().any(is_markdown_fence),
        "SKILL.md 正文不得包含 fenced code block"
    );
    for (flow_heading, required_documents) in RPG_MAKER_SKILL_FLOWS {
        let flow = markdown_level_two_section(body, flow_heading).unwrap_or_else(|| {
            panic!("SKILL.md 缺少流程章节 {flow_heading}");
        });
        for required_heading in [
            "### 进入条件",
            "### 必读文档",
            "### 任务清单",
            "### 完成证据",
            "### 返回位置",
        ] {
            assert!(
                flow.contains(required_heading),
                "{flow_heading} 缺少统一章节 {required_heading}"
            );
        }
        let linked_documents = directly_linked_local_files(&skill_path, flow);
        for relative_path in required_documents {
            let expected_path = canonicalize_for_contract(&workspace_root().join(relative_path));
            assert!(
                linked_documents.contains(&expected_path),
                "{flow_heading} 必须直接链接权威文档 {relative_path}"
            );
        }
    }

    let interface_path = skill_root.join("agents/openai.yaml");
    let interface = parse_openai_interface(&interface_path, &read_utf8(&interface_path));
    assert_eq!(
        interface,
        BTreeMap::from([
            (
                "default_prompt".to_owned(),
                "\"使用 $translate-rpg-maker-with-att，根据当前游戏和项目状态推进这项 RPG Maker 汉化任务。\""
                    .to_owned(),
            ),
            (
                "display_name".to_owned(),
                "\"使用 ATT 汉化 RPG Maker\"".to_owned(),
            ),
            (
                "short_description".to_owned(),
                "\"根据项目状态执行、诊断并验证 RPG Maker 汉化流程\"".to_owned(),
            ),
        ]),
        "agents/openai.yaml 必须且只能包含已确认的三个 interface 字段"
    );
}

#[test]
fn rpg_maker_skill_is_directly_reachable_and_links_every_authoritative_document() {
    let skill_path = rpg_maker_skill_root().join("SKILL.md");
    let expected_skill_path = canonicalize_for_contract(&skill_path);
    for entry_path in [
        workspace_root().join("README.md"),
        workspace_root().join("docs/README.md"),
    ] {
        let linked_files = directly_linked_local_files(&entry_path, &read_utf8(&entry_path));
        assert!(
            linked_files.contains(&expected_skill_path),
            "{} 必须直接链接 RPG Maker Skill",
            display_relative(&entry_path)
        );
    }

    let linked_documents = directly_linked_local_files(&skill_path, &read_utf8(&skill_path));
    for relative_path in RPG_MAKER_SKILL_DOCUMENTS {
        let expected_path = canonicalize_for_contract(&workspace_root().join(relative_path));
        assert!(
            linked_documents.contains(&expected_path),
            "SKILL.md 必须直接链接权威文档 {relative_path}"
        );
    }
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

#[test]
fn repository_examples_remain_wired_into_production_contract_tests() {
    // 这里只固定“示例仍由真实边界消费”。字段语义继续由各生产解析器和真实 Lua VM
    // 测试负责，避免在文档测试里复制第二套解析规则。
    for (example, source_path, include_expression, contract_test) in PRODUCTION_EXAMPLE_BINDINGS {
        let source_path = workspace_root().join(source_path);
        let source = read_utf8(&source_path).replace("\r\n", "\n");
        assert!(
            source.contains(include_expression),
            "{example} 必须继续由 {} 直接 include",
            display_relative(&source_path)
        );
        assert!(
            source.contains(contract_test),
            "{example} 必须继续由 {} 的生产契约测试消费",
            display_relative(&source_path)
        );
    }
}

#[test]
fn runtime_toml_examples_are_explicitly_classified() {
    let runtime_root = workspace_root().join("docs/runtime");
    let markdown_files = collect_files_with_extension(&runtime_root, "md");
    let production_configuration_source =
        read_utf8(&workspace_root().join("src/application/config.rs")).replace("\r\n", "\n");
    let mut production_examples = 0_usize;
    let mut fragments = 0_usize;

    for markdown_path in markdown_files {
        let source = read_utf8(&markdown_path);
        for example in parse_configuration_examples(&markdown_path, &source) {
            toml::from_str::<TomlValue>(&example.body).unwrap_or_else(|error| {
                panic!(
                    "{}:{} 的配置示例必须至少是完整 TOML 语法：{error}",
                    display_relative(&markdown_path),
                    example.opening_line
                )
            });

            match example.kind {
                ConfigurationExampleKind::ProductionMinimalInit => {
                    production_examples += 1;
                    let production_fixture = format!("r#\"\n{}\"#", example.body);
                    assert!(
                        production_configuration_source.contains(&production_fixture),
                        "{}:{} 的 production-minimal-init 必须与生产配置测试的输入完全一致",
                        display_relative(&markdown_path),
                        example.opening_line
                    );
                    assert!(
                        production_configuration_source.contains(
                            "fn non_translate_commands_load_their_minimal_configuration()"
                        ),
                        "生产配置测试必须继续用当前 schema 验证最小 Init 配置"
                    );
                }
                ConfigurationExampleKind::Fragment => fragments += 1,
            }
        }
    }

    assert_ne!(
        production_examples, 0,
        "runtime 文档至少应保留一个由生产配置 schema 覆盖的完整示例"
    );
    assert_ne!(
        fragments, 0,
        "runtime 文档中的组合片段必须通过 fragment 标记明确跳过生产 schema 校验"
    );
}

#[test]
fn external_prompt_locales_preserve_the_same_machine_contract() {
    let prompt_root = workspace_root().join("prompts/rpg_maker");
    let expected_locales = PROMPT_LOCALES
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let actual_locales = fs::read_dir(&prompt_root)
        .expect("Prompt 资源根必须存在")
        .map(|entry| {
            let entry = entry.expect("Prompt locale 目录应可枚举");
            assert!(
                entry.file_type().expect("应可读取资源类型").is_dir(),
                "Prompt 资源根只能包含 locale 目录：{}",
                display_relative(&entry.path())
            );
            entry
                .file_name()
                .into_string()
                .expect("Prompt locale 目录名必须是 Unicode")
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(actual_locales, expected_locales);

    for locale in PROMPT_LOCALES {
        let locale_root = prompt_root.join(locale);
        let actual_files = fs::read_dir(&locale_root)
            .unwrap_or_else(|error| panic!("无法读取 {}：{error}", display_relative(&locale_root)))
            .map(|entry| {
                let entry = entry.expect("Prompt 组件应可枚举");
                assert!(
                    entry.file_type().expect("应可读取资源类型").is_file(),
                    "Prompt locale 目录只能包含普通文件：{}",
                    display_relative(&entry.path())
                );
                entry
                    .file_name()
                    .into_string()
                    .expect("Prompt 组件文件名必须是 Unicode")
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            actual_files,
            BTreeSet::from(["system.md".to_owned(), "thinking.md".to_owned()]),
            "{locale} 必须且只能包含两份现行 Prompt 组件"
        );

        let system = read_utf8(&locale_root.join("system.md"));
        let thinking = read_utf8(&locale_root.join("thinking.md"));
        assert!(!system.trim().is_empty(), "{locale}/system.md 不能为空");
        assert!(!thinking.trim().is_empty(), "{locale}/thinking.md 不能为空");
        assert_eq!(
            prompt_template_variables(&system),
            BTreeSet::from(["source_language", "target_language"]),
            "{locale}/system.md 只能使用两项现行模板变量"
        );
        assert!(
            !thinking.contains("{{") && !thinking.contains("}}"),
            "{locale}/thinking.md 不允许模板变量"
        );

        for shape_literal in [
            "single line",
            "free line breaking",
            "N lines, corresponding line by line",
            "N items, corresponding item by item",
        ] {
            assert!(
                system.contains(shape_literal),
                "{locale}/system.md 缺少形状协议字面量 {shape_literal:?}"
            );
            assert!(
                thinking.contains(shape_literal),
                "{locale}/thinking.md 缺少形状协议字面量 {shape_literal:?}"
            );
        }
        let combined = format!("{system}\n{thinking}");
        for literal in ["JSON", "[ID]", "<why>", "</why>", "ATT token"] {
            assert!(
                combined.contains(literal),
                "{locale} Prompt 资源缺少协议字面量 {literal:?}"
            );
        }
    }
}

fn prompt_template_variables(source: &str) -> BTreeSet<&str> {
    let mut variables = BTreeSet::new();
    let mut remaining = source;
    loop {
        let next_open = remaining.find("{{");
        let next_close = remaining.find("}}");
        let Some(open) = next_open else {
            assert!(next_close.is_none(), "Prompt 模板含有未配对的结束定界符");
            break;
        };
        assert!(
            next_close.is_none_or(|close| open < close),
            "Prompt 模板含有未配对的结束定界符"
        );
        let after_open = &remaining[open + 2..];
        let close = after_open.find("}}").expect("Prompt 模板变量必须闭合");
        assert!(
            !after_open[..close].contains("{{"),
            "Prompt 模板变量不得嵌套"
        );
        variables.insert(&after_open[..close]);
        remaining = &after_open[close + 2..];
    }
    variables
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

fn parse_configuration_examples(path: &Path, source: &str) -> Vec<ConfigurationExample> {
    let lines = source.lines().collect::<Vec<_>>();
    let mut examples = Vec::new();
    let mut index = 0_usize;
    while index < lines.len() {
        let line = lines[index];
        if ConfigurationExampleKind::parse(line).is_some() {
            assert!(
                lines
                    .get(index + 1)
                    .is_some_and(|line| line.starts_with("```toml")),
                "{}:{} 的 att-config-example 标记必须紧邻 TOML fenced code block",
                display_relative(path),
                index + 1
            );
        }
        if !line.starts_with("```") || line == "```" {
            index += 1;
            continue;
        }

        let opening_line = index + 1;
        let language = line.trim_start_matches('`').trim();
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
        if language == "toml" {
            let kind = body_start
                .checked_sub(2)
                .and_then(|marker| ConfigurationExampleKind::parse(lines[marker]))
                .unwrap_or_else(|| {
                    panic!(
                        "{}:{opening_line} 的 runtime TOML 缺少紧邻的 att-config-example 分类",
                        display_relative(path)
                    )
                });
            let mut body = lines[body_start..index].join("\n");
            body.push('\n');
            examples.push(ConfigurationExample {
                kind,
                body,
                opening_line,
            });
        }
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
        if line.starts_with("```") {
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
    let mut files = vec![
        root.join("AGENTS.md"),
        root.join("README.md"),
        root.join("docs/README.md"),
    ];
    files.extend(collect_files_with_extension(
        &root.join("docs/runtime"),
        "md",
    ));
    files.extend(collect_files_with_extension(
        &root.join("docs/rpg-maker"),
        "md",
    ));
    files.extend(collect_files_with_extension(&root.join("skills"), "md"));
    files.sort();
    files.dedup();
    files
}

fn parse_skill_frontmatter<'a>(
    path: &Path,
    source: &'a str,
) -> (BTreeMap<String, String>, &'a str) {
    let remainder = source
        .strip_prefix("---\n")
        .unwrap_or_else(|| panic!("{} 必须以 YAML frontmatter 开始", display_relative(path)));
    let (frontmatter_source, body) = remainder.split_once("\n---\n").unwrap_or_else(|| {
        panic!(
            "{} 的 YAML frontmatter 必须以独立的 --- 闭合",
            display_relative(path)
        )
    });
    let mut frontmatter = BTreeMap::new();
    for (line_index, line) in frontmatter_source.lines().enumerate() {
        assert!(
            !line.is_empty() && !line.starts_with(char::is_whitespace),
            "{}:{} 的 frontmatter 只能包含顶层字段",
            display_relative(path),
            line_index + 2
        );
        let (key, value) = line.split_once(':').unwrap_or_else(|| {
            panic!(
                "{}:{} 的 frontmatter 字段缺少冒号",
                display_relative(path),
                line_index + 2
            )
        });
        assert!(
            !key.is_empty() && !value.trim().is_empty(),
            "{}:{} 的 frontmatter 字段名和值均不能为空",
            display_relative(path),
            line_index + 2
        );
        assert!(
            frontmatter
                .insert(key.to_owned(), value.trim().to_owned())
                .is_none(),
            "{}:{} 的 frontmatter 字段 {key:?} 重复",
            display_relative(path),
            line_index + 2
        );
    }
    (frontmatter, body)
}

fn parse_openai_interface(path: &Path, source: &str) -> BTreeMap<String, String> {
    let normalized = source.replace("\r\n", "\n");
    let mut lines = normalized
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.is_empty());
    let (root_line_index, root_line) = lines.next().unwrap_or_else(|| {
        panic!("{} 不能为空", display_relative(path));
    });
    assert_eq!(
        root_line,
        "interface:",
        "{}:{} 必须以唯一顶层字段 interface 开始",
        display_relative(path),
        root_line_index + 1
    );

    let mut interface = BTreeMap::new();
    for (line_index, line) in lines {
        let field = line.strip_prefix("  ").unwrap_or_else(|| {
            panic!(
                "{}:{} 只能包含 interface 下的两空格缩进字段",
                display_relative(path),
                line_index + 1
            )
        });
        assert!(
            !field.starts_with(char::is_whitespace),
            "{}:{} 不得在 interface 字段下继续嵌套",
            display_relative(path),
            line_index + 1
        );
        let (key, value) = field.split_once(':').unwrap_or_else(|| {
            panic!(
                "{}:{} 的 interface 字段缺少冒号",
                display_relative(path),
                line_index + 1
            )
        });
        assert!(
            !key.is_empty() && !value.trim().is_empty(),
            "{}:{} 的 interface 字段名和值均不能为空",
            display_relative(path),
            line_index + 1
        );
        assert!(
            interface
                .insert(key.to_owned(), value.trim().to_owned())
                .is_none(),
            "{}:{} 的 interface 字段 {key:?} 重复",
            display_relative(path),
            line_index + 1
        );
    }
    interface
}

fn is_markdown_fence(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

fn markdown_level_two_section<'a>(source: &'a str, heading: &str) -> Option<&'a str> {
    let start = source.find(heading)?;
    let remainder = &source[start + heading.len()..];
    let end = remainder.find("\n## ").unwrap_or(remainder.len());
    Some(&remainder[..end])
}

fn directly_linked_local_files(path: &Path, source: &str) -> BTreeSet<PathBuf> {
    local_markdown_links(source)
        .into_iter()
        .filter_map(|(_, target)| {
            let file_target = target
                .split_once('#')
                .map_or(target.as_str(), |(file, _)| file);
            if file_target.is_empty() {
                return None;
            }
            let linked_path = path
                .parent()
                .expect("Markdown 文件始终有父目录")
                .join(file_target);
            Some(canonicalize_for_contract(&linked_path))
        })
        .collect()
}

fn canonicalize_for_contract(path: &Path) -> PathBuf {
    fs::canonicalize(path)
        .unwrap_or_else(|error| panic!("无法解析 {}：{error}", display_relative(path)))
}

fn directory_shape(path: &Path) -> BTreeSet<(String, &'static str)> {
    fs::read_dir(path)
        .unwrap_or_else(|error| panic!("无法读取 {}：{error}", display_relative(path)))
        .map(|entry| {
            let entry = entry.expect("Skill 目录项应可枚举");
            let file_type = entry.file_type().unwrap_or_else(|error| {
                panic!(
                    "无法读取 {} 的文件类型：{error}",
                    display_relative(&entry.path())
                )
            });
            let kind = if file_type.is_file() {
                "file"
            } else if file_type.is_dir() {
                "directory"
            } else {
                "other"
            };
            (
                entry
                    .file_name()
                    .into_string()
                    .expect("Skill 目录项名称必须是 Unicode"),
                kind,
            )
        })
        .collect()
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

fn rpg_maker_skill_root() -> PathBuf {
    workspace_root().join("skills").join(RPG_MAKER_SKILL_NAME)
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
