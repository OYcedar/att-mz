#![cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]

//! Windows x64 生产进程边界的多引擎 CLI 与 RPG Maker 主流程黑盒测试。

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde_json::{Value, json};

const PROJECT: &str = "shared";
const SOURCE_TEXT: &str = "薬草です";
const TRANSLATION: &str = "治疗药草";
const MV_SPEAKER: &str = "アリス";
const MV_BODY: &str = "こんにちは、世界！";
const MV_SPEAKER_TRANSLATION: &str = "爱丽丝";
const MV_BODY_TRANSLATION: &str = "你好，世界！";
const RULES_SHORT_SOURCE: &str = "ポーション";
const RULES_SHORT_TRANSLATION: &str = "治疗药水";
const RULES_LONG_SOURCE: &str = "高級ポーション";

#[test]
fn help_exposes_mv_mz_and_generic_as_independent_command_domains() {
    let temporary = tempfile::tempdir().expect("应可建立 CLI 帮助测试目录");

    let root_help = run_information_command(temporary.path(), &["--help"]);
    for engine in ["mv", "mz", "generic"] {
        assert!(
            root_help.contains(engine),
            "根帮助必须列出 {engine} 命令域：\n{root_help}"
        );
    }

    for engine in ["mv", "mz", "generic"] {
        let help = run_information_command(temporary.path(), &[engine, "--help"]);
        for command in ["init", "extract", "translate", "write-back", "lua"] {
            assert!(
                help.contains(command),
                "{engine} 帮助必须列出 {command}：\n{help}"
            );
        }
    }
}

#[test]
fn configuration_is_required_and_stage_commands_reject_lua_options() {
    let temporary = tempfile::tempdir().expect("应可建立 CLI 参数测试目录");
    let root = temporary.path();

    let missing_configuration = Command::new(env!("CARGO_BIN_EXE_att"))
        .current_dir(root)
        .args(["mz", "extract", "--name", PROJECT, "--builtin"])
        .output()
        .expect("att.exe 应可执行");
    assert_eq!(missing_configuration.status.code(), Some(2));
    assert!(missing_configuration.stdout.is_empty());
    assert!(String::from_utf8_lossy(&missing_configuration.stderr).contains("--config"));

    for engine in ["mv", "mz", "generic"] {
        for stage in ["extract", "translate", "write-back"] {
            let mut arguments = vec![engine, stage, "--name", PROJECT, "--lua", "stage.lua"];
            if stage == "translate" {
                arguments.insert(4, "local");
            }
            let output = Command::new(env!("CARGO_BIN_EXE_att"))
                .current_dir(root)
                .args(arguments)
                .output()
                .expect("att.exe 应可执行");
            assert!(
                !output.status.success(),
                "{engine} {stage} 不得接受阶段 --lua"
            );
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("--lua"),
                "参数诊断应指出 --lua"
            );
        }

        let output = Command::new(env!("CARGO_BIN_EXE_att"))
            .current_dir(root)
            .args([
                engine,
                "lua",
                "--name",
                PROJECT,
                "script.lua",
                "--profile",
                "local",
            ])
            .output()
            .expect("att.exe 应可执行");
        assert!(
            !output.status.success(),
            "{engine} 独立 Lua 不得接受 --profile"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("--profile"),
            "参数诊断应指出 --profile"
        );
    }
}

#[test]
fn same_named_mv_mz_and_generic_projects_remain_isolated_across_real_processes() {
    let temporary = tempfile::tempdir().expect("应可建立 RPG Maker 端到端测试目录");
    let root = temporary.path();
    let mz_game = root.join("mz-game");
    let mv_game = root.join("mv-game");
    let generic_input = root.join("jsonl");
    write_minimal_mz_game(&mz_game);
    write_minimal_mv_game(&mv_game);
    fs::create_dir(&generic_input).expect("Generic 输入目录应可建立");
    write_generic_group(&generic_input.join("story.jsonl"), "同名项目隔离", false);
    write_rpg_maker_prompt(root);

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("本地模型服务端口应可绑定");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("本地模型地址应可读取")
    );
    write_configuration(root, &endpoint);

    assert_success("MZ Init", &run_att(root, init_arguments("mz", &mz_game)));
    assert_success("MV Init", &run_att(root, init_arguments("mv", &mv_game)));
    let mut generic_init = arguments(&["generic", "init", "--name", PROJECT, "--path"]);
    generic_init.push(generic_input.as_os_str().to_owned());
    generic_init.extend(arguments(&[
        "--source-language",
        "ja",
        "--target-language",
        "zh-Hans",
    ]));
    assert_success("Generic Init", &run_att(root, generic_init));

    let mz_workspace = root.join("projects/mz").join(PROJECT);
    let mv_workspace = root.join("projects/mv").join(PROJECT);
    let generic_workspace = root.join("projects/generic").join(PROJECT);
    assert!(mz_workspace.join("project.db").is_file());
    assert!(mv_workspace.join("project.db").is_file());
    assert!(generic_workspace.join("project.db").is_file());
    assert!(mz_workspace.join("source/data/Items.json").is_file());
    assert!(mv_workspace.join("source/www/data/Map001.json").is_file());
    assert_ne!(
        fs::canonicalize(&mz_workspace).expect("MZ 工作区应存在"),
        fs::canonicalize(&mv_workspace).expect("MV 工作区应存在"),
        "同名 MV 与 MZ 项目必须使用不同工作区"
    );
    assert_ne!(
        fs::canonicalize(&mz_workspace).expect("MZ 工作区应存在"),
        fs::canonicalize(&generic_workspace).expect("Generic 工作区应存在"),
        "同名 MZ 与 Generic 项目必须使用不同工作区"
    );

    assert_success(
        "MZ Extract",
        &run_att(
            root,
            arguments(&["mz", "extract", "--name", PROJECT, "--builtin"]),
        ),
    );
    assert_success(
        "MV Extract",
        &run_att(
            root,
            arguments(&["mv", "extract", "--name", PROJECT, "--builtin"]),
        ),
    );

    let server = thread::spawn(move || serve_one_translation(listener));
    assert_success(
        "MZ Translate",
        &run_att(
            root,
            arguments(&["mz", "translate", "--name", PROJECT, "local"]),
        ),
    );
    let request = server
        .join()
        .expect("本地模型服务线程不得 panic")
        .expect("本地模型服务必须完成一次请求");
    assert_eq!(request["model"], "e2e-model");
    let messages = request["messages"]
        .as_array()
        .expect("请求必须包含 messages 数组");
    assert_eq!(messages.len(), 2, "一次翻译请求只应包含 system 与 user");
    assert!(
        messages[1]["content"]
            .as_str()
            .is_some_and(|content| content.contains(SOURCE_TEXT)),
        "模型 user message 必须包含待译原文"
    );

    assert_success(
        "MZ WriteBack",
        &run_att(root, arguments(&["mz", "write-back", "--name", PROJECT])),
    );
    assert_success(
        "MV WriteBack",
        &run_att(root, arguments(&["mv", "write-back", "--name", PROJECT])),
    );

    let mz_output: Value = serde_json::from_slice(
        &fs::read(mz_workspace.join("write_back/data/Items.json"))
            .expect("MZ WriteBack 必须生成 Items.json"),
    )
    .expect("MZ WriteBack Items.json 必须可重新解析");
    assert_eq!(mz_output[1]["description"], TRANSLATION);

    let mv_output = mv_workspace.join("write_back/www/data/Map001.json");
    assert!(mv_output.is_file(), "MV WriteBack 必须保留 www 布局");
    assert!(
        !mv_workspace.join("write_back/data").exists(),
        "MV WriteBack 不得把 www 内容提升到输出根"
    );

    let original_mz: Value = serde_json::from_slice(
        &fs::read(mz_game.join("data/Items.json")).expect("外部 MZ 输入必须保留"),
    )
    .expect("外部 MZ 输入必须保持有效 JSON");
    assert_eq!(
        original_mz[1]["description"], SOURCE_TEXT,
        "WriteBack 不得修改外部游戏目录"
    );
}

#[test]
fn mv_dialogue_crosses_extract_translate_and_write_back_processes() {
    let temporary = tempfile::tempdir().expect("应可建立 MV 对话端到端测试目录");
    let root = temporary.path();
    let game = root.join("mv-game");
    write_minimal_mv_game(&game);
    write_mv_dialogue_rules(root);
    write_rpg_maker_prompt(root);

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("本地模型服务端口应可绑定");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("本地模型地址应可读取")
    );
    write_configuration(root, &endpoint);

    assert_success("MV 对话 Init", &run_att(root, init_arguments("mv", &game)));
    assert_success(
        "MV 对话 Extract",
        &run_att(
            root,
            arguments(&[
                "mv",
                "extract",
                "--name",
                PROJECT,
                "--builtin",
                "--dialogue-rules",
                "dialogue.toml",
            ]),
        ),
    );

    let workspace = root.join("projects/mv").join(PROJECT);
    let database = workspace.join("project.db");
    let extracted = read_owner_units(&database, "builtin");
    assert_eq!(
        extracted,
        vec![(json!(MV_SPEAKER), None), (json!([MV_BODY]), None),],
        "MV 姓名和正文必须按 Speaker、Body 顺序物化"
    );
    let connection = Connection::open(&database).expect("MV 项目数据库应可打开");
    let (groups, claims): (i64, i64) = connection
        .query_row(
            "SELECT
                (SELECT count(*) FROM rpg_maker_text_group WHERE owner = 'builtin'),
                (SELECT count(*) FROM rpg_maker_mutation_claim WHERE owner = 'builtin')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("MV 对话组和修改目标应可读取");
    assert_eq!(groups, 1, "MV 对话必须物化为一个原子语义组");
    assert!(claims > 0, "MV 对话写回目标必须由 Mutation Claim 保护");
    let definition: String = connection
        .query_row(
            "SELECT canonical_json
             FROM rpg_maker_project_definition
             WHERE definition_kind = 'mv_dialogue_rules'",
            [],
            |row| row.get(0),
        )
        .expect("MV 对话投影定义应随 Builtin 快照保存");
    let definition: Value =
        serde_json::from_str(&definition).expect("MV 对话投影定义必须是规范 JSON");
    assert_eq!(definition["rules"].as_array().map(Vec::len), Some(1));
    drop(connection);

    let server = thread::spawn(move || {
        serve_one_response(
            listener,
            json!({
                "1": [MV_SPEAKER_TRANSLATION],
                "2": [MV_BODY_TRANSLATION]
            }),
        )
    });
    assert_success(
        "MV 对话 Translate",
        &run_att(
            root,
            arguments(&["mv", "translate", "--name", PROJECT, "local"]),
        ),
    );
    let request = server
        .join()
        .expect("MV 对话模型服务线程不得 panic")
        .expect("MV 对话模型服务必须完成请求");
    let user = request["messages"][1]["content"]
        .as_str()
        .expect("MV 对话 user message 必须是字符串");
    assert!(
        user.contains(MV_SPEAKER) && user.contains(MV_BODY),
        "同一模型任务必须包含 Speaker 与 Body：{user}"
    );
    assert!(
        user.find(MV_SPEAKER) < user.find(MV_BODY),
        "MV 对话必须按 Speaker、Body 的自然顺序请求：{user}"
    );
    assert_eq!(
        read_owner_units(&database, "builtin"),
        vec![
            (json!(MV_SPEAKER), Some(json!(MV_SPEAKER_TRANSLATION)),),
            (json!([MV_BODY]), Some(json!([MV_BODY_TRANSLATION])),),
        ],
        "模型响应必须按语义 Unit 提交到项目数据库"
    );

    assert_success(
        "MV 对话 WriteBack",
        &run_att(root, arguments(&["mv", "write-back", "--name", PROJECT])),
    );
    let output_root = workspace.join("write_back");
    let output_map: Value = serde_json::from_slice(
        &fs::read(output_root.join("www/data/Map001.json")).expect("MV 写回 Map 应存在"),
    )
    .expect("MV 写回 Map 必须是 JSON");
    let commands = output_map["events"][1]["pages"][0]["list"]
        .as_array()
        .expect("MV 写回事件命令必须是数组");
    assert_eq!(commands[0]["code"], 101);
    assert_eq!(commands[0]["parameters"], json!(["", 0, 0, 2]));
    assert_eq!(commands[1]["code"], 401);
    assert_eq!(
        commands[1]["parameters"][0],
        format!(r"\n<{MV_SPEAKER_TRANSLATION}>{MV_BODY_TRANSLATION}")
    );
    assert_eq!(commands[2]["code"], 0);
    assert_eq!(
        fs::read_to_string(output_root.join("www/js/rpg_core.js")).expect("MV core 应保留"),
        "/* MV core */"
    );
    assert!(
        !output_root.join("data").exists() && !output_root.join("js").exists(),
        "MV 写回不得把 www 内容提升到输出根"
    );

    let original_map: Value = serde_json::from_slice(
        &fs::read(game.join("www/data/Map001.json")).expect("外部 MV Map 应保留"),
    )
    .expect("外部 MV Map 必须保持有效 JSON");
    assert_eq!(
        original_map["events"][1]["pages"][0]["list"][1]["parameters"][0],
        format!(r"\n<{MV_SPEAKER}>{MV_BODY}"),
        "WriteBack 不得修改外部 MV 游戏"
    );
}

#[test]
fn rules_owner_replaces_writes_back_and_disables_without_touching_builtin() {
    let temporary = tempfile::tempdir().expect("应可建立 Rules 端到端测试目录");
    let root = temporary.path();
    let game = root.join("mz-game");
    write_minimal_mz_game(&game);
    write_extract_rules(root, Some("customShortName"));
    write_rpg_maker_prompt(root);

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("本地模型服务端口应可绑定");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("本地模型地址应可读取")
    );
    write_configuration(root, &endpoint);

    assert_success(
        "Rules 项目 Init",
        &run_att(root, init_arguments("mz", &game)),
    );
    assert_success(
        "Builtin 与 Rules 初次 Extract",
        &run_att(
            root,
            arguments(&[
                "mz",
                "extract",
                "--name",
                PROJECT,
                "--builtin",
                "--rules",
                "rules.toml",
            ]),
        ),
    );

    let workspace = root.join("projects/mz").join(PROJECT);
    let database = workspace.join("project.db");
    assert_eq!(
        read_owner_units(&database, "builtin"),
        vec![(json!(SOURCE_TEXT), None)]
    );
    assert_eq!(
        read_owner_units(&database, "rules"),
        vec![(json!(RULES_SHORT_SOURCE), None)]
    );

    let server = thread::spawn(move || {
        serve_one_response(
            listener,
            json!({
                "1": [TRANSLATION],
                "2": [RULES_SHORT_TRANSLATION]
            }),
        )
    });
    assert_success(
        "Builtin 与 Rules Translate",
        &run_att(
            root,
            arguments(&["mz", "translate", "--name", PROJECT, "local"]),
        ),
    );
    let request = server
        .join()
        .expect("Rules 模型服务线程不得 panic")
        .expect("Rules 模型服务必须完成请求");
    let user = request["messages"][1]["content"]
        .as_str()
        .expect("Rules user message 必须是字符串");
    assert!(
        user.contains(SOURCE_TEXT) && user.contains(RULES_SHORT_SOURCE),
        "同一翻译运行必须读取 Builtin 与 Rules owner：{user}"
    );
    assert_success(
        "Rules 初次 WriteBack",
        &run_att(root, arguments(&["mz", "write-back", "--name", PROJECT])),
    );
    let output_items = read_items(&workspace.join("write_back/data/Items.json"));
    assert_eq!(output_items[1]["description"], TRANSLATION);
    assert_eq!(output_items[1]["customShortName"], RULES_SHORT_TRANSLATION);

    write_extract_rules(root, Some("customLongName"));
    assert_success(
        "Rules 定义替换",
        &run_att(
            root,
            arguments(&["mz", "extract", "--name", PROJECT, "--rules", "rules.toml"]),
        ),
    );
    assert_eq!(
        read_owner_units(&database, "builtin"),
        vec![(json!(SOURCE_TEXT), Some(json!(TRANSLATION)))],
        "仅替换 Rules 不得扰动 Builtin 资产或译文"
    );
    assert_eq!(
        read_owner_units(&database, "rules"),
        vec![(json!(RULES_LONG_SOURCE), None)],
        "新 Rules 定义必须原子替换旧 owner"
    );
    assert_success(
        "Rules 替换后 WriteBack",
        &run_att(root, arguments(&["mz", "write-back", "--name", PROJECT])),
    );
    let output_items = read_items(&workspace.join("write_back/data/Items.json"));
    assert_eq!(output_items[1]["description"], TRANSLATION);
    assert_eq!(output_items[1]["customShortName"], RULES_SHORT_SOURCE);
    assert_eq!(output_items[1]["customLongName"], RULES_LONG_SOURCE);

    write_extract_rules(root, None);
    let disabled = run_att(
        root,
        arguments(&["mz", "extract", "--name", PROJECT, "--rules", "rules.toml"]),
    );
    assert_success("Rules 显式停用", &disabled);
    let disabled_stdout = String::from_utf8_lossy(&disabled.stdout);
    assert!(
        disabled_stdout.contains("已停用 owner") && disabled_stdout.contains("Rules"),
        "显式空 Rules 必须向操作者说明该 owner 已停用：{disabled_stdout}"
    );
    assert!(read_owner_units(&database, "rules").is_empty());
    assert_eq!(
        read_owner_units(&database, "builtin"),
        vec![(json!(SOURCE_TEXT), Some(json!(TRANSLATION)))],
        "停用 Rules 不得删除 Builtin"
    );
    let connection = Connection::open(&database).expect("项目数据库应可打开");
    let saved_extract_plans: i64 = connection
        .query_row("SELECT count(*) FROM extract_run_plan", [], |row| {
            row.get(0)
        })
        .expect("Extract 运行方案应可读取");
    assert_eq!(
        saved_extract_plans, 0,
        "仅显式停用 Rules 后不得保留空自动 Extract 方案"
    );
    drop(connection);

    assert_success(
        "Rules 停用后 WriteBack",
        &run_att(root, arguments(&["mz", "write-back", "--name", PROJECT])),
    );
    let output_items = read_items(&workspace.join("write_back/data/Items.json"));
    assert_eq!(output_items[1]["description"], TRANSLATION);
    assert_eq!(output_items[1]["customShortName"], RULES_SHORT_SOURCE);
    assert_eq!(output_items[1]["customLongName"], RULES_LONG_SOURCE);

    let omitted = run_att(root, arguments(&["mz", "extract", "--name", PROJECT]));
    assert!(
        !omitted.status.success(),
        "停用唯一自动 owner 后省略全部 Extract 选项必须失败"
    );
}

#[test]
fn generic_reextract_preserves_moves_and_rejects_unextracted_changes() {
    const GENERIC_SOURCE: &str = "こんにちは\n世界";
    const GENERIC_TRANSLATION: &str = "你好\n世界";
    const GENERIC_REVISION: &str = "您好\n世界";
    const UPDATED_SOURCE: &str = "こんばんは\n世界";

    let temporary = tempfile::tempdir().expect("应可建立 Generic 端到端测试目录");
    let root = temporary.path();
    let input = root.join("jsonl");
    fs::create_dir(&input).expect("Generic 输入目录应可建立");
    write_generic_duplicate_group(&input.join("story.jsonl"), GENERIC_SOURCE, false);
    write_generic_prompt(root);

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("本地模型服务端口应可绑定");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("本地模型地址应可读取")
    );
    write_configuration(root, &endpoint);

    let mut init = arguments(&["generic", "init", "--name", PROJECT, "--path"]);
    init.push(input.as_os_str().to_owned());
    init.extend(arguments(&[
        "--source-language",
        "ja",
        "--target-language",
        "zh-Hans",
    ]));
    assert_success("Generic Init", &run_att(root, init));
    assert_success(
        "Generic Extract",
        &run_att(root, arguments(&["generic", "extract", "--name", PROJECT])),
    );

    let server =
        thread::spawn(move || serve_one_generic_translation(listener, GENERIC_TRANSLATION));
    assert_success(
        "Generic Translate",
        &run_att(
            root,
            arguments(&["generic", "translate", "--name", PROJECT, "local"]),
        ),
    );
    let request = server
        .join()
        .expect("Generic 本地模型线程不得 panic")
        .expect("Generic 本地模型服务必须完成请求");
    assert!(
        request["messages"][1]["content"]
            .as_str()
            .is_some_and(|content| content.contains("こんにちは")),
        "Generic user message 必须包含待译 Group"
    );
    assert_success(
        "Generic WriteBack",
        &run_att(
            root,
            arguments(&["generic", "write-back", "--name", PROJECT]),
        ),
    );

    let workspace = root.join("projects/generic").join(PROJECT);
    let first_output = workspace.join("write_back/story.jsonl");
    assert_eq!(
        read_generic_texts(&first_output),
        vec![
            GENERIC_TRANSLATION.to_owned(),
            GENERIC_TRANSLATION.to_owned()
        ],
        "Generic 全局去重必须向同文未译 Unit 传播字符串译文"
    );
    assert_eq!(
        read_generic_texts(&input.join("story.jsonl")),
        vec![GENERIC_SOURCE.to_owned(), GENERIC_SOURCE.to_owned()],
        "Generic WriteBack 不得修改外部 JSONL"
    );

    let override_script = workspace_root().join("docs/lua/examples/generic-override.lua");
    let mut lua_arguments = arguments(&["generic", "lua", "--name", PROJECT]);
    lua_arguments.push(override_script.into_os_string());
    lua_arguments.push("--".into());
    lua_arguments.extend(arguments(&["scene-1", "line-1", GENERIC_REVISION]));
    assert_success("Generic Lua 精确修订", &run_att(root, lua_arguments));
    assert_success(
        "Generic 多种 Current 收敛",
        &run_att(
            root,
            arguments(&["generic", "translate", "--name", PROJECT]),
        ),
    );
    assert_success(
        "Generic Lua 修订后 WriteBack",
        &run_att(
            root,
            arguments(&["generic", "write-back", "--name", PROJECT]),
        ),
    );
    assert_eq!(
        read_generic_texts(&first_output),
        vec![GENERIC_REVISION.to_owned(), GENERIC_TRANSLATION.to_owned()],
        "Generic Lua 必须精确修改指定 Unit，不能向同文 Unit 传播"
    );

    let moved_directory = input.join("nested");
    fs::create_dir(&moved_directory).expect("嵌套输入目录应可建立");
    let moved_input = moved_directory.join("moved.jsonl");
    write_generic_duplicate_group(&moved_input, GENERIC_SOURCE, true);
    fs::remove_file(input.join("story.jsonl")).expect("旧 JSONL 位置应可删除");
    assert_success(
        "Generic 移动后 Extract",
        &run_att(root, arguments(&["generic", "extract", "--name", PROJECT])),
    );
    assert_success(
        "Generic 移动后 WriteBack",
        &run_att(
            root,
            arguments(&["generic", "write-back", "--name", PROJECT]),
        ),
    );
    let moved_output = workspace.join("write_back/nested/moved.jsonl");
    assert_eq!(
        read_generic_texts(&moved_output),
        vec![GENERIC_REVISION.to_owned(), GENERIC_TRANSLATION.to_owned()],
        "只移动文件并改变等价 JSON 书写不得清除人工译文"
    );
    assert!(
        !first_output.exists(),
        "Generic 发布必须一次替换整个 write_back"
    );

    write_generic_duplicate_group(&moved_input, UPDATED_SOURCE, false);
    let stale_write_back = run_att(
        root,
        arguments(&["generic", "write-back", "--name", PROJECT]),
    );
    assert!(
        !stale_write_back.status.success(),
        "输入变化后未重新 Extract 必须拒绝 WriteBack"
    );
    assert_eq!(
        read_generic_texts(&moved_output),
        vec![GENERIC_REVISION.to_owned(), GENERIC_TRANSLATION.to_owned()],
        "拒绝过期 WriteBack 时必须保留上次成功输出"
    );

    assert_success(
        "Generic 原文变化后 Extract",
        &run_att(root, arguments(&["generic", "extract", "--name", PROJECT])),
    );
    assert_success(
        "Generic 原文变化后 WriteBack",
        &run_att(
            root,
            arguments(&["generic", "write-back", "--name", PROJECT]),
        ),
    );
    assert_eq!(
        read_generic_texts(&moved_output),
        vec![UPDATED_SOURCE.to_owned(), UPDATED_SOURCE.to_owned()],
        "原文变化必须清除该 Group 译文，未译 Unit 写回当前原文"
    );
}

#[test]
fn malformed_configuration_does_not_echo_api_key() {
    const SECRET: &str = "e2e-api-key-must-not-appear";

    let temporary = tempfile::tempdir().expect("应可建立配置诊断测试目录");
    let root = temporary.path();
    fs::write(
        root.join("config.toml"),
        format!(
            r#"[projects]
root = "projects"

[llm.clients.invalid]
url = "https://example.invalid/v1/chat/completions"
api_key = "{SECRET}" "invalid"
"#
        ),
    )
    .expect("无效配置夹具应可写入");

    let output = run_att(
        root,
        arguments(&["mz", "extract", "--name", PROJECT, "--builtin"]),
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("stderr 必须是 UTF-8");
    assert!(!stderr.is_empty(), "配置失败必须输出诊断");
    assert!(
        stderr.contains("config.toml"),
        "配置诊断必须指出失败文件：{stderr}"
    );
    assert!(
        !stderr.contains(SECRET),
        "配置诊断不得回显 api_key：{stderr}"
    );
}

#[test]
fn generic_lua_syntax_failure_is_logged_and_reported_before_project_open() {
    let temporary = tempfile::tempdir().expect("应可建立 Generic Lua 失败测试目录");
    let root = temporary.path();
    write_configuration(root, "http://127.0.0.1:9/v1/chat/completions");
    let script = root.join("invalid-generic.lua");
    fs::write(&script, "local =\n").expect("无效 Lua 脚本应可写入");

    let mut lua_arguments = arguments(&["generic", "lua", "--name", PROJECT]);
    lua_arguments.push(script.into_os_string());
    let output = run_att(root, lua_arguments);

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).expect("stderr 必须是 UTF-8");
    assert!(
        stderr.contains("near '='"),
        "语法诊断必须保留 Lua 编译器给出的具体原因：{stderr}"
    );

    let logs = root.join("projects/generic").join(PROJECT).join("logs");
    let log_paths = fs::read_dir(&logs)
        .expect("语法预检开始前应建立项目日志")
        .map(|entry| entry.expect("日志目录项应可读取").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect::<Vec<_>>();
    assert_eq!(log_paths.len(), 1, "一次失败命令只应产生一份项目日志");
    let records = fs::read_to_string(&log_paths[0]).expect("失败项目日志应可读取");
    let records = records
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("项目日志行必须是 JSON"))
        .collect::<Vec<_>>();
    assert!(
        records.iter().any(|record| record["code"] == "lua.script"),
        "语法失败日志必须保存脚本身份和哈希"
    );
    let failure = records
        .iter()
        .find(|record| record["code"] == "failure.reported")
        .expect("语法失败日志必须保存主错误");
    assert_eq!(
        failure["payload"]["diagnostic"]["reason"]["kind"],
        "failure_with_detail"
    );
    assert!(
        failure["payload"]["diagnostic"]["reason"]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("near '='")),
        "日志诊断必须保留同一份 Lua 编译原因"
    );
    assert!(
        records.iter().any(|record| {
            record["code"] == "run.finished" && record["payload"]["outcome"] == "failed"
        }),
        "语法失败日志必须写入明确终态"
    );
}

#[test]
fn atomic_lua_documented_examples_commit_once_or_roll_back_once() {
    let temporary = tempfile::tempdir().expect("应可建立原子 Lua 端到端测试目录");
    let root = temporary.path();
    let game = root.join("mz-game");
    write_minimal_mz_game(&game);
    write_configuration(root, "http://127.0.0.1:9/v1/chat/completions");
    assert_success("Lua 项目 Init", &run_att(root, init_arguments("mz", &game)));

    let note_script = workspace_root().join("docs/lua/examples/project-note.lua");
    let mut note_arguments = arguments(&["mz", "lua", "--name", PROJECT]);
    note_arguments.push(note_script.into_os_string());
    note_arguments.push("--".into());
    note_arguments.extend(arguments(&["menu", "checked"]));
    assert_success("Lua 私有表提交", &run_att(root, note_arguments));

    let database = root.join("projects/mz").join(PROJECT).join("project.db");
    let connection = Connection::open(&database).expect("项目数据库应可重新打开");
    let note: String = connection
        .query_row("SELECT note FROM lua_notes WHERE key = 'menu'", [], |row| {
            row.get(0)
        })
        .expect("文档示例应提交私有表内容");
    assert_eq!(note, "checked");
    drop(connection);

    let rollback_script = workspace_root().join("docs/lua/examples/rollback.lua");
    let mut rollback_arguments = arguments(&["mz", "lua", "--name", PROJECT]);
    rollback_arguments.push(rollback_script.into_os_string());
    let rollback = run_att(root, rollback_arguments);
    assert!(!rollback.status.success(), "未捕获 Lua 错误必须让命令失败");

    let connection = Connection::open(&database).expect("回滚后数据库应可重新打开");
    let rollback_table_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = 'lua_rollback_example'",
            [],
            |row| row.get(0),
        )
        .expect("应可检查回滚示例表");
    assert_eq!(
        rollback_table_count, 0,
        "失败脚本建立的私有表也必须随外层事务回滚"
    );
    let note_after_rollback: String = connection
        .query_row("SELECT note FROM lua_notes WHERE key = 'menu'", [], |row| {
            row.get(0)
        })
        .expect("前一次成功事务不得受后续回滚影响");
    assert_eq!(note_after_rollback, "checked");
    drop(connection);

    let restricted_script = root.join("restricted.lua");
    fs::write(
        &restricted_script,
        r#"
assert(ctx.project.engine == "mz")
assert(io == nil)
assert(os == nil)
assert(package == nil)
assert(require == nil)
assert(loadfile == nil)
assert(dofile == nil)
assert(debug == nil)
assert(warn == nil)
"#,
    )
    .expect("受限 VM 测试脚本应可写入");
    let mut restricted_arguments = arguments(&["mz", "lua", "--name", PROJECT]);
    restricted_arguments.push(restricted_script.into_os_string());
    assert_success("Lua 受限 VM", &run_att(root, restricted_arguments));
}

fn run_information_command(root: &Path, arguments: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_att"))
        .current_dir(root)
        .args(arguments)
        .output()
        .expect("att.exe 应可执行");
    assert!(
        output.status.success(),
        "{arguments:?} 应成功：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("帮助输出必须是 UTF-8")
}

fn init_arguments(engine: &str, game_root: &Path) -> Vec<OsString> {
    let mut values = arguments(&[engine, "init", "--name", PROJECT, "--path"]);
    values.push(game_root.as_os_str().to_owned());
    values.extend(arguments(&[
        "--source-language",
        "ja",
        "--target-language",
        "zh-Hans",
        "--dialogue-max-fullwidth-chars",
        "20",
        "--scrolling-text-max-fullwidth-chars",
        "20",
        "--help-description-max-fullwidth-chars",
        "20",
    ]));
    values
}

fn arguments(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

fn run_att(root: &Path, arguments: Vec<OsString>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_att"));
    command
        .current_dir(root)
        .arg("--config")
        .arg(root.join("config.toml"))
        .args(["--ui-language", "zh-Hans", "--progress", "off"])
        .args(arguments);
    command.output().expect("att.exe 应可执行")
}

fn assert_success(stage: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{stage} 应成功\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write_configuration(root: &Path, endpoint: &str) {
    let configuration = format!(
        r#"[projects]
root = "projects"

[prompts]
root = "prompts"
locale = "zh-Hans"
thinking_output = false

[llm.clients.primary]
url = "{endpoint}"
api_key = "e2e-secret"
model = "e2e-model"
max_concurrent_requests = 2
connect_timeout_ms = 5000
read_timeout_ms = 10000
request_timeout_ms = 10000
proxy = false
additional_pem_files = []
retry_delays_ms = [10]
max_retry_after_ms = 1000
parameters = '''
{{}}
'''

[[languages]]
type = "japanese"
id = "ja"
minimum_kana_characters = 1
allowed_terms = []
quote_repair_pairs = [["“", "”"], ["‘", "’"]]

[translation]
record_translation_tasks = false

[[translation.profiles]]
id = "local"
llm_client = "primary"
target_task_user_message_characters = 10000
"#
    );
    fs::write(root.join("config.toml"), configuration).expect("测试配置应可写入");
}

fn write_rpg_maker_prompt(root: &Path) {
    let prompt_root = root.join("prompts/rpg_maker/zh-Hans");
    fs::create_dir_all(&prompt_root).expect("Prompt 目录应可建立");
    fs::write(
        prompt_root.join("system.md"),
        "Translate {{source_language}} into {{target_language}}. Return the required JSON object.",
    )
    .expect("system Prompt 应可写入");
}

fn write_mv_dialogue_rules(root: &Path) {
    fs::write(
        root.join("dialogue.toml"),
        "[[rule]]\npattern = '(?i)\\\\n<(?<speaker>[^>]*?)(?::)?>'\n",
    )
    .expect("MV 对话姓名投影规则应可写入");
}

fn write_extract_rules(root: &Path, field: Option<&str>) {
    let definition = field.map_or_else(
        || "rule = []\n".to_owned(),
        |field| format!("[[rule]]\nfile = \"Items.json\"\npath = '[].{field}'\n"),
    );
    fs::write(root.join("rules.toml"), definition).expect("Extract Rules 应可写入");
}

fn write_generic_prompt(root: &Path) {
    let prompt_root = root.join("prompts/generic/zh-Hans");
    fs::create_dir_all(&prompt_root).expect("Generic Prompt 目录应可建立");
    fs::write(
        prompt_root.join("system.md"),
        "Translate {{source_language}} into {{target_language}}. Return string values.",
    )
    .expect("Generic system Prompt 应可写入");
}

fn write_generic_group(path: &Path, text: &str, alternate_order: bool) {
    let line = if alternate_order {
        format!(
            "{{ \"units\" : [{{\"text\":{},\"id\":\"line-1\"}}], \"kind\" : \"dialogue\", \"id\" : \"scene-1\" }}",
            serde_json::to_string(text).expect("Generic text 应可编码")
        )
    } else {
        json!({
            "id": "scene-1",
            "kind": "dialogue",
            "units": [{ "id": "line-1", "text": text }]
        })
        .to_string()
    };
    fs::write(path, format!("{line}\n")).expect("Generic JSONL 夹具应可写入");
}

fn write_generic_duplicate_group(path: &Path, text: &str, alternate_order: bool) {
    let encoded = serde_json::to_string(text).expect("Generic text 应可编码");
    let line = if alternate_order {
        format!(
            "{{ \"units\" : [{{\"text\":{encoded},\"id\":\"line-1\"}},{{\"id\":\"line-2\",\"text\":{encoded}}}], \"kind\" : \"dialogue\", \"id\" : \"scene-1\" }}"
        )
    } else {
        json!({
            "id": "scene-1",
            "kind": "dialogue",
            "units": [
                { "id": "line-1", "text": text },
                { "id": "line-2", "text": text }
            ]
        })
        .to_string()
    };
    fs::write(path, format!("{line}\n")).expect("Generic JSONL 夹具应可写入");
}

fn read_generic_texts(path: &Path) -> Vec<String> {
    let source = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("{} 应可读取：{error}", path.display()));
    let group: Value = serde_json::from_str(
        source
            .lines()
            .next()
            .expect("Generic JSONL 输出必须包含 Group"),
    )
    .expect("Generic JSONL 输出必须可解析");
    group["units"]
        .as_array()
        .expect("Generic units 必须是数组")
        .iter()
        .map(|unit| {
            unit["text"]
                .as_str()
                .expect("Generic text 必须是字符串")
                .to_owned()
        })
        .collect()
}

fn write_minimal_mz_game(game_root: &Path) {
    let data = game_root.join("data");
    let js = game_root.join("js");
    fs::create_dir_all(&data).expect("MZ data 目录应可建立");
    fs::create_dir_all(&js).expect("MZ js 目录应可建立");

    for file in [
        "Actors.json",
        "Armors.json",
        "Classes.json",
        "CommonEvents.json",
        "Enemies.json",
        "Skills.json",
        "States.json",
        "Troops.json",
        "Weapons.json",
    ] {
        fs::write(data.join(file), b"[null]").expect("空 RPG Maker 数据文件应可写入");
    }
    fs::write(
        data.join("Items.json"),
        serde_json::to_vec(&json!([
            null,
            {
                "id": 1,
                "name": "",
                "description": SOURCE_TEXT,
                "customShortName": RULES_SHORT_SOURCE,
                "customLongName": RULES_LONG_SOURCE
            }
        ]))
        .expect("Items 夹具应可序列化"),
    )
    .expect("Items 夹具应可写入");
    fs::write(
        data.join("System.json"),
        serde_json::to_vec(&json!({
            "gameTitle": "",
            "currencyUnit": "",
            "terms": { "basic": [], "commands": [], "params": [], "messages": {} },
            "elements": [],
            "skillTypes": [],
            "weaponTypes": [],
            "armorTypes": [],
            "equipTypes": []
        }))
        .expect("System 夹具应可序列化"),
    )
    .expect("System 夹具应可写入");
    fs::write(
        data.join("Map001.json"),
        serde_json::to_vec(&json!({ "displayName": "", "events": [null] }))
            .expect("Map 夹具应可序列化"),
    )
    .expect("Map 夹具应可写入");
    fs::write(js.join("plugins.js"), "/* ATT e2e */").expect("plugins.js 应可写入");
    fs::write(js.join("rmmz_core.js"), "/* MZ core */").expect("MZ core 标记应可写入");
}

fn write_minimal_mv_game(game_root: &Path) {
    let content_root = game_root.join("www");
    write_minimal_mz_game(&content_root);
    let data = content_root.join("data");
    let js = content_root.join("js");
    fs::remove_file(js.join("rmmz_core.js")).expect("MZ core 标记应可删除");
    fs::write(js.join("rpg_core.js"), "/* MV core */").expect("MV core 标记应可写入");
    fs::write(data.join("Items.json"), b"[null]").expect("MV Items 夹具应可写入");
    fs::write(
        data.join("Map001.json"),
        serde_json::to_vec(&json!({
            "displayName": "",
            "events": [
                null,
                {
                    "id": 1,
                    "name": "Dialogue",
                    "note": "",
                    "pages": [{
                        "conditions": {},
                        "image": {},
                        "moveRoute": { "list": [{ "code": 0, "parameters": [] }] },
                        "list": [
                            { "code": 101, "indent": 0, "parameters": ["", 0, 0, 2] },
                            {
                                "code": 401,
                                "indent": 0,
                                "parameters": ["\\n<アリス>こんにちは、世界！"]
                            },
                            { "code": 0, "indent": 0, "parameters": [] }
                        ]
                    }]
                }
            ]
        }))
        .expect("MV Map 夹具应可序列化"),
    )
    .expect("MV Map 夹具应可写入");
}

fn read_owner_units(database: &Path, owner: &str) -> Vec<(Value, Option<Value>)> {
    let connection = Connection::open(database).expect("项目数据库应可打开");
    let mut statement = connection
        .prepare(
            "SELECT unit.source_content_json, unit.translation_content_json
             FROM rpg_maker_text_unit AS unit
             JOIN rpg_maker_text_group AS group_row
               ON group_row.owner = unit.owner
              AND group_row.group_location = unit.group_location
             WHERE unit.owner = ?1
             ORDER BY group_row.group_order, unit.unit_order",
        )
        .expect("RPG Maker Unit 查询应可准备");
    statement
        .query_map([owner], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })
        .expect("RPG Maker Unit 查询应可执行")
        .map(|row| {
            let (source, translation) = row.expect("RPG Maker Unit 应可读取");
            (
                serde_json::from_str(&source).expect("Unit source 必须是规范 JSON"),
                translation.map(|translation| {
                    serde_json::from_str(&translation).expect("Unit translation 必须是规范 JSON")
                }),
            )
        })
        .collect()
}

fn read_items(path: &Path) -> Value {
    serde_json::from_slice(
        &fs::read(path).unwrap_or_else(|error| panic!("{} 应可读取：{error}", path.display())),
    )
    .expect("Items.json 必须是有效 JSON")
}

fn serve_one_translation(listener: TcpListener) -> Result<Value, String> {
    serve_one_response(listener, json!({ "1": [TRANSLATION] }))
}

fn serve_one_generic_translation(
    listener: TcpListener,
    translation: &str,
) -> Result<Value, String> {
    serve_one_response(listener, json!({ "1": translation }))
}

fn serve_one_response(listener: TcpListener, model_output: Value) -> Result<Value, String> {
    let (mut stream, request) = accept_request(listener)?;
    let content = serde_json::to_string(&model_output).map_err(|error| error.to_string())?;
    write_chat_response(&mut stream, &content)?;
    Ok(request)
}

fn accept_request(listener: TcpListener) -> Result<(TcpStream, Value), String> {
    listener
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut stream, _) = loop {
        match listener.accept() {
            Ok(connection) => break connection,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err("等待 ATT 模型请求超时".to_owned());
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.to_string()),
        }
    };
    // Windows 会让 accept 得到的 socket 继承 listener 的非阻塞状态。请求正文可能尚未
    // 全部到达，因此在按超时读取 HTTP 前恢复为阻塞模式。
    stream
        .set_nonblocking(false)
        .map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    let request = read_http_json(&mut stream)?;
    Ok((stream, request))
}

fn write_chat_response(stream: &mut TcpStream, content: &str) -> Result<(), String> {
    let body = json!({
        "id": "response-e2e",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 11,
            "completion_tokens": 3,
            "total_tokens": 14
        }
    })
    .to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nx-request-id: request-e2e\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .map_err(|error| error.to_string())?;
    stream.flush().map_err(|error| error.to_string())?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

fn read_http_json(stream: &mut TcpStream) -> Result<Value, String> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];
    let header_end = loop {
        let read = stream
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("HTTP 请求在 header 完成前结束".to_owned());
        }
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(position) = find_subslice(&bytes, b"\r\n\r\n") {
            break position + 4;
        }
    };
    let header =
        std::str::from_utf8(&bytes[..header_end - 4]).map_err(|error| error.to_string())?;
    let content_length = header
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .ok_or_else(|| "HTTP 请求缺少有效 Content-Length".to_owned())?;
    while bytes.len() < header_end + content_length {
        let read = stream
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("HTTP 请求 body 提前结束".to_owned());
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    serde_json::from_slice(&bytes[header_end..header_end + content_length])
        .map_err(|error| error.to_string())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}
