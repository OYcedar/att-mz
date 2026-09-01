#![cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]

//! Windows x64 生产进程边界的多引擎 CLI 与 RPG Maker 主流程黑盒测试。

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc;
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
const MV_BODY_WRITE_BACK: &str = "你好、世界！";
const RULES_SHORT_SOURCE: &str = "ポーション";
const RULES_SHORT_TRANSLATION: &str = "治疗药水";
const RULES_LONG_SOURCE: &str = "高級ポーション";
const THINKING_PROMPT: &str = "在 think 中写出影响译文的判断。";
const THINKING_SENTINEL: &str = "PRIVATE_THINKING_SENTINEL";
const PARTIAL_RETRY_SOURCES: [&str; 4] = [
    "春の便りです",
    "夏の便りです",
    "秋の便りです",
    "冬の便りです",
];
const PARTIAL_RETRY_TRANSLATIONS: [&str; 4] = ["春日来信", "夏日来信", "秋日来信", "冬日来信"];

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
        for command in [
            "init",
            "extract",
            "translate",
            "write-back",
            "manual",
            "lua",
        ] {
            assert!(
                help.contains(command),
                "{engine} 帮助必须列出 {command}：\n{help}"
            );
        }
    }
}

#[test]
fn manual_export_check_and_apply_work_for_mv_and_mz() {
    for (engine, game_directory) in [("mv", "mv-manual-game"), ("mz", "mz-manual-game")] {
        let temporary = tempfile::tempdir().expect("应可建立 RPG Maker Manual 测试目录");
        let root = temporary.path();
        let game = root.join(game_directory);
        if engine == "mv" {
            write_minimal_mv_game(&game);
        } else {
            write_minimal_mz_game(&game);
        }
        let data = if engine == "mv" {
            game.join("www/data")
        } else {
            game.join("data")
        };
        fs::write(
            data.join("Items.json"),
            serde_json::to_vec(&json!([
                null,
                {
                    "id": 1,
                    "name": "回復薬",
                    "description": "一行目\n二行目"
                }
            ]))
            .expect("Manual Items 夹具应可序列化"),
        )
        .expect("Manual Items 夹具应可写入");
        fs::write(
            data.join("Map001.json"),
            serde_json::to_vec(&json!({ "displayName": "", "events": [null] }))
                .expect("Manual Map 夹具应可序列化"),
        )
        .expect("Manual Map 夹具应可写入");
        write_configuration(root, "http://127.0.0.1:9/v1/chat/completions");

        assert_success(
            &format!("{engine} Manual Init"),
            &run_att(root, init_arguments(engine, &game)),
        );
        assert_success(
            &format!("{engine} Manual Extract"),
            &run_att(
                root,
                arguments(&[engine, "extract", "--name", PROJECT, "--builtin"]),
            ),
        );

        let manual = root.join(format!("{engine}-manual.toml"));
        let mut export = arguments(&[engine, "manual", "export", "--name", PROJECT]);
        export.push(manual.as_os_str().to_owned());
        assert_success(&format!("{engine} Manual export"), &run_att(root, export));
        let document = read_manual_toml(&manual);
        let entries = document["translation"]
            .as_array()
            .expect("Manual translation 必须是数组");
        assert_eq!(entries.len(), 2, "只应导出两个真正需要翻译的条目");
        let name = find_manual_entry(entries, "Items.json:1:name");
        assert_eq!(name["type"].as_str(), Some("fixed"));
        assert_eq!(
            name["source"].as_array().expect("name source 必须是数组"),
            &[toml::Value::String("回復薬".to_owned())]
        );
        let description = find_manual_entry(entries, "Items.json:1:description");
        assert_eq!(description["type"].as_str(), Some("free"));
        assert_eq!(
            description["source"]
                .as_array()
                .expect("description source 必须是数组"),
            &[
                toml::Value::String("一行目".to_owned()),
                toml::Value::String("二行目".to_owned()),
            ]
        );
        for entry in entries {
            let table = entry.as_table().expect("Manual 条目必须是 table");
            assert_eq!(table.len(), 4);
            assert!(
                table.keys().all(|key| {
                    matches!(key.as_str(), "id" | "type" | "source" | "translation")
                })
            );
        }

        let mut check = arguments(&[engine, "manual", "check", "--name", PROJECT]);
        check.push(manual.as_os_str().to_owned());
        assert_success(
            &format!("{engine} Manual check 未填写"),
            &run_att(root, check),
        );

        set_manual_toml_field(
            &manual,
            "Items.json:1:name",
            "translation",
            toml::Value::Array(vec![toml::Value::String("恢复药".to_owned())]),
        );
        let mut check = arguments(&[engine, "manual", "check", "--name", PROJECT]);
        check.push(manual.as_os_str().to_owned());
        assert_success(
            &format!("{engine} Manual check 混合条目"),
            &run_att(root, check),
        );
        let mut apply = arguments(&[engine, "manual", "apply", "--name", PROJECT]);
        apply.push(manual.as_os_str().to_owned());
        assert_success(
            &format!("{engine} Manual apply 单项"),
            &run_att(root, apply),
        );

        let workspace = distribution_root(root)
            .join("projects")
            .join(engine)
            .join(PROJECT);
        let database = workspace.join("project.db");
        let connection = Connection::open(&database).expect("Manual 项目数据库应可打开");
        let count: i64 = connection
            .query_row(
                "SELECT count(*) FROM rpg_maker_manual_translation",
                [],
                |row| row.get(0),
            )
            .expect("人工译文数量应可读取");
        assert_eq!(count, 1);
        drop(connection);

        set_manual_toml_field(
            &manual,
            "Items.json:1:description",
            "translation",
            toml::Value::Array(vec![toml::Value::String("合并说明".to_owned())]),
        );
        set_manual_toml_field(
            &manual,
            "Items.json:1:name",
            "source",
            toml::Value::Array(vec![toml::Value::String("错误原文".to_owned())]),
        );
        let mut invalid_apply = arguments(&[engine, "manual", "apply", "--name", PROJECT]);
        invalid_apply.push(manual.as_os_str().to_owned());
        let invalid = run_att(root, invalid_apply);
        assert_eq!(invalid.status.code(), Some(1));
        let stderr = String::from_utf8(invalid.stderr).expect("Manual 错误必须是 UTF-8");
        assert!(stderr.contains("Items.json:1:name") && stderr.contains("重新运行 manual export"));
        assert!(!stderr.contains("group_location") && !stderr.contains("unit_role"));
        let connection = Connection::open(&database).expect("失败后项目数据库应可打开");
        let count: i64 = connection
            .query_row(
                "SELECT count(*) FROM rpg_maker_manual_translation",
                [],
                |row| row.get(0),
            )
            .expect("失败后人工译文数量应可读取");
        assert_eq!(count, 1, "混合有效与无效条目必须原子失败");
        drop(connection);

        set_manual_toml_field(
            &manual,
            "Items.json:1:name",
            "source",
            toml::Value::Array(vec![toml::Value::String("回復薬".to_owned())]),
        );
        let mut apply = arguments(&[engine, "manual", "apply", "--name", PROJECT]);
        apply.push(manual.as_os_str().to_owned());
        assert_success(
            &format!("{engine} Manual apply 全部"),
            &run_att(root, apply),
        );
        assert_success(
            &format!("{engine} Manual WriteBack"),
            &run_att(root, arguments(&[engine, "write-back", "--name", PROJECT])),
        );
        let output = if engine == "mv" {
            workspace.join("write_back/www/data/Items.json")
        } else {
            workspace.join("write_back/data/Items.json")
        };
        let items = read_items(&output);
        assert_eq!(items[1]["name"], "恢复药");
        assert_eq!(items[1]["description"], "合并说明");

        fs::write(
            data.join("Items.json"),
            serde_json::to_vec(&json!([
                null,
                {
                    "id": 1,
                    "name": "新しい薬",
                    "description": "一行目\n二行目"
                }
            ]))
            .expect("变化后的 Manual Items 应可序列化"),
        )
        .expect("变化后的 Manual Items 应可写入");
        assert_success(
            &format!("{engine} Manual 原文变化后 Init"),
            &run_att(root, init_arguments(engine, &game)),
        );
        assert_success(
            &format!("{engine} Manual 原文变化后 Extract"),
            &run_att(
                root,
                arguments(&[engine, "extract", "--name", PROJECT, "--builtin"]),
            ),
        );
        let after_change_manual = root.join(format!("{engine}-after-change.toml"));
        let mut after_change_export = arguments(&[engine, "manual", "export", "--name", PROJECT]);
        after_change_export.push(after_change_manual.as_os_str().to_owned());
        assert_success(
            &format!("{engine} Manual 原文变化后 export"),
            &run_att(root, after_change_export),
        );
        let after_change = read_manual_toml(&after_change_manual);
        let after_change_entries = after_change["translation"]
            .as_array()
            .expect("变化后 Manual translation 必须是数组");
        assert_eq!(after_change_entries.len(), 1);
        find_manual_entry(after_change_entries, "Items.json:1:name");
        assert_success(
            &format!("{engine} Manual 原文变化后 WriteBack"),
            &run_att(root, arguments(&[engine, "write-back", "--name", PROJECT])),
        );
        let items = read_items(&output);
        assert_eq!(items[1]["name"], "新しい薬");
        assert_eq!(items[1]["description"], "合并说明");
        let connection = Connection::open(&database).expect("过期人工译文数据库应可打开");
        let count: i64 = connection
            .query_row(
                "SELECT count(*) FROM rpg_maker_manual_translation",
                [],
                |row| row.get(0),
            )
            .expect("过期人工译文数量应可读取");
        assert_eq!(count, 2, "过期人工译文必须保留，不能静默删除");
    }
}

#[test]
fn mz_standard_bootstrap_titles_follow_the_single_game_title_unit() {
    let temporary = tempfile::tempdir().expect("应可建立启动标题纵向测试目录");
    let root = temporary.path();
    let game = root.join("mz-bootstrap-title-game");
    write_minimal_mz_game(&game);
    let system_path = game.join("data/System.json");
    let mut system: Value =
        serde_json::from_slice(&fs::read(&system_path).expect("System 夹具应可读取"))
            .expect("System 夹具应为 JSON");
    system["gameTitle"] = Value::String("原题".to_owned());
    fs::write(
        &system_path,
        serde_json::to_vec(&system).expect("System 夹具应可序列化"),
    )
    .expect("System 夹具应可写入");
    let package = r#"{"name":"demo","main":"index.html","window" : {"title" : "原题", "width":816},"title":"原题"}"#;
    let html = r#"<head><title>原题</title><meta name="title" content="原题"></head><body title="原题">原题</body>"#;
    fs::write(game.join("package.json"), package).expect("package 夹具应可写入");
    fs::write(game.join("index.html"), html).expect("HTML 夹具应可写入");
    write_configuration(root, "http://127.0.0.1:9/v1/chat/completions");

    assert_success("启动标题 Init", &run_att(root, init_arguments("mz", &game)));
    assert_success(
        "启动标题 Extract",
        &run_att(
            root,
            arguments(&["mz", "extract", "--name", PROJECT, "--builtin"]),
        ),
    );
    let manual = root.join("bootstrap-title.toml");
    let mut export = arguments(&["mz", "manual", "export", "--name", PROJECT]);
    export.push(manual.as_os_str().to_owned());
    assert_success("启动标题 Manual export", &run_att(root, export));
    let document = read_manual_toml(&manual);
    let entries = document["translation"]
        .as_array()
        .expect("Manual translation 必须是数组");
    assert_eq!(
        entries
            .iter()
            .filter(|entry| entry["id"].as_str() == Some("System.json:gameTitle"))
            .count(),
        1,
        "启动标题只能复用唯一 System.gameTitle Unit"
    );
    set_manual_toml_field(
        &manual,
        "System.json:gameTitle",
        "translation",
        toml::Value::Array(vec![toml::Value::String("译题".to_owned())]),
    );
    let mut apply = arguments(&["mz", "manual", "apply", "--name", PROJECT]);
    apply.push(manual.as_os_str().to_owned());
    assert_success("启动标题 Manual apply", &run_att(root, apply));
    assert_success(
        "启动标题 WriteBack",
        &run_att(root, arguments(&["mz", "write-back", "--name", PROJECT])),
    );

    let output = distribution_root(root)
        .join("projects/mz")
        .join(PROJECT)
        .join("write_back");
    let output_system: Value = serde_json::from_slice(
        &fs::read(output.join("data/System.json")).expect("输出 System 应可读取"),
    )
    .expect("输出 System 应为 JSON");
    assert_eq!(output_system["gameTitle"], "译题");
    assert_eq!(
        fs::read_to_string(output.join("package.json")).expect("输出 package 应可读取"),
        r#"{"name":"demo","main":"index.html","window" : {"title" : "译题", "width":816},"title":"原题"}"#
    );
    assert_eq!(
        fs::read_to_string(output.join("index.html")).expect("输出 HTML 应可读取"),
        r#"<head><title>译题</title><meta name="title" content="原题"></head><body title="原题">原题</body>"#
    );
    assert_eq!(
        fs::read_to_string(game.join("package.json")).expect("原 package 应可读取"),
        package
    );
    assert_eq!(
        fs::read_to_string(game.join("index.html")).expect("原 HTML 应可读取"),
        html
    );
}

#[test]
fn fixed_configuration_is_required_and_stage_commands_reject_lua_options() {
    let temporary = tempfile::tempdir().expect("应可建立 CLI 参数测试目录");
    let root = temporary.path();

    let missing_configuration = Command::new(stage_att_executable(root))
        .current_dir(root)
        .args(["mz", "extract", "--name", PROJECT, "--builtin"])
        .output()
        .expect("att.exe 应可执行");
    assert_eq!(missing_configuration.status.code(), Some(1));
    assert!(missing_configuration.stdout.is_empty());
    let missing_stderr = String::from_utf8_lossy(&missing_configuration.stderr);
    let fixed_configuration = distribution_root(root).join("config.toml");
    assert!(
        missing_stderr.contains(fixed_configuration.to_string_lossy().as_ref()),
        "缺失配置诊断必须报告可执行文件旁的固定绝对路径：{missing_stderr}"
    );

    let removed_argument = Command::new(stage_att_executable(root))
        .current_dir(root)
        .args([
            "--config",
            "elsewhere.toml",
            "mz",
            "extract",
            "--name",
            PROJECT,
            "--builtin",
        ])
        .output()
        .expect("att.exe 应可执行");
    assert_eq!(removed_argument.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&removed_argument.stderr).contains("--config"));

    for engine in ["mv", "mz", "generic"] {
        for stage in ["extract", "translate", "write-back"] {
            let mut arguments = vec![engine, stage, "--name", PROJECT, "--lua", "stage.lua"];
            if stage == "translate" {
                arguments.insert(4, "local");
            }
            let output = Command::new(stage_att_executable(root))
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

        let output = Command::new(stage_att_executable(root))
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

    // 可执行文件位于 release/，而这些相对输入必须继续以调用 cwd 为基准。
    assert_success(
        "MZ Init",
        &run_att(root, init_arguments("mz", Path::new("mz-game"))),
    );
    assert_success(
        "MV Init",
        &run_att(root, init_arguments("mv", Path::new("mv-game"))),
    );
    let mut generic_init = arguments(&["generic", "init", "--name", PROJECT, "--path"]);
    generic_init.push(OsString::from("jsonl"));
    generic_init.extend(arguments(&[
        "--source-language",
        "ja",
        "--target-language",
        "zh-Hans",
    ]));
    assert_success("Generic Init", &run_att(root, generic_init));

    let mz_workspace = distribution_root(root).join("projects/mz").join(PROJECT);
    let mv_workspace = distribution_root(root).join("projects/mv").join(PROJECT);
    let generic_workspace = distribution_root(root)
        .join("projects/generic")
        .join(PROJECT);
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
    let translate = run_att(
        root,
        arguments(&["mz", "translate", "--name", PROJECT, "local"]),
    );
    assert_success("MZ Translate", &translate);
    let request = server
        .join()
        .expect("本地模型服务线程不得 panic")
        .expect("本地模型服务必须完成一次请求");
    assert_eq!(request["model"], "e2e-model");
    let messages = request["messages"]
        .as_array()
        .expect("请求必须包含 messages 数组");
    assert_eq!(messages.len(), 2, "一次翻译请求只应包含 system 与 user");
    let user_message = parse_user_message(
        messages[1]["content"]
            .as_str()
            .expect("模型 user message 必须是字符串"),
    );
    assert!(
        user_message_texts(&user_message).contains(&SOURCE_TEXT),
        "模型 user message 必须在 JSON text 数组中包含待译原文"
    );
    assert!(
        messages[0]["content"]
            .as_str()
            .is_some_and(|content| content.contains(THINKING_PROMPT)),
        "正式成功路径必须加载 Thinking Prompt"
    );
    let task_record = read_single_task_record_sharing_log_run_id(&mz_workspace);
    assert!(task_record.contains("# 翻译任务"));
    assert!(
        task_record.contains("状态：完成，已确认提交"),
        "实际任务记录：\n{task_record}"
    );
    assert!(task_record.contains("要求译文：1 项"));
    assert!(task_record.contains("已接受：1 项（ID：0），写入 1 个实际位置"));
    assert!(task_record.contains("未接受：0 项（ID：—）"));
    assert!(task_record.contains(THINKING_SENTINEL));
    assert_eq!(task_record.matches("## Assistant").count(), 1);
    assert!(task_record.contains("## User"));
    assert!(!task_record.contains("## System"));
    assert!(!task_record.contains("## Thinking"));
    assert!(!task_record.contains("## Raw Assistant"));
    assert!(!task_record.contains("Endpoint"));
    assert!(!task_record.contains("Request attempts"));
    assert!(task_record.contains("\"translations\""));
    assert!(task_record.contains("## Assistant\n\n```json\n"));
    assert!(!task_record.contains("````text"));
    assert!(!task_record.contains("## JSON Repairs"));
    assert!(task_record.contains("## 最终结果"));
    assert!(
        task_record.find("## 最终结果").expect("应包含最终结果")
            < task_record.find("## User").expect("应包含 User"),
        "最终结果必须位于 User 与 Assistant 之前"
    );
    assert!(
        !String::from_utf8_lossy(&translate.stdout).contains(THINKING_SENTINEL)
            && !String::from_utf8_lossy(&translate.stderr).contains(THINKING_SENTINEL),
        "Thinking 正文不得进入终端输出"
    );
    assert_workspace_does_not_contain(&mz_workspace.join("logs"), THINKING_SENTINEL);
    assert!(
        !sqlite_contains_text(&mz_workspace.join("project.db"), THINKING_SENTINEL),
        "Thinking 正文不得进入权威数据库的任何逻辑列值"
    );

    const GENERIC_TRANSLATION: &str = "通用译文";
    let responses_listener =
        TcpListener::bind(("127.0.0.1", 0)).expect("Responses 本地模型端口应可绑定");
    let responses_base = format!(
        "http://{}/v1",
        responses_listener
            .local_addr()
            .expect("Responses 本地模型地址应可读取")
    );
    write_configuration_with_protocol(root, &responses_base, Some("responses"));
    assert_success(
        "Generic Extract",
        &run_att(root, arguments(&["generic", "extract", "--name", PROJECT])),
    );
    let server = thread::spawn(move || {
        serve_two_responses_outputs(
            responses_listener,
            json!({ "0": [GENERIC_TRANSLATION] }),
            json!({
                "0": [format!(r"\n<{MV_SPEAKER_TRANSLATION}>{MV_BODY_TRANSLATION}")]
            }),
        )
    });
    let translate = run_att(
        root,
        arguments(&["generic", "translate", "--name", PROJECT, "local"]),
    );
    assert_success("Generic Responses Translate", &translate);
    assert_eq!(
        read_generic_units(&generic_workspace.join("project.db")),
        vec![(
            "同名项目隔离".to_owned(),
            Some(GENERIC_TRANSLATION.to_owned())
        )],
        "Generic Responses 译文必须提交到自己的项目数据库"
    );

    let translate = run_att(
        root,
        arguments(&["mv", "translate", "--name", PROJECT, "local"]),
    );
    assert_success("MV Responses Translate", &translate);
    let [generic_request, mv_request] = server
        .join()
        .expect("Responses 本地模型服务线程不得 panic")
        .expect("Responses 本地模型服务必须完成两次请求");
    assert!(generic_request.get("messages").is_none());
    assert_eq!(generic_request["background"], false);
    let input = generic_request["input"]
        .as_array()
        .expect("Generic Responses 请求必须包含 input 数组");
    assert_eq!(input.len(), 2, "一次翻译请求只应包含 system 与 user");
    let user_message = parse_user_message(
        input[1]["content"]
            .as_str()
            .expect("Generic Responses user message 必须是字符串"),
    );
    assert!(
        user_message_texts(&user_message).contains(&"同名项目隔离"),
        "Responses user message 必须包含 Generic 待译原文"
    );
    assert!(mv_request.get("messages").is_none());
    assert_eq!(mv_request["background"], false);
    let input = mv_request["input"]
        .as_array()
        .expect("MV Responses 请求必须包含 input 数组");
    let user_message = parse_user_message(
        input[1]["content"]
            .as_str()
            .expect("MV Responses user message 必须是字符串"),
    );
    let expected_mv_source = format!(r"\n<{MV_SPEAKER}>{MV_BODY}");
    assert_eq!(
        user_message_texts(&user_message),
        [expected_mv_source.as_str()],
        "未配置 Speaker 投影规则时，MV Responses 请求必须保留完整对话行"
    );
    assert_eq!(
        read_generic_units(&generic_workspace.join("project.db")),
        vec![(
            "同名项目隔离".to_owned(),
            Some(GENERIC_TRANSLATION.to_owned())
        )],
        "同名 MV Responses Translate 不得改变 Generic 项目状态"
    );

    assert_success(
        "MZ WriteBack",
        &run_att(root, arguments(&["mz", "write-back", "--name", PROJECT])),
    );
    assert_success(
        "MV WriteBack",
        &run_att(root, arguments(&["mv", "write-back", "--name", PROJECT])),
    );
    assert_success(
        "Generic WriteBack",
        &run_att(
            root,
            arguments(&["generic", "write-back", "--name", PROJECT]),
        ),
    );

    let mz_output: Value = serde_json::from_slice(
        &fs::read(mz_workspace.join("write_back/data/Items.json"))
            .expect("MZ WriteBack 必须生成 Items.json"),
    )
    .expect("MZ WriteBack Items.json 必须可重新解析");
    assert_eq!(mz_output[1]["description"], TRANSLATION);
    assert_eq!(
        read_generic_texts(&generic_workspace.join("write_back/story.jsonl")),
        vec![GENERIC_TRANSLATION]
    );

    let mv_output_path = mv_workspace.join("write_back/www/data/Map001.json");
    assert!(mv_output_path.is_file(), "MV WriteBack 必须保留 www 布局");
    let mv_output: Value = serde_json::from_slice(
        &fs::read(&mv_output_path).expect("MV WriteBack Map001.json 必须可读取"),
    )
    .expect("MV WriteBack Map001.json 必须可重新解析");
    assert_eq!(
        mv_output["events"][1]["pages"][0]["list"][1]["parameters"][0],
        format!(r"\n<{MV_SPEAKER_TRANSLATION}>{MV_BODY_WRITE_BACK}")
    );
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
fn rpg_maker_zero_task_translate_uses_confirmed_phase_without_a_fake_percentage() {
    let temporary = tempfile::tempdir().expect("应可建立 RPG Maker 零任务测试目录");
    let root = temporary.path();
    let game = root.join("mz-empty-game");
    write_minimal_mz_game(&game);
    fs::write(game.join("data/Items.json"), b"[null]").expect("零任务 Items.json 应可写入");
    write_rpg_maker_prompt(root);
    write_configuration(root, "http://127.0.0.1:9/v1/chat/completions");

    assert_success(
        "MZ 零任务 Init",
        &run_att(root, init_arguments("mz", &game)),
    );
    assert_success(
        "MZ 零任务 Extract",
        &run_att(
            root,
            arguments(&["mz", "extract", "--name", PROJECT, "--builtin"]),
        ),
    );
    let translate = run_att(
        root,
        arguments(&["mz", "translate", "--name", PROJECT, "local"]),
    );
    assert_success("MZ 零任务 Translate", &translate);

    assert_plain_progress_lines(
        &translate.stderr,
        &[
            "正在规划翻译任务",
            "已确认翻译任务: 无需处理",
            "正在完成必要收尾",
        ],
    );
    let stderr = String::from_utf8_lossy(&translate.stderr);
    assert!(
        !stderr.contains("0/0") && !stderr.contains("100%"),
        "零任务不得显示 0/0 或伪造 100%：{stderr}"
    );
    assert!(
        !stderr.contains("无需调用模型"),
        "无需模型请求属于最终结果，不得作为进度阶段重复呈现：{stderr}"
    );
    let stdout = String::from_utf8(translate.stdout).expect("Translate stdout 必须是 UTF-8");
    let plain_stdout = stdout.replace(['\u{2068}', '\u{2069}'], "");
    assert!(
        plain_stdout.contains("状态：无需处理")
            && plain_stdout.contains("全部翻译单元均为最新状态，本次无需请求模型。"),
        "最终结果应单独说明零任务且没有发送模型请求：{stdout}"
    );
}

#[test]
fn rpg_maker_write_back_materializes_manual_translation_exactly() {
    for (engine, game_directory, source_language) in [
        ("mz", "mz-game", "en"),
        ("mz", "mz-game", "ja"),
        ("mv", "mv-game", "en"),
        ("mv", "mv-game", "ja"),
    ] {
        let temporary = tempfile::tempdir().expect("应可建立 RPG Maker 逐字物化进程测试目录");
        let root = temporary.path();
        let game = root.join(game_directory);
        if engine == "mv" {
            write_minimal_mv_game(&game);
        } else {
            write_minimal_mz_game(&game);
        }

        let data = if engine == "mv" {
            game.join("www/data")
        } else {
            game.join("data")
        };
        fs::write(
            data.join("Items.json"),
            serde_json::to_vec(&json!([
                null,
                {
                    "id": 1,
                    "name": "",
                    "description": "General, Misc, Audio, Toggle"
                }
            ]))
            .expect("Categories Items 夹具应可序列化"),
        )
        .expect("Categories Items 夹具应可写入");
        if engine == "mv" {
            fs::write(
                data.join("Map001.json"),
                serde_json::to_vec(&json!({ "displayName": "", "events": [null] }))
                    .expect("无文本 MV Map 夹具应可序列化"),
            )
            .expect("无文本 MV Map 夹具应可写入");
        }

        write_configuration(root, "http://127.0.0.1:9/v1/chat/completions");
        let distribution = distribution_root(root);

        let mut init = init_arguments(engine, &game);
        let source_language_position = init
            .iter()
            .position(|value| value == "--source-language")
            .expect("Init 参数必须包含源语言");
        init[source_language_position + 1] = source_language.into();
        assert_success(&format!("{engine} 逐字物化 Init"), &run_att(root, init));
        assert_success(
            &format!("{engine} 逐字物化 Extract"),
            &run_att(
                root,
                arguments(&[engine, "extract", "--name", PROJECT, "--builtin"]),
            ),
        );

        let workspace = distribution.join("projects").join(engine).join(PROJECT);
        let script = root.join(format!("set-{engine}-categories.lua"));
        fs::write(
            &script,
            "ctx.translation.set(\"Items.json:1:description\", { \"常规、杂项、声音、开关\" })\n",
        )
        .expect("Categories Lua 脚本应可写入");
        let mut lua_arguments = arguments(&[engine, "lua", "--name", PROJECT]);
        lua_arguments.push(script.into_os_string());
        assert_success(
            &format!("{engine} 逐字物化 Lua"),
            &run_att(root, lua_arguments),
        );

        let logs = workspace.join("logs");
        let logs_before = fs::read_dir(&logs)
            .expect("WriteBack 前项目日志目录应可读取")
            .map(|entry| entry.expect("WriteBack 前项目日志项应可读取").path())
            .collect::<Vec<_>>();
        let write_back = run_att(root, arguments(&[engine, "write-back", "--name", PROJECT]));
        assert_success(&format!("{engine} 逐字物化 WriteBack"), &write_back);
        assert_plain_progress_lines(
            &write_back.stderr,
            &["正在读取已验收资产", "正在规划文档改写", "正在发布输出"],
        );
        let output = if engine == "mv" {
            workspace.join("write_back/www/data/Items.json")
        } else {
            workspace.join("write_back/data/Items.json")
        };
        assert_eq!(
            read_items(&output)[1]["description"],
            "常规、杂项、声音、开关",
            "WriteBack 必须原样采用人工译文"
        );

        let new_logs = fs::read_dir(&logs)
            .expect("WriteBack 后项目日志目录应可读取")
            .map(|entry| entry.expect("WriteBack 后项目日志项应可读取").path())
            .filter(|path| !logs_before.contains(path))
            .collect::<Vec<_>>();
        assert_eq!(new_logs.len(), 1, "一次 WriteBack 只能新增一份项目日志");
        let publication = read_project_log_records(&new_logs[0])
            .into_iter()
            .find(|record| record["event"] == "publication.finished")
            .expect("成功 WriteBack 必须记录 publication.finished");
        assert_eq!(publication["payload"]["result"]["kind"], "published");
        assert_eq!(
            publication["payload"]["result"]["summary"]["engine"],
            "rpg_maker"
        );
        assert_eq!(
            publication["payload"]["result"]["summary"]["summary"],
            serde_json::json!({
                "translated_units": 1,
                "original_units": 0,
            }),
            "项目日志只保存实际物化结果"
        );
    }
}

#[test]
fn mz_switching_llm_client_keeps_and_reuses_the_current_automatic_translation() {
    let temporary = tempfile::tempdir().expect("应可建立 Client 切换进程测试目录");
    let root = temporary.path();
    let game = root.join("mz-game");
    write_minimal_mz_game(&game);
    write_rpg_maker_prompt(root);

    let client_a_listener =
        TcpListener::bind(("127.0.0.1", 0)).expect("Client A 本地模型端口应可绑定");
    let client_a_endpoint = format!(
        "http://{}/v1/chat/completions",
        client_a_listener
            .local_addr()
            .expect("Client A 本地模型地址应可读取")
    );
    write_configuration_for_client(root, &client_a_endpoint, "client_a", "model-a");

    assert_success("MZ Init", &run_att(root, init_arguments("mz", &game)));
    assert_success(
        "MZ Extract",
        &run_att(
            root,
            arguments(&["mz", "extract", "--name", PROJECT, "--builtin"]),
        ),
    );
    let client_a_server = thread::spawn(move || serve_one_translation(client_a_listener));
    assert_success(
        "MZ Client A Translate",
        &run_att(
            root,
            arguments(&["mz", "translate", "--name", PROJECT, "local"]),
        ),
    );
    let client_a_request = client_a_server
        .join()
        .expect("Client A 服务线程不得 panic")
        .expect("Client A 必须收到首次翻译请求");
    assert_eq!(client_a_request["model"], "model-a");

    let workspace = distribution_root(root).join("projects/mz").join(PROJECT);
    let database = workspace.join("project.db");
    assert_eq!(
        read_owner_units(&database, "builtin"),
        vec![(json!(SOURCE_TEXT), Some(json!(TRANSLATION)))],
        "Client A 接受的译文必须成为当前可消费状态"
    );

    let client_b_listener = TcpListener::bind(("127.0.0.1", 0)).expect("Client B 监视端口应可绑定");
    let client_b_endpoint = format!(
        "http://{}/v1/chat/completions",
        client_b_listener
            .local_addr()
            .expect("Client B 本地模型地址应可读取")
    );
    write_configuration_for_client(root, &client_b_endpoint, "client_b", "model-b");
    let (stop_sender, stop_receiver) = mpsc::channel();
    let client_b_spy = thread::spawn(move || serve_provider_spy(client_b_listener, stop_receiver));

    assert_success(
        "MZ Client B WriteBack",
        &run_att(root, arguments(&["mz", "write-back", "--name", PROJECT])),
    );
    assert_eq!(
        read_items(&workspace.join("write_back/data/Items.json"))[1]["description"],
        TRANSLATION,
        "更换 LLM Client 后，WriteBack 必须继续发布已经确认的当前译文"
    );
    assert_success(
        "MZ Client B Translate",
        &run_att(
            root,
            arguments(&["mz", "translate", "--name", PROJECT, "local"]),
        ),
    );
    stop_sender.send(()).expect("应可停止 Client B 监视器");
    let client_b_requests = client_b_spy
        .join()
        .expect("Client B 监视线程不得 panic")
        .expect("Client B 监视器应正常结束");
    assert!(
        client_b_requests.is_empty(),
        "只有 Client/Profile/模型变化时，已完成 Unit 不得重新请求模型：{client_b_requests:?}"
    );
    assert_eq!(
        read_owner_units(&database, "builtin"),
        vec![(json!(SOURCE_TEXT), Some(json!(TRANSLATION)))],
        "Client B 的零工作量 Translate 不得删除或改写旧译文"
    );
}

#[test]
fn mz_language_pair_round_trip_hides_and_restores_the_same_automatic_body() {
    let temporary = tempfile::tempdir().expect("应可建立语言往返进程测试目录");
    let root = temporary.path();
    let game = root.join("mz-game");
    write_minimal_mz_game(&game);
    write_rpg_maker_prompt(root);
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("语言往返本地模型端口应可绑定");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("语言往返本地模型地址应可读取")
    );
    write_configuration(root, &endpoint);

    assert_success("MZ Init", &run_att(root, init_arguments("mz", &game)));
    assert_success(
        "MZ Extract",
        &run_att(
            root,
            arguments(&["mz", "extract", "--name", PROJECT, "--builtin"]),
        ),
    );
    let workspace = distribution_root(root).join("projects/mz").join(PROJECT);
    let database = workspace.join("project.db");
    let server = thread::spawn(move || serve_one_translation(listener));
    assert_success(
        "MZ initial Translate",
        &run_att(
            root,
            arguments(&["mz", "translate", "--name", PROJECT, "local"]),
        ),
    );
    server
        .join()
        .expect("语言往返模型服务线程不得 panic")
        .expect("语言往返初始翻译必须收到模型请求");

    let source_json = serde_json::to_string(SOURCE_TEXT).expect("测试原文应可编码");
    let original_state: Vec<u8> = Connection::open(&database)
        .expect("项目数据库应可打开")
        .query_row(
            "SELECT translation_state FROM rpg_maker_text_unit
             WHERE source_content_json = ?1",
            [&source_json],
            |row| row.get(0),
        )
        .expect("初始自动译文必须保存当前适用性");
    assert_success(
        "MZ target en Init",
        &run_att(
            root,
            arguments(&["mz", "init", "--name", PROJECT, "--target-language", "en"]),
        ),
    );
    let connection = Connection::open(&database).expect("语言变更后的数据库应可打开");
    let state: Vec<u8> = connection
        .query_row(
            "SELECT translation_state FROM rpg_maker_text_unit
             WHERE source_content_json = ?1",
            [&source_json],
            |row| row.get(0),
        )
        .expect("保留正文必须同时保留状态");
    assert_eq!(
        state, original_state,
        "语言事实变化只能改变适用性，不能改写持久状态"
    );
    drop(connection);

    assert_success(
        "MZ target en WriteBack",
        &run_att(root, arguments(&["mz", "write-back", "--name", PROJECT])),
    );
    let output = workspace.join("write_back/data/Items.json");
    assert_eq!(
        read_items(&output)[1]["description"],
        SOURCE_TEXT,
        "语言对变化后旧自动正文必须保留在数据库但不得发布"
    );

    assert_success(
        "MZ target zh-Hans Init",
        &run_att(
            root,
            arguments(&[
                "mz",
                "init",
                "--name",
                PROJECT,
                "--target-language",
                "zh-Hans",
            ]),
        ),
    );
    assert_success(
        "MZ restored WriteBack",
        &run_att(root, arguments(&["mz", "write-back", "--name", PROJECT])),
    );
    assert_eq!(
        read_items(&output)[1]["description"],
        TRANSLATION,
        "语言对恢复后同一正文必须无需重跑模型即可重新成为 Current"
    );
}

#[test]
fn mz_write_back_rejects_manual_translation_invalidated_by_placeholder_changes() {
    let temporary = tempfile::tempdir().expect("应可建立 MZ Placeholder 重新验收测试目录");
    let root = temporary.path();
    let game = root.join("mz-game");
    write_minimal_mz_game(&game);
    fs::write(
        game.join("data/Items.json"),
        serde_json::to_vec(&json!([
            null,
            {
                "id": 1,
                "name": "",
                "description": "General, Misc"
            }
        ]))
        .expect("Placeholder 重新验收 Items 应可序列化"),
    )
    .expect("Placeholder 重新验收 Items 应可写入");
    write_configuration(root, "http://127.0.0.1:9/v1/chat/completions");
    let distribution = distribution_root(root);

    assert_success(
        "MZ Placeholder 重新验收 Init",
        &run_att(root, init_arguments("mz", &game)),
    );
    assert_success(
        "MZ Placeholder 重新验收 Extract",
        &run_att(
            root,
            arguments(&["mz", "extract", "--name", PROJECT, "--builtin"]),
        ),
    );

    let workspace = distribution.join("projects/mz").join(PROJECT);
    let database = workspace.join("project.db");
    let script = root.join("set-placeholder-current.lua");
    fs::write(
        &script,
        "ctx.translation.set(\"Items.json:1:description\", { \"[常规、杂项]\" })\n",
    )
    .expect("新增 Placeholder 的 Current Lua 应可写入");
    let mut lua_arguments = arguments(&["mz", "lua", "--name", PROJECT]);
    lua_arguments.push(script.into_os_string());
    assert_success("MZ 建立规则变化前的 Current", &run_att(root, lua_arguments));

    let placeholder_rules = serde_json::to_string(&json!([{
        "scopes": ["database_entry"],
        "pattern": r"\[[^]]+\]"
    }]))
    .expect("项目 Placeholder 规则应可编码");
    assert_eq!(
        Connection::open(&database)
            .expect("MZ 项目数据库应可重新打开")
            .execute(
                "UPDATE rpg_maker_translation_resource
                 SET canonical_json = ?1
                 WHERE resource_kind = 'placeholder_rules'",
                [&placeholder_rules],
            )
            .expect("应可更新项目 Placeholder 快照"),
        1
    );

    let write_back = run_att(root, arguments(&["mz", "write-back", "--name", PROJECT]));
    assert!(
        !write_back.status.success(),
        "Placeholder 契约变化后，失效的 Manual 译文不得继续作为 Current 写回\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&write_back.stdout),
        String::from_utf8_lossy(&write_back.stderr)
    );
    assert!(
        !workspace.join("write_back/data/Items.json").exists(),
        "候选验收失败不得发布半成品"
    );
}

#[test]
fn mz_partial_retry_reuses_the_complete_task_block_across_real_processes() {
    let temporary = tempfile::tempdir().expect("应可建立 MZ Partial 重试端到端测试目录");
    let root = temporary.path();
    let game = root.join("mz-game");
    write_partial_retry_mz_game(&game);
    write_rpg_maker_prompt(root);

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("本地模型服务端口应可绑定");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("本地模型地址应可读取")
    );
    write_configuration(root, &endpoint);

    assert_success(
        "MZ Partial 重试 Init",
        &run_att(root, init_arguments("mz", &game)),
    );
    assert_success(
        "MZ Partial 重试 Extract",
        &run_att(
            root,
            arguments(&["mz", "extract", "--name", PROJECT, "--builtin"]),
        ),
    );

    let workspace = distribution_root(root).join("projects/mz").join(PROJECT);
    let database = workspace.join("project.db");
    let server = thread::spawn(move || {
        serve_two_responses(
            listener,
            json!({
                "0": [PARTIAL_RETRY_TRANSLATIONS[0]],
                "2": [PARTIAL_RETRY_TRANSLATIONS[2]],
                "3": [PARTIAL_RETRY_TRANSLATIONS[3]]
            }),
            json!({ "0": [PARTIAL_RETRY_TRANSLATIONS[1]] }),
        )
    });

    assert_success(
        "MZ 首次 Partial Translate",
        &run_att(
            root,
            arguments(&["mz", "translate", "--name", PROJECT, "local"]),
        ),
    );
    assert_eq!(
        read_owner_units(&database, "builtin"),
        vec![
            (
                json!(PARTIAL_RETRY_SOURCES[0]),
                Some(json!(PARTIAL_RETRY_TRANSLATIONS[0])),
            ),
            (json!(PARTIAL_RETRY_SOURCES[1]), None),
            (
                json!(PARTIAL_RETRY_SOURCES[2]),
                Some(json!(PARTIAL_RETRY_TRANSLATIONS[2])),
            ),
            (
                json!(PARTIAL_RETRY_SOURCES[3]),
                Some(json!(PARTIAL_RETRY_TRANSLATIONS[3])),
            ),
        ],
        "首次响应必须只提交 A、C、D"
    );
    let first_task_records = read_task_records_sharing_log_run_ids(&workspace);
    assert_eq!(
        first_task_records.len(),
        1,
        "首次 Partial 必须建立一份任务记录"
    );
    assert!(first_task_records[0].1.contains("# 翻译任务"));
    assert!(
        first_task_records[0]
            .1
            .contains("状态：部分完成，已确认提交")
    );
    assert!(first_task_records[0].1.contains("要求译文：4 项"));
    assert!(
        first_task_records[0]
            .1
            .contains("已接受：3 项（ID：0, 2, 3），写入 3 个实际位置")
    );
    assert!(first_task_records[0].1.contains("未接受：1 项（ID：1）"));
    assert!(first_task_records[0].1.contains("任务诊断"));
    assert_eq!(first_task_records[0].1.matches("## Assistant").count(), 1);
    assert!(!first_task_records[0].1.contains("## Thinking"));
    assert!(!first_task_records[0].1.contains("## Raw Assistant"));
    assert!(first_task_records[0].1.contains("\"translations\""));
    let first_run_id = first_task_records[0].0.clone();

    assert_success(
        "MZ 第二次 Partial 重试 Translate",
        &run_att(
            root,
            arguments(&["mz", "translate", "--name", PROJECT, "local"]),
        ),
    );
    let [first_request, second_request] = server
        .join()
        .expect("MZ Partial 重试模型服务线程不得 panic")
        .expect("MZ Partial 重试模型服务必须完成两次请求");

    let first_user = first_request["messages"][1]["content"]
        .as_str()
        .expect("MZ 首次请求 user message 必须是字符串");
    assert_eq!(
        parse_user_message(first_user),
        expected_rpg_maker_description_user_message(&[
            (PARTIAL_RETRY_SOURCES[0], Some(0)),
            (PARTIAL_RETRY_SOURCES[1], Some(1)),
            (PARTIAL_RETRY_SOURCES[2], Some(2)),
            (PARTIAL_RETRY_SOURCES[3], Some(3)),
        ]),
        "首次请求必须按 A、B、C、D 的自然顺序发送完整 TaskBlock"
    );
    let second_user = second_request["messages"][1]["content"]
        .as_str()
        .expect("MZ 第二次请求 user message 必须是字符串");
    assert_eq!(
        parse_user_message(second_user),
        expected_rpg_maker_description_user_message(&[
            (PARTIAL_RETRY_TRANSLATIONS[0], None),
            (PARTIAL_RETRY_SOURCES[1], Some(0)),
            (PARTIAL_RETRY_TRANSLATIONS[2], None),
            (PARTIAL_RETRY_TRANSLATIONS[3], None),
        ]),
        "第二次请求必须保留原 TaskBlock，并只给 B 分配 ID 0"
    );

    let task_records = read_task_records_sharing_log_run_ids(&workspace);
    assert_eq!(task_records.len(), 2, "两次 Translate 必须各有一份任务记录");
    let second_task_records = task_records
        .iter()
        .filter(|(run_id, _)| run_id != &first_run_id)
        .collect::<Vec<_>>();
    assert_eq!(
        second_task_records.len(),
        1,
        "第二次 Translate 必须建立新的 RunId 任务记录"
    );
    let second_task_record = &second_task_records[0].1;
    assert!(
        second_task_record.contains(second_user),
        "第二次 MZ 任务记录必须保存与实际请求相同的完整 TaskBlock"
    );
    assert!(second_task_record.contains("# 翻译任务"));
    assert!(second_task_record.contains("状态：完成，已确认提交"));
    assert_eq!(
        read_owner_units(&database, "builtin"),
        PARTIAL_RETRY_SOURCES
            .iter()
            .zip(PARTIAL_RETRY_TRANSLATIONS)
            .map(|(source, translation)| (json!(*source), Some(json!(translation))))
            .collect::<Vec<_>>(),
        "第二次响应必须补交 B，并保留首次已提交的 A、C、D"
    );
}

#[test]
fn mz_translate_keeps_applied_translation_when_run_plan_transaction_rolls_back() {
    let temporary = tempfile::tempdir().expect("应可建立 RunPlan 回滚进程测试目录");
    let root = temporary.path();
    let game = root.join("mz-game");
    write_minimal_mz_game(&game);
    write_translation_prompt(root);

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("本地模型服务端口应可绑定");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("本地模型地址应可读取")
    );
    write_configuration(root, &endpoint);

    let project = "run-plan-rollback";
    let mut init = init_arguments("mz", &game);
    let name_position = init
        .iter()
        .position(|value| value == "--name")
        .expect("Init 参数必须包含项目名");
    init[name_position + 1] = project.into();
    assert_success("RunPlan 回滚 MZ Init", &run_att(root, init));
    assert_success(
        "RunPlan 回滚 MZ Extract",
        &run_att(
            root,
            arguments(&["mz", "extract", "--name", project, "--builtin"]),
        ),
    );

    let workspace = distribution_root(root).join("projects/mz").join(project);
    let database = workspace.join("project.db");
    let logs = workspace.join("logs");
    let logs_before = fs::read_dir(&logs)
        .expect("Translate 前日志目录应可读取")
        .map(|entry| entry.expect("Translate 前日志项应可读取").path())
        .collect::<Vec<_>>();
    let server_database = database.clone();
    let server = thread::spawn(move || {
        serve_translation_after_installing_run_plan_rollback(listener, &server_database)
    });

    let translate = run_att(
        root,
        arguments(&["mz", "translate", "--name", project, "local"]),
    );
    server
        .join()
        .expect("RunPlan 回滚服务线程不得 panic")
        .expect("RunPlan 回滚服务必须完成请求并安装触发器");
    assert_eq!(
        translate.status.code(),
        Some(1),
        "RunPlan 未保存必须保留独立失败退出语义\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&translate.stdout),
        String::from_utf8_lossy(&translate.stderr)
    );

    let connection = Connection::open(&database).expect("RunPlan 回滚后数据库应可打开");
    let translation: Option<String> = connection
        .query_row(
            "SELECT translation_content_json
             FROM rpg_maker_text_unit
             WHERE translation_content_json IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("业务已提交的译文应可读取");
    assert_eq!(
        translation
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()
            .expect("已提交译文必须是规范 JSON"),
        Some(json!(TRANSLATION)),
        "RunPlan 回滚不得回滚已经确认的翻译业务结果"
    );
    let saved_profile_count: i64 = connection
        .query_row("SELECT count(*) FROM translate_run_plan", [], |row| {
            row.get(0)
        })
        .expect("Translate RunPlan 行数应可读取");
    assert_eq!(saved_profile_count, 0, "回滚后不得保存本次 Profile");
    connection
        .execute_batch("DROP TRIGGER att_e2e_reject_translate_run_plan")
        .expect("测试触发器应可清理");
    drop(connection);

    let new_logs = fs::read_dir(&logs)
        .expect("Translate 后日志目录应可读取")
        .map(|entry| entry.expect("Translate 后日志项应可读取").path())
        .filter(|path| !logs_before.contains(path))
        .collect::<Vec<_>>();
    assert_eq!(new_logs.len(), 1, "一次 Translate 只能新增一份项目日志");
    let records = read_project_log_records(&new_logs[0]);
    let translation_finished = records
        .iter()
        .position(|record| {
            record["event"] == "translation.finished"
                && record["payload"]["result"]["kind"] == "complete"
        })
        .expect("RunPlan 保存前必须先记录完整翻译业务终态");
    let run_plan_diagnostic = records
        .iter()
        .position(|record| record["event"] == "diagnostic.run_plan")
        .expect("RunPlan 回滚必须形成独立诊断");
    assert!(
        translation_finished < run_plan_diagnostic,
        "翻译业务终态必须先于 RunPlan 持久化失败"
    );
    let diagnostic = records[run_plan_diagnostic]["payload"]
        .as_object()
        .expect("RunPlan 诊断 payload 必须是对象");
    assert_eq!(diagnostic.len(), 5);
    assert_eq!(diagnostic["relation"], "primary");
    for field in ["object", "reason", "impact", "help"] {
        assert!(
            diagnostic[field]
                .as_str()
                .is_some_and(|value| !value.is_empty()),
            "RunPlan 诊断 {field} 必须是非空可读文本"
        );
    }
    let finalized = records
        .iter()
        .find(|record| record["event"] == "run_plan.finalized")
        .expect("RunPlan 回滚必须写入最终化事件");
    assert_eq!(finalized["payload"]["result"]["kind"], "not_saved");
    assert_eq!(finalized["payload"]["result"]["transaction"], "rolled_back");
    assert_eq!(finalized["payload"]["result"]["run_continues"], false);
    assert!(finalized["payload"]["result"].get("diagnostic").is_none());
    assert!(
        records.iter().all(|record| {
            !(record["event"] == "run_plan.finalized"
                && record["payload"]["result"]["kind"] == "saved")
        }),
        "回滚路径不得同时伪报 RunPlan 已保存"
    );
    let terminal = records.last().expect("运行日志必须有唯一终态");
    assert_eq!(terminal["event"], "run.finished");
    assert_eq!(terminal["payload"]["result"]["kind"], "failed");
}

#[test]
fn generic_partial_retry_reuses_the_complete_task_block_across_real_processes() {
    let temporary = tempfile::tempdir().expect("应可建立 Generic Partial 重试端到端测试目录");
    let root = temporary.path();
    let input = root.join("jsonl");
    fs::create_dir(&input).expect("Generic Partial 重试输入目录应可建立");
    write_partial_retry_generic_group(&input.join("story.jsonl"));
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
    assert_success("Generic Partial 重试 Init", &run_att(root, init));
    assert_success(
        "Generic Partial 重试 Extract",
        &run_att(root, arguments(&["generic", "extract", "--name", PROJECT])),
    );

    let workspace = distribution_root(root)
        .join("projects/generic")
        .join(PROJECT);
    let database = workspace.join("project.db");
    let server = thread::spawn(move || {
        serve_two_responses(
            listener,
            json!({
                "0": [PARTIAL_RETRY_TRANSLATIONS[0]],
                "2": [PARTIAL_RETRY_TRANSLATIONS[2]],
                "3": [PARTIAL_RETRY_TRANSLATIONS[3]]
            }),
            json!({ "0": [PARTIAL_RETRY_TRANSLATIONS[1]] }),
        )
    });

    assert_success(
        "Generic 首次 Partial Translate",
        &run_att(
            root,
            arguments(&["generic", "translate", "--name", PROJECT, "local"]),
        ),
    );
    assert_eq!(
        read_generic_units(&database),
        PARTIAL_RETRY_SOURCES
            .iter()
            .zip([
                Some(PARTIAL_RETRY_TRANSLATIONS[0]),
                None,
                Some(PARTIAL_RETRY_TRANSLATIONS[2]),
                Some(PARTIAL_RETRY_TRANSLATIONS[3]),
            ])
            .map(|(source, translation)| { ((*source).to_owned(), translation.map(str::to_owned)) })
            .collect::<Vec<_>>(),
        "首次 Generic 响应必须只提交 A、C、D"
    );
    let first_task_records = read_task_records_sharing_log_run_ids(&workspace);
    assert_eq!(
        first_task_records.len(),
        1,
        "首次 Generic Partial 必须建立一份任务记录"
    );
    assert!(first_task_records[0].1.contains("# 翻译任务"));
    assert!(
        first_task_records[0]
            .1
            .contains("状态：部分完成，已确认提交")
    );
    assert_eq!(first_task_records[0].1.matches("## Assistant").count(), 1);
    assert!(!first_task_records[0].1.contains("## Thinking"));
    assert!(!first_task_records[0].1.contains("## Raw Assistant"));
    assert!(first_task_records[0].1.contains("\"translations\""));
    let first_run_id = first_task_records[0].0.clone();

    assert_success(
        "Generic 第二次 Partial 重试 Translate",
        &run_att(
            root,
            arguments(&["generic", "translate", "--name", PROJECT, "local"]),
        ),
    );
    let [first_request, second_request] = server
        .join()
        .expect("Generic Partial 重试模型服务线程不得 panic")
        .expect("Generic Partial 重试模型服务必须完成两次请求");

    let first_user = first_request["messages"][1]["content"]
        .as_str()
        .expect("Generic 首次请求 user message 必须是字符串");
    assert_pretty_translation_user_message(first_user);
    assert!(
        first_task_records[0].1.contains(first_user),
        "任务记录必须保留实际发送给模型的格式化 user message"
    );
    assert_eq!(
        parse_user_message(first_user),
        expected_generic_user_message(&[
            (PARTIAL_RETRY_SOURCES[0], Some(0)),
            (PARTIAL_RETRY_SOURCES[1], Some(1)),
            (PARTIAL_RETRY_SOURCES[2], Some(2)),
            (PARTIAL_RETRY_SOURCES[3], Some(3)),
        ]),
        "首次 Generic 请求必须按 A、B、C、D 的自然顺序发送完整 TaskBlock"
    );
    let second_user = second_request["messages"][1]["content"]
        .as_str()
        .expect("Generic 第二次请求 user message 必须是字符串");
    assert_eq!(
        parse_user_message(second_user),
        expected_generic_user_message(&[
            (PARTIAL_RETRY_TRANSLATIONS[0], None),
            (PARTIAL_RETRY_SOURCES[1], Some(0)),
            (PARTIAL_RETRY_TRANSLATIONS[2], None),
            (PARTIAL_RETRY_TRANSLATIONS[3], None),
        ]),
        "第二次 Generic 请求必须保留原 TaskBlock，并只给 B 分配 ID 0"
    );

    let task_records = read_task_records_sharing_log_run_ids(&workspace);
    assert_eq!(task_records.len(), 2, "两次 Translate 必须各有一份任务记录");
    let second_task_records = task_records
        .iter()
        .filter(|(run_id, _)| run_id != &first_run_id)
        .collect::<Vec<_>>();
    assert_eq!(
        second_task_records.len(),
        1,
        "第二次 Generic Translate 必须建立新的 RunId 任务记录"
    );
    let second_task_record = &second_task_records[0].1;
    assert!(
        second_task_record.contains(second_user),
        "第二次 Generic 任务记录必须保存与实际请求相同的完整 TaskBlock"
    );
    assert!(second_task_record.contains("# 翻译任务"));
    assert!(second_task_record.contains("状态：完成，已确认提交"));
    assert_eq!(
        read_generic_units(&database),
        PARTIAL_RETRY_SOURCES
            .iter()
            .zip(PARTIAL_RETRY_TRANSLATIONS)
            .map(|(source, translation)| ((*source).to_owned(), Some(translation.to_owned())))
            .collect::<Vec<_>>(),
        "第二次 Generic 响应必须补交 B，并保留首次已提交的 A、C、D"
    );
}

#[test]
fn generic_source_placeholder_failure_sends_no_incomplete_task_block() {
    let temporary = tempfile::tempdir().expect("应可建立 Generic Placeholder 失败测试目录");
    let root = temporary.path();
    let input = root.join("jsonl");
    fs::create_dir(&input).expect("Generic Placeholder 失败输入目录应可建立");
    fs::write(
        input.join("story.jsonl"),
        concat!(
            r#"{"id":"scene","kind":"dialogue","units":["#,
            r#"{"id":"good-a","text":"春の便りです"},"#,
            r#"{"id":"broken","text":"夏の便りです {hero}"},"#,
            r#"{"id":"good-c","text":"秋の便りです"}]}"#,
            "\n"
        ),
    )
    .expect("Generic Placeholder 失败 JSONL 应可写入");
    let placeholders = root.join("overlapping-placeholders.toml");
    fs::write(
        &placeholders,
        concat!(
            "[[rule]]\n",
            "scopes = [\"dialogue\"]\n",
            "order = \"preserve\"\n",
            "pattern = '\\{[^}]+\\}'\n",
            "\n",
            "[[rule]]\n",
            "scopes = [\"dialogue\"]\n",
            "order = \"preserve\"\n",
            "pattern = '\\{hero\\}'\n",
        ),
    )
    .expect("重叠 Placeholder 规则应可写入");
    write_generic_prompt(root);

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("本地模型服务端口应可绑定");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("本地模型地址应可读取")
    );
    write_configuration(root, &endpoint);
    let (stop_sender, stop_receiver) = mpsc::channel();
    let provider = thread::spawn(move || serve_provider_spy(listener, stop_receiver));

    let mut init = arguments(&["generic", "init", "--name", PROJECT, "--path"]);
    init.push(input.as_os_str().to_owned());
    init.extend(arguments(&[
        "--source-language",
        "ja",
        "--target-language",
        "zh-Hans",
    ]));
    assert_success("Generic Placeholder 失败 Init", &run_att(root, init));
    assert_success(
        "Generic Placeholder 失败 Extract",
        &run_att(root, arguments(&["generic", "extract", "--name", PROJECT])),
    );

    let mut translate = arguments(&[
        "generic",
        "translate",
        "--name",
        PROJECT,
        "local",
        "--placeholders",
    ]);
    translate.push(placeholders.as_os_str().to_owned());
    let output = run_att(root, translate);
    stop_sender
        .send(())
        .expect("Generic Placeholder 失败后应可停止 Provider spy");
    let requests = provider
        .join()
        .expect("Generic Placeholder Provider spy 不得 panic")
        .expect("Generic Placeholder Provider spy 必须正常结束");

    assert_eq!(output.status.code(), Some(1));
    assert!(
        requests.is_empty(),
        "同一 TaskBlock 中任一源文无法完成 Placeholder 投影时，不得删除坏 Unit 后发送残缺块：{requests:?}"
    );
    let workspace = distribution_root(root)
        .join("projects/generic")
        .join(PROJECT);
    assert!(
        !workspace.join("task-records").exists(),
        "模型请求开始前的源文 Placeholder 失败不得建立任务记录"
    );
    assert!(
        read_generic_units(&workspace.join("project.db"))
            .iter()
            .all(|(_, translation)| translation.is_none()),
        "规划失败不得提交同块其他 Unit 的译文"
    );
    let stderr = String::from_utf8(output.stderr).expect("Generic 失败诊断必须是 UTF-8");
    assert!(
        stderr.contains("story.jsonl:line1:unit2:text"),
        "规划失败必须指出实际失败 Unit：{stderr}"
    );
    assert!(stderr.contains("原因：") && stderr.contains("处理办法："));
    for internal in [
        "translation.placeholder",
        "relative_path=",
        "group_id=",
        "unit_id=",
        "first_rule_number=",
        "second_rule_number=",
        "first_range=",
        "second_range=",
    ] {
        assert!(
            !stderr.contains(internal),
            "公开诊断不得显示内部字段 {internal:?}：{stderr}"
        );
    }
}

#[test]
fn mv_source_placeholder_failure_fails_before_database_and_model_side_effects() {
    let temporary = tempfile::tempdir().expect("应可建立 MV Placeholder 失败测试目录");
    let root = temporary.path();
    let game = root.join("mv-placeholder-failure-game");
    write_minimal_mv_game(&game);
    fs::write(
        game.join("www/data/Items.json"),
        serde_json::to_vec(&json!([
            null,
            {"id": 1, "name": "春の薬", "description": "春の便りです"},
            {"id": 2, "name": "夏の薬", "description": "夏の便りです \\n[123]"},
            {"id": 3, "name": "秋の薬", "description": "秋の便りです"}
        ]))
        .expect("MV Placeholder 失败 Items 应可序列化"),
    )
    .expect("MV Placeholder 失败 Items 应可写入");
    let placeholders = root.join("overlapping-mv-placeholders.toml");
    fs::write(
        &placeholders,
        concat!(
            "[[rule]]\n",
            "scopes = [\"database_entry\"]\n",
            "order = \"preserve\"\n",
            "pattern = '\\\\n\\[[0-9]+\\]'\n",
        ),
    )
    .expect("MV 重叠 Placeholder 规则应可写入");

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("本地模型服务端口应可绑定");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("本地模型地址应可读取")
    );
    write_configuration(root, &endpoint);
    write_rpg_maker_prompt(root);
    let (stop_sender, stop_receiver) = mpsc::channel();
    let provider = thread::spawn(move || serve_provider_spy(listener, stop_receiver));

    assert_success(
        "MV Placeholder 失败 Init",
        &run_att(root, init_arguments("mv", &game)),
    );
    assert_success(
        "MV Placeholder 失败 Extract",
        &run_att(
            root,
            arguments(&["mv", "extract", "--name", PROJECT, "--builtin"]),
        ),
    );

    let workspace = distribution_root(root).join("projects/mv").join(PROJECT);
    let database = workspace.join("project.db");
    let database_before = read_sqlite_logical_snapshot(&database);
    let logs = workspace.join("logs");
    let logs_before = fs::read_dir(&logs)
        .expect("Translate 前日志目录应可读取")
        .map(|entry| entry.expect("Translate 前日志项应可读取").path())
        .collect::<Vec<_>>();

    let mut translate = arguments(&[
        "mv",
        "translate",
        "--name",
        PROJECT,
        "local",
        "--placeholders",
    ]);
    translate.push(placeholders.as_os_str().to_owned());
    let output = run_att(root, translate);
    stop_sender
        .send(())
        .expect("MV Placeholder 失败后应可停止 Provider spy");
    let requests = provider
        .join()
        .expect("MV Placeholder Provider spy 不得 panic")
        .expect("MV Placeholder Provider spy 必须正常结束");

    assert_eq!(output.status.code(), Some(1));
    assert!(
        requests.is_empty(),
        "任一 RPG Maker Unit 准备失败时不得发送其他完整块：{requests:?}"
    );
    assert_eq!(
        read_sqlite_logical_snapshot(&database),
        database_before,
        "规划失败不得改变任何 SQLite 逻辑业务状态"
    );
    assert!(
        !workspace.join("task-records").exists(),
        "零模型请求的 RPG Maker 规划失败不得建立任务记录"
    );

    let new_logs = fs::read_dir(&logs)
        .expect("Translate 后日志目录应可读取")
        .map(|entry| entry.expect("Translate 后日志项应可读取").path())
        .filter(|path| !logs_before.contains(path))
        .collect::<Vec<_>>();
    assert_eq!(new_logs.len(), 1, "一次 Translate 只能新增一份项目日志");
    let records = read_project_log_records(&new_logs[0]);
    let diagnostics = records
        .iter()
        .filter(|record| record["event"] == "diagnostic.run_plan")
        .collect::<Vec<_>>();
    assert_eq!(diagnostics.len(), 1, "规划失败必须产生唯一 RunPlan 诊断");
    let payload = diagnostics[0]["payload"]
        .as_object()
        .expect("RunPlan 诊断 payload 必须是对象");
    assert_eq!(payload.len(), 5);
    assert_eq!(payload["relation"], "primary");
    for field in ["object", "reason", "impact", "help"] {
        assert!(
            payload[field]
                .as_str()
                .is_some_and(|value| !value.is_empty()),
            "公开诊断 {field} 必须是非空可读文本"
        );
    }
    let object = payload["object"]
        .as_str()
        .expect("规划诊断 object 必须是可读文本");
    for expected in [
        "Items.json",
        "role=scalar:description",
        "overlapping-mv-placeholders.toml",
        "builtin",
        "custom rule 1",
    ] {
        assert!(
            object.contains(expected),
            "规划诊断 object 缺少 {expected:?}：{object}"
        );
    }
    for forbidden in ["group_id", "unit_id", "first_range", "second_range", "byte"] {
        assert!(
            !object.contains(forbidden),
            "规划诊断 object 不得显示内部字段 {forbidden:?}：{object}"
        );
    }
    assert!(
        payload["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("重叠")),
        "规划诊断 reason 必须说明规则重叠：{payload:?}"
    );
    assert!(
        records
            .iter()
            .all(|record| record["event"] != "task.started")
    );

    let translation_finished = records
        .iter()
        .filter(|record| record["event"] == "translation.finished")
        .collect::<Vec<_>>();
    assert_eq!(translation_finished.len(), 1);
    let result = &translation_finished[0]["payload"]["result"];
    assert_eq!(result["kind"], "failed");
    let tasks = &result["tasks"];
    assert_eq!(tasks["started"], 0);
    for field in ["complete", "partial", "unavailable", "failed", "cancelled"] {
        assert_eq!(tasks[field], 0, "规划失败前不得开始任何任务");
    }
    assert_eq!(tasks["planned"], tasks["not_started"]);
}

#[test]
fn task_record_write_failure_warns_once_without_changing_translate_success() {
    let temporary = tempfile::tempdir().expect("应可建立任务记录降级测试目录");
    let root = temporary.path();
    let game = root.join("mz-game");
    write_minimal_mz_game(&game);
    write_rpg_maker_prompt(root);

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("本地模型服务端口应可绑定");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("本地模型地址应可读取")
    );
    write_configuration(root, &endpoint);
    assert_success(
        "任务记录降级 Init",
        &run_att(root, init_arguments("mz", &game)),
    );
    assert_success(
        "任务记录降级 Extract",
        &run_att(
            root,
            arguments(&["mz", "extract", "--name", PROJECT, "--builtin"]),
        ),
    );

    let workspace = distribution_root(root).join("projects/mz").join(PROJECT);
    fs::write(workspace.join("task-records"), b"not-a-directory")
        .expect("普通文件应可稳定触发任务记录写入失败");
    let server = thread::spawn(move || serve_one_translation(listener));
    let translate = run_att(
        root,
        arguments(&["mz", "translate", "--name", PROJECT, "local"]),
    );
    assert_success("任务记录降级 Translate", &translate);
    server
        .join()
        .expect("本地模型服务线程不得 panic")
        .expect("本地模型服务必须完成一次请求");
    assert_eq!(
        read_owner_units(&workspace.join("project.db"), "builtin"),
        vec![(json!(SOURCE_TEXT), Some(json!(TRANSLATION)))],
        "任务记录写入失败不得回滚已确认译文"
    );
    let stderr = String::from_utf8(translate.stderr).expect("stderr 必须是 UTF-8");
    for expected in [
        "警告：",
        "task-records",
        "原因：",
        "所需对象不存在",
        "影响：",
        "业务状态没有修改",
        "处理办法：",
        "检查路径、文件系统状态和权限",
    ] {
        assert!(
            stderr.contains(expected),
            "任务记录四字段警告缺少 {expected:?}：{stderr}"
        );
    }
    assert_eq!(
        stderr.matches("task-records").count(),
        1,
        "任务记录故障必须恰好呈现一次自然路径：{stderr}"
    );
    assert!(
        !stderr.contains("翻译任务记录不可用或已降级"),
        "任务记录故障不得保留旧的一行模糊文案：{stderr}"
    );
    let task_record_failures = fs::read_dir(workspace.join("logs"))
        .expect("项目日志目录应存在")
        .collect::<Result<Vec<_>, _>>()
        .expect("项目日志目录应可读取")
        .into_iter()
        .flat_map(|entry| {
            fs::read_to_string(entry.path())
                .expect("项目日志应可读取")
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).expect("日志行应为 JSON"))
                .collect::<Vec<_>>()
        })
        .filter(|record| record["event"] == "diagnostic.task_record")
        .collect::<Vec<_>>();
    assert!(
        !task_record_failures.is_empty(),
        "任务记录故障必须写入 Translate 的同 RunId JSONL"
    );
    assert!(task_record_failures.iter().all(|record| {
        let Some(payload) = record["payload"].as_object() else {
            return false;
        };
        record["context"]["command"] == "translate"
            && record["level"] == "warn"
            && payload.len() == 5
            && payload["relation"] == "primary"
            && payload["object"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
            && payload["reason"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
            && payload["impact"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
            && payload["help"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
            && payload.get("report").is_none()
    }));
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

    let workspace = distribution_root(root).join("projects/mv").join(PROJECT);
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
                "0": [MV_SPEAKER_TRANSLATION],
                "1": [MV_BODY_TRANSLATION]
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
    let user = parse_user_message(
        request["messages"][1]["content"]
            .as_str()
            .expect("MV 对话 user message 必须是字符串"),
    );
    assert_eq!(
        user_message_texts(&user),
        [MV_SPEAKER, MV_BODY],
        "MV 对话必须在同一 JSON 消息中按 Speaker、Body 的自然顺序请求"
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
        format!(r"\n<{MV_SPEAKER_TRANSLATION}>{MV_BODY_WRITE_BACK}")
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
fn rules_failure_after_builtin_commit_keeps_known_failed_terminal_state() {
    let temporary = tempfile::tempdir().expect("应可建立 Rules 失败回归测试目录");
    let root = temporary.path();
    let game = root.join("mz-game");
    write_minimal_mz_game(&game);
    let items_path = game.join("data/Items.json");
    let mut items = read_items(&items_path);
    items[1]["customShortName"] = json!(7);
    fs::write(
        &items_path,
        serde_json::to_vec(&items).expect("无效 Rules 目标夹具应可序列化"),
    )
    .expect("无效 Rules 目标夹具应可写入");
    write_extract_rules(root, Some("customShortName"));
    write_configuration(root, "http://127.0.0.1:9/v1/chat/completions");

    assert_success(
        "Rules 失败回归项目 Init",
        &run_att(root, init_arguments("mz", &game)),
    );
    let workspace = distribution_root(root).join("projects/mz").join(PROJECT);
    let logs = workspace.join("logs");
    let logs_before = fs::read_dir(&logs)
        .expect("失败 Extract 前日志目录应可读取")
        .map(|entry| entry.expect("失败 Extract 前日志项应可读取").path())
        .collect::<Vec<_>>();

    let extract = run_att(
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
    );

    assert_eq!(extract.status.code(), Some(1));
    assert_eq!(
        read_owner_units(&workspace.join("project.db"), "builtin"),
        vec![(json!(SOURCE_TEXT), None)],
        "后续 Rules 失败必须保留已提交的 Builtin 结果"
    );
    let stderr = String::from_utf8(extract.stderr).expect("失败诊断必须是 UTF-8");
    assert!(!stderr.is_empty(), "Rules 失败必须通过 stderr 呈现终端诊断");

    let new_logs = fs::read_dir(&logs)
        .expect("失败 Extract 后日志目录应可读取")
        .map(|entry| entry.expect("失败 Extract 后日志项应可读取").path())
        .filter(|path| !logs_before.contains(path))
        .collect::<Vec<_>>();
    assert_eq!(new_logs.len(), 1, "一次失败 Extract 只能新增一份项目日志");
    let records = read_project_log_records(&new_logs[0]);
    assert_eq!(
        records
            .iter()
            .filter(|record| record["event"] == "diagnostic.run")
            .count(),
        1,
        "Rules 主错误不得被日志合同错误重复或替换"
    );
    let diagnostic = records
        .iter()
        .find(|record| record["event"] == "diagnostic.run")
        .expect("Rules 失败必须有结构化主诊断");
    assert_eq!(diagnostic["payload"]["relation"], "primary");
    assert!(
        diagnostic["payload"]["object"]
            .as_str()
            .is_some_and(|object| object.contains("rules.toml")),
        "Rules 失败诊断必须指向实际失败规则输入"
    );
    for field in ["reason", "impact", "help"] {
        assert!(
            diagnostic["payload"][field]
                .as_str()
                .is_some_and(|value| !value.trim().is_empty()),
            "Rules 失败诊断的 {field} 必须是非空可读事实"
        );
    }
    assert!(
        records
            .iter()
            .all(|record| record["event"] != "run_plan.finalized"),
        "业务失败前未执行运行方案保存，不得伪造最终化事件"
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record["event"] == "run.finished")
            .count(),
        1,
        "失败运行必须只有一个终态"
    );
    let terminal = records.last().expect("失败运行必须有终态");
    assert_eq!(terminal["event"], "run.finished");
    assert_eq!(terminal["payload"]["result"]["kind"], "failed");
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

    let workspace = distribution_root(root).join("projects/mz").join(PROJECT);
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
                "0": [TRANSLATION],
                "1": [RULES_SHORT_TRANSLATION]
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
    let user = parse_user_message(
        request["messages"][1]["content"]
            .as_str()
            .expect("Rules user message 必须是字符串"),
    );
    let user_texts = user_message_texts(&user);
    assert!(
        user_texts.contains(&SOURCE_TEXT) && user_texts.contains(&RULES_SHORT_SOURCE),
        "同一翻译运行必须把 Builtin 与 Rules owner 写入 JSON user message"
    );
    assert_eq!(
        read_owner_units(&database, "builtin"),
        vec![(json!(SOURCE_TEXT), Some(json!(TRANSLATION)))],
        "模型接受的 Builtin 译文必须先以可消费状态提交"
    );
    assert_eq!(
        read_owner_units(&database, "rules"),
        vec![(
            json!(RULES_SHORT_SOURCE),
            Some(json!(RULES_SHORT_TRANSLATION))
        )],
        "模型接受的 Rules 译文必须先以可消费状态提交"
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
    assert_eq!(
        output_items[1]["description"], SOURCE_TEXT,
        "同一逻辑 Group 的 Rules 兄弟来源变化后，旧 Builtin 正文必须保留但不得继续发布"
    );
    assert_eq!(output_items[1]["customShortName"], RULES_SHORT_SOURCE);
    assert_eq!(output_items[1]["customLongName"], RULES_LONG_SOURCE);

    write_extract_rules(root, None);
    let logs = workspace.join("logs");
    let logs_before = fs::read_dir(&logs)
        .expect("Rules 停用前日志目录应可读取")
        .map(|entry| entry.expect("Rules 停用前日志项应可读取").path())
        .collect::<Vec<_>>();
    let disabled = run_att(
        root,
        arguments(&["mz", "extract", "--name", PROJECT, "--rules", "rules.toml"]),
    );
    assert_success("Rules 显式停用", &disabled);
    let disabled_stdout = String::from_utf8_lossy(&disabled.stdout);
    let disabled_stderr =
        String::from_utf8_lossy(&disabled.stderr).replace(['\u{2068}', '\u{2069}'], "");
    assert!(
        !disabled_stdout.contains("已停用 owner")
            && !disabled_stdout.contains("没有可执行的 Extract owner"),
        "显式空 Rules 不得保留旧的一行模糊提示：{disabled_stdout}"
    );
    for expected in [
        "警告：",
        "对象：",
        "rules.toml",
        "原因：",
        "rule = []",
        "影响：业务结果已经生效，但本次运行方案没有保存",
        "处理办法：如果这是预期结果，无需处理；否则在指出的文件中添加有效规则并重新运行 Extract",
    ] {
        assert!(
            disabled_stderr.contains(expected),
            "显式空 Rules 的四字段警告缺少 {expected:?}：{disabled_stderr}"
        );
    }
    let new_logs = fs::read_dir(&logs)
        .expect("Rules 停用后日志目录应可读取")
        .map(|entry| entry.expect("Rules 停用后日志项应可读取").path())
        .filter(|path| !logs_before.contains(path))
        .collect::<Vec<_>>();
    assert_eq!(new_logs.len(), 1, "Rules 停用只能新增一份项目日志");
    let records = read_project_log_records(&new_logs[0]);
    let diagnostics = records
        .iter()
        .filter(|record| record["event"] == "diagnostic.extract")
        .collect::<Vec<_>>();
    assert_eq!(diagnostics.len(), 1, "Rules 停用必须写一条 Extract 诊断");
    let payload = diagnostics[0]["payload"]
        .as_object()
        .expect("Rules 停用诊断 payload 必须是对象");
    assert_eq!(
        payload
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>(),
        ["help", "impact", "object", "reason", "relation"]
            .into_iter()
            .collect(),
        "Rules 停用诊断只能公开五个可读字段"
    );
    assert_eq!(payload["relation"], "primary");
    assert!(
        payload["object"]
            .as_str()
            .is_some_and(|value| value.contains("rules.toml"))
    );
    assert!(
        payload["reason"]
            .as_str()
            .is_some_and(|value| value.contains("rule = []"))
    );
    assert_eq!(
        payload["impact"],
        "业务结果已经生效，但本次运行方案没有保存"
    );
    assert_eq!(
        payload["help"],
        "如果这是预期结果，无需处理；否则在指出的文件中添加有效规则并重新运行 Extract"
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
    assert_eq!(
        output_items[1]["description"], SOURCE_TEXT,
        "移除同一逻辑 Group 的 Rules Unit 仍会改变完整来源语境，旧正文不得被误当成 Current"
    );
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
    let user = parse_user_message(
        request["messages"][1]["content"]
            .as_str()
            .expect("Generic user message 必须是字符串"),
    );
    assert!(
        user_message_texts(&user).contains(&"こんにちは"),
        "Generic user message 必须在 JSON text 数组中包含待译 Group"
    );
    let workspace = distribution_root(root)
        .join("projects/generic")
        .join(PROJECT);
    let task_record = read_single_task_record_sharing_log_run_id(&workspace);
    assert!(task_record.contains("# 翻译任务"));
    assert!(
        task_record.contains("状态：完成，已确认提交"),
        "实际任务记录：\n{task_record}"
    );
    assert!(task_record.contains("要求译文：1 项"));
    assert!(task_record.contains("已接受：1 项（ID：0），写入 2 个实际位置"));
    assert!(task_record.contains("未接受：0 项（ID：—）"));
    assert!(task_record.contains(THINKING_SENTINEL));
    assert_eq!(task_record.matches("## Assistant").count(), 1);
    assert!(task_record.contains("## Assistant\n\n```json\n"));
    assert!(
        task_record.find("## 最终结果").expect("应包含最终结果")
            < task_record.find("## User").expect("应包含 User")
    );
    assert!(!task_record.contains("## Thinking"));
    assert!(!task_record.contains("## Raw Assistant"));
    assert_success(
        "Generic WriteBack",
        &run_att(
            root,
            arguments(&["generic", "write-back", "--name", PROJECT]),
        ),
    );

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

    let override_script = root.join("generic-override.lua");
    fs::write(
        &override_script,
        r#"assert(ctx.project.engine == "generic")
ctx.translation.set(
  assert(arg[1]),
  { assert(arg[2]), assert(arg[3]) }
)
"#,
    )
    .expect("产品 E2E 的 Lua 输入应可写入");
    let mut lua_arguments = arguments(&["generic", "lua", "--name", PROJECT]);
    lua_arguments.push(override_script.into_os_string());
    lua_arguments.push("--".into());
    lua_arguments.extend(arguments(&["story.jsonl:line1:unit1:text", "您好", "世界"]));
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
        vec![GENERIC_SOURCE.to_owned(), GENERIC_TRANSLATION.to_owned()],
        "所属文件变化后人工译文应保留为过期快照，WriteBack 不再使用"
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
        vec![GENERIC_SOURCE.to_owned(), GENERIC_TRANSLATION.to_owned()],
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
    let distribution = distribution_root(root);
    fs::create_dir_all(&distribution).expect("测试发行目录应可建立");
    fs::write(
        distribution.join("config.toml"),
        format!(
            r#"[llm.clients.invalid]
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
    for expected in [
        "compile_script",
        "Lua 主程序编译失败",
        "修正指出的输入后重试",
    ] {
        assert!(
            stderr.contains(expected),
            "Lua 语法诊断缺少 {expected:?}：{stderr}"
        );
    }
    for internal in ["lua.compilation", "near '='"] {
        assert!(
            !stderr.contains(internal),
            "Lua 语法诊断不得显示内部信息 {internal:?}：{stderr}"
        );
    }

    let logs = distribution_root(root)
        .join("projects/generic")
        .join(PROJECT)
        .join("logs");
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
        records
            .iter()
            .all(|record| { record["event"] != "lua.script" && record["event"] != "lua.summary" }),
        "Lua 日志不得保存脚本哈希或无实际作用的摘要"
    );
    let failure = records
        .iter()
        .find(|record| record["event"] == "diagnostic.run")
        .expect("语法失败日志必须保存主错误");
    let failure_payload = failure["payload"]
        .as_object()
        .expect("Lua 失败诊断 payload 必须是对象");
    assert_eq!(failure_payload.len(), 5);
    assert_eq!(failure_payload["relation"], "primary");
    for field in ["object", "reason", "impact", "help"] {
        assert!(
            failure_payload[field]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
    }
    assert!(
        records.iter().any(|record| {
            record["event"] == "run.finished" && record["payload"]["result"]["kind"] == "failed"
        }),
        "语法失败日志必须写入明确终态"
    );

    let logs_before_project_open = fs::read_dir(&logs)
        .expect("项目打开失败前日志目录应可读取")
        .map(|entry| entry.expect("项目打开失败前日志项应可读取").path())
        .collect::<Vec<_>>();
    let valid_script = root.join("valid-generic.lua");
    fs::write(&valid_script, "return\n").expect("合法 Generic Lua 脚本应可写入");
    let mut valid_arguments = arguments(&["generic", "lua", "--name", PROJECT]);
    valid_arguments.push(valid_script.into_os_string());
    let project_open_failure = run_att(root, valid_arguments);
    assert_eq!(project_open_failure.status.code(), Some(1));
    let project_open_logs = fs::read_dir(&logs)
        .expect("项目打开失败后日志目录应可读取")
        .map(|entry| entry.expect("项目打开失败后日志项应可读取").path())
        .filter(|path| !logs_before_project_open.contains(path))
        .collect::<Vec<_>>();
    assert_eq!(
        project_open_logs.len(),
        1,
        "项目打开前失败也只应新增一份项目日志"
    );
    let project_open_records = fs::read_to_string(&project_open_logs[0])
        .expect("项目打开失败日志应可读取")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("项目打开失败日志行必须是 JSON"))
        .collect::<Vec<_>>();
    assert!(
        project_open_records
            .iter()
            .all(|record| { record["event"] != "lua.script" && record["event"] != "lua.summary" }),
        "项目打开失败也不得生成 Lua 脚本哈希或摘要"
    );
}

#[test]
fn mv_lua_noop_does_not_validate_or_repair_translation_business_state() {
    let temporary = tempfile::tempdir().expect("应可建立 MV Lua 规划失败回归测试目录");
    let root = temporary.path();
    let game = root.join("mv-game");
    write_minimal_mv_game(&game);
    fs::write(
        game.join("www/data/Items.json"),
        serde_json::to_vec(&json!([
            null,
            {
                "id": 1,
                "name": "",
                "description": "回復 {hero}"
            }
        ]))
        .expect("含重叠 Placeholder 的 MV Items 夹具应可序列化"),
    )
    .expect("含重叠 Placeholder 的 MV Items 夹具应可写入");
    write_configuration(root, "http://127.0.0.1:9/v1/chat/completions");

    assert_success(
        "MV Lua 规划失败回归 Init",
        &run_att(root, init_arguments("mv", &game)),
    );
    assert_success(
        "MV Lua 规划失败回归 Extract",
        &run_att(
            root,
            arguments(&["mv", "extract", "--name", PROJECT, "--builtin"]),
        ),
    );

    let database = distribution_root(root)
        .join("projects/mv")
        .join(PROJECT)
        .join("project.db");
    let overlapping_rules = serde_json::to_string(&json!([
        { "scopes": ["database_entry"], "pattern": r"\{hero\}" },
        { "scopes": ["database_entry"], "pattern": "hero" }
    ]))
    .expect("重叠 Placeholder 资源应可序列化");
    assert_eq!(
        Connection::open(&database)
            .expect("MV 项目数据库应可打开")
            .execute(
                "UPDATE rpg_maker_translation_resource
                 SET canonical_json = ?1
                 WHERE resource_kind = 'placeholder_rules'",
                [&overlapping_rules],
            )
            .expect("应可安装只影响未译 Unit 的重叠 Placeholder"),
        1
    );

    let script = root.join("noop-mv.lua");
    fs::write(&script, "return\n").expect("MV 空 Lua 脚本应可写入");
    let mut lua_arguments = arguments(&["mv", "lua", "--name", PROJECT]);
    lua_arguments.push(script.as_os_str().to_owned());
    assert_success(
        "实际 att.exe 不应让无 Current Unit 的规划失败阻断 MV Lua",
        &run_att(root, lua_arguments),
    );

    let connection = Connection::open(&database).expect("MV 项目数据库应可重新打开");
    let (owner, group_location, unit_role): (String, String, String) = connection
        .query_row(
            "SELECT unit.owner, text_group.group_location, unit.unit_role
             FROM rpg_maker_text_unit AS unit
             JOIN rpg_maker_text_group AS text_group
               ON text_group.owner = unit.owner
              AND text_group.group_id = unit.group_id
             WHERE unit.source_content_json LIKE '%{hero}%'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("应定位含重叠 Placeholder 的 MV Unit");
    assert_eq!(
        connection
            .execute(
                "UPDATE rpg_maker_text_unit
                 SET translation_content_json = ?1, translation_state = ?2
                 WHERE owner = ?3
                   AND group_id = (
                       SELECT group_id FROM rpg_maker_text_group
                       WHERE owner = ?3 AND group_location = ?4
                   )
                   AND unit_role = ?5",
                (
                    serde_json::to_string("测试译文").expect("测试译文应可编码"),
                    vec![0xa5_u8; 32],
                    &owner,
                    &group_location,
                    &unit_role,
                ),
            )
            .expect("应建立会触发捕获诊断的 Current"),
        1
    );
    drop(connection);

    let mut second_arguments = arguments(&["mv", "lua", "--name", PROJECT]);
    second_arguments.push(script.into_os_string());
    assert_success(
        "实际 att.exe 不得在脚本结束时验证或修复业务状态",
        &run_att(root, second_arguments),
    );
    let connection = Connection::open(&database).expect("MV 项目数据库应可再次打开");
    let persisted: (String, Vec<u8>) = connection
        .query_row(
            "SELECT translation_content_json, translation_state
             FROM rpg_maker_text_unit
             WHERE owner = ?1
               AND group_id = (
                   SELECT group_id FROM rpg_maker_text_group
                   WHERE owner = ?1 AND group_location = ?2
               )
               AND unit_role = ?3",
            (&owner, &group_location, &unit_role),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("Lua 不得删除或修复测试写入的乱码状态");
    assert_eq!(persisted.0, serde_json::to_string("测试译文").unwrap());
    assert_eq!(persisted.1, vec![0xa5_u8; 32]);
}

#[test]
fn mv_lua_high_level_api_uses_readable_ids_and_raw_sql_can_bypass_it() {
    let temporary = tempfile::tempdir().expect("应可建立 MV Lua 全量清理端到端测试目录");
    let root = temporary.path();
    let game = root.join("mv-game");
    write_minimal_mv_game(&game);
    fs::write(
        game.join("www/data/Items.json"),
        serde_json::to_vec(&json!([
            null,
            {
                "id": 1,
                "name": "",
                "description": "回復一行目\n回復二行目"
            },
            {
                "id": 2,
                "name": "",
                "description": "魔法一行目\n魔法二行目"
            }
        ]))
        .expect("含 Placeholder 的 MV Items 夹具应可序列化"),
    )
    .expect("含 Placeholder 的 MV Items 夹具应可写入");
    write_configuration(root, "http://127.0.0.1:9/v1/chat/completions");

    assert_success(
        "MV Lua 全量清理 Init",
        &run_att(root, init_arguments("mv", &game)),
    );
    assert_success(
        "MV Lua 全量清理 Extract",
        &run_att(
            root,
            arguments(&["mv", "extract", "--name", PROJECT, "--builtin"]),
        ),
    );

    let workspace = distribution_root(root).join("projects/mv").join(PROJECT);
    let database = workspace.join("project.db");
    let ids = ["Items.json:1:description", "Items.json:2:description"];

    let set_script = root.join("set-mv-translations.lua");
    fs::write(
        &set_script,
        r#"local ids = {
  "Items.json:1:description",
  "Items.json:2:description",
}
local before = ctx.translation.list({ ids = ids })
assert(#before == 2)
ctx.translation.set(ids[1], { "治疗一行", "治疗二行" })
ctx.translation.set(ids[2], { "魔法一行", "魔法二行" })
local after = ctx.translation.list({ status = "translated", ids = ids })
assert(#after == 2)
assert(after[1].origin == "manual" and after[2].origin == "manual")
local context = ctx.translation.context({ ids[2], ids[1] })
assert(context[1].id == ids[2] and context[2].id == ids[1])
assert(type(ctx.terminology.list()) == "table")
"#,
    )
    .expect("MV Lua 高级 API 脚本应可写入");
    let mut set_arguments = arguments(&["mv", "lua", "--name", PROJECT]);
    set_arguments.push(set_script.into_os_string());
    assert_success(
        "实际 att.exe 执行 MV Lua 高级 API",
        &run_att(root, set_arguments),
    );

    let connection = Connection::open(&database).expect("建立人工译文后数据库应可重新打开");
    let manual_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM rpg_maker_manual_translation",
            [],
            |row| row.get(0),
        )
        .expect("人工译文数量应可读取");
    assert_eq!(manual_count, 2);
    let automatic_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM rpg_maker_text_unit
             WHERE translation_content_json IS NOT NULL OR translation_state IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("自动译文数量应可读取");
    assert_eq!(automatic_count, 0, "人工 set 必须清除同位置自动译文");
    drop(connection);

    let damage_script = root.join("damage-mv-manual.lua");
    fs::write(
        &damage_script,
        r#"ctx.db.execute([=[
UPDATE rpg_maker_manual_translation
SET translation_json = '["乱码一","乱码二","额外行"]'
WHERE readable_id = 'Items.json:2:description'
]=])
"#,
    )
    .expect("MV raw SQL 破坏脚本应可写入");
    let mut damage_arguments = arguments(&["mv", "lua", "--name", PROJECT]);
    damage_arguments.push(damage_script.into_os_string());
    assert_success("MV raw SQL 绕过高级 API", &run_att(root, damage_arguments));

    let selective_clear_script = root.join("clear-one-mv-translation.lua");
    fs::write(
        &selective_clear_script,
        format!("ctx.translation.clear(\"{}\")\n", ids[0]),
    )
    .expect("MV 单项清理 Lua 脚本应可写入");
    let mut selective_clear_arguments = arguments(&["mv", "lua", "--name", PROJECT]);
    selective_clear_arguments.push(selective_clear_script.into_os_string());
    assert_success(
        "实际 att.exe 清除一项人工译文",
        &run_att(root, selective_clear_arguments),
    );

    let connection = Connection::open(&database).expect("单项清理后数据库应可重新打开");
    let (remaining_id, remaining_translation): (String, String) = connection
        .query_row(
            "SELECT readable_id, translation_json FROM rpg_maker_manual_translation",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("未清理人工译文应继续存在");
    assert_eq!(remaining_id, ids[1]);
    assert_eq!(
        serde_json::from_str::<Value>(&remaining_translation).expect("raw SQL 译文应为 JSON"),
        json!(["乱码一", "乱码二", "额外行"])
    );
    drop(connection);

    let clear_script = root.join("clear-all-mv-translations.lua");
    fs::write(
        &clear_script,
        r#"for _, translation in ipairs(ctx.translation.list({ status = "translated" })) do
  if translation.origin == "manual" then
    ctx.translation.clear(translation.id)
  end
end
print("人工译文已清理")
"#,
    )
    .expect("MV 全量清理 Lua 脚本应可写入");

    let logs = workspace.join("logs");
    let logs_before = fs::read_dir(&logs)
        .expect("MV 项目日志目录应可读取")
        .map(|entry| entry.expect("MV 项目日志目录项应可读取").path())
        .collect::<Vec<_>>();
    let mut clear_arguments = arguments(&["mv", "lua", "--name", PROJECT]);
    clear_arguments.push(clear_script.into_os_string());
    assert_success(
        "实际 att.exe 执行 ctx.translation.clear 全量清理",
        &run_att(root, clear_arguments),
    );

    let connection = Connection::open(&database).expect("全量清理后数据库应可重新打开");
    let remaining: i64 = connection
        .query_row(
            "SELECT count(*) FROM rpg_maker_manual_translation",
            [],
            |row| row.get(0),
        )
        .expect("全量清理后人工译文数量应可读取");
    assert_eq!(remaining, 0, "人工译文必须全部清空");
    drop(connection);

    let new_logs = fs::read_dir(&logs)
        .expect("全量清理后 MV 项目日志目录应可读取")
        .map(|entry| entry.expect("MV 项目日志目录项应可读取").path())
        .filter(|path| !logs_before.contains(path))
        .collect::<Vec<_>>();
    assert_eq!(new_logs.len(), 1, "一次 Lua 命令只应新增一份项目日志");
    let records = read_project_log_records(&new_logs[0]);
    assert_eq!(
        records
            .iter()
            .filter(|record| record["event"] == "lua.print")
            .count(),
        1,
        "脚本 print 必须写入一条日志"
    );
    assert!(
        records
            .iter()
            .all(|record| { record["event"] != "lua.script" && record["event"] != "lua.summary" }),
        "Lua 日志不得保存脚本哈希或无实际作用的摘要"
    );
}

fn read_manual_toml(path: &Path) -> toml::Value {
    toml::from_str(
        &fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("{} 应可读取：{error}", path.display())),
    )
    .expect("Manual TOML 应可解析")
}

fn find_manual_entry<'a>(entries: &'a [toml::Value], id: &str) -> &'a toml::Value {
    entries
        .iter()
        .find(|entry| entry["id"].as_str() == Some(id))
        .unwrap_or_else(|| panic!("Manual TOML 应包含 {id}"))
}

fn set_manual_toml_field(path: &Path, id: &str, field: &str, value: toml::Value) {
    let mut document = read_manual_toml(path);
    let entries = document["translation"]
        .as_array_mut()
        .expect("Manual translation 必须是数组");
    let entry = entries
        .iter_mut()
        .find(|entry| entry["id"].as_str() == Some(id))
        .unwrap_or_else(|| panic!("Manual TOML 应包含 {id}"));
    entry
        .as_table_mut()
        .expect("Manual 条目必须是 table")
        .insert(field.to_owned(), value);
    fs::write(
        path,
        toml::to_string_pretty(&document).expect("Manual TOML 应可重新编码"),
    )
    .expect("Manual TOML 应可更新");
}

fn run_information_command(root: &Path, arguments: &[&str]) -> String {
    let output = Command::new(stage_att_executable(root))
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
    ]));
    values
}

fn arguments(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

fn distribution_root(root: &Path) -> PathBuf {
    root.join("release")
}

fn stage_att_executable(root: &Path) -> PathBuf {
    let source = Path::new(env!("CARGO_BIN_EXE_att"));
    let release = distribution_root(root);
    fs::create_dir_all(&release).expect("测试发行目录应可建立");
    let executable = release.join("att.exe");
    if !executable.exists() {
        fs::copy(source, &executable).expect("测试 att.exe 应可复制到独立发行目录");
        let source_directory = source.parent().expect("测试 att.exe 应拥有父目录");
        for entry in fs::read_dir(source_directory).expect("测试构建目录应可读取") {
            let path = entry.expect("测试构建目录项应可读取").path();
            if path.extension().is_some_and(|extension| extension == "dll") {
                let name = path.file_name().expect("DLL 应拥有文件名");
                fs::copy(&path, release.join(name)).expect("运行时 DLL 应可复制到发行目录");
            }
        }
    }
    executable
}

fn run_att(root: &Path, arguments: Vec<OsString>) -> Output {
    let mut command = Command::new(stage_att_executable(root));
    command
        .current_dir(root)
        .args(["--ui-language", "zh-Hans"])
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

fn assert_plain_progress_lines(stderr: &[u8], expected: &[&str]) {
    let text = String::from_utf8_lossy(stderr);
    assert!(!text.is_empty(), "进度 stderr 不得为空");
    assert!(text.ends_with('\n'), "每条进度必须以普通换行结束：{text:?}");
    assert!(!text.contains('\r'), "进度不得使用回车覆盖：{text:?}");
    assert!(
        !text.contains('\u{1b}'),
        "进度不得包含 ANSI 控制符：{text:?}"
    );
    for dynamic_marker in ["[|]", "[/]", "[-]", "[\\]", "[#"] {
        assert!(
            !text.contains(dynamic_marker),
            "进度不得包含 spinner 或进度条标记 {dynamic_marker:?}：{text:?}"
        );
    }
    for expected_text in expected {
        assert!(
            text.contains(expected_text),
            "进度 stderr 缺少 {expected_text:?}：{text}"
        );
    }
}

fn write_configuration(root: &Path, endpoint: &str) {
    write_configuration_for_client_with_protocol(root, endpoint, None, "primary", "e2e-model");
}

fn write_configuration_with_protocol(root: &Path, endpoint: &str, protocol: Option<&str>) {
    write_configuration_for_client_with_protocol(root, endpoint, protocol, "primary", "e2e-model");
}

fn write_configuration_for_client(root: &Path, endpoint: &str, client: &str, model: &str) {
    write_configuration_for_client_with_protocol(root, endpoint, None, client, model);
}

fn write_configuration_for_client_with_protocol(
    root: &Path,
    endpoint: &str,
    protocol: Option<&str>,
    client: &str,
    model: &str,
) {
    let protocol = protocol.map_or_else(String::new, |protocol| {
        format!("protocol = \"{protocol}\"\n")
    });
    let configuration = format!(
        r#"[prompts]
thinking_output = true
source_echo = false

[llm.clients.{client}]
{protocol}url = "{endpoint}"
api_key = "e2e-secret"
model = "{model}"
stream = false
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

[[languages]]
type = "english"
id = "en"
minimum_word_count = 1
minimum_letter_count = 2
ignored_terms = []
minimum_copied_word_count = 2
minimum_copied_letter_count = 4
allowed_terms = []

[translation]

[[translation.profiles]]
id = "local"
llm_client = "{client}"
target_task_user_message_characters = 10000
"#
    );
    let distribution = distribution_root(root);
    fs::create_dir_all(&distribution).expect("测试发行目录应可建立");
    fs::write(distribution.join("config.toml"), configuration).expect("测试配置应可写入");
}

fn write_rpg_maker_prompt(root: &Path) {
    write_translation_prompt(root);
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
    write_translation_prompt(root);
}

fn write_translation_prompt(root: &Path) {
    let prompt_root = distribution_root(root).join("prompts/translation");
    fs::create_dir_all(prompt_root.join("rules")).expect("Prompt 规则目录应可建立");
    fs::create_dir_all(prompt_root.join("examples")).expect("Prompt 示例目录应可建立");
    fs::write(
        prompt_root.join("system.md"),
        "把 {{source_language}} 翻译成 {{target_language}}。",
    )
    .expect("共享 system Prompt 应可写入");
    fs::write(prompt_root.join("thinking.md"), THINKING_PROMPT)
        .expect("共享 Thinking Prompt 应可写入");
    fs::write(
        prompt_root.join("rules/thinking.md"),
        "只输出带 think 和 translations 的 JSON object。",
    )
    .expect("思考模式规则应可写入");
    fs::write(
        prompt_root.join("examples/thinking.md"),
        "# 示例\n\n输入：{}\n\n输出：{\"think\":\"判断\",\"translations\":{}}",
    )
    .expect("思考模式示例应可写入");
}

fn expected_rpg_maker_description_user_message(units: &[(&str, Option<usize>)]) -> Value {
    let groups = units
        .iter()
        .map(|(text, task_id)| {
            let mut unit = json!({
                "role": "description",
                "text": [text]
            });
            if let Some(task_id) = task_id {
                unit["id"] = json!(task_id.to_string());
                unit["type"] = json!("free");
            }
            json!({
                "kind": "database_entry",
                "units": [unit]
            })
        })
        .collect::<Vec<_>>();
    json!({ "groups": groups })
}

fn expected_generic_user_message(units: &[(&str, Option<usize>)]) -> Value {
    let units = units
        .iter()
        .map(|(text, task_id)| {
            let mut unit = json!({ "text": [text] });
            if let Some(task_id) = task_id {
                unit["id"] = json!(task_id.to_string());
                unit["type"] = json!("free");
            }
            unit
        })
        .collect::<Vec<_>>();
    json!({
        "groups": [{
            "kind": "dialogue",
            "units": units
        }]
    })
}

fn parse_user_message(message: &str) -> Value {
    let json = message
        .strip_prefix("```json\n")
        .and_then(|value| value.strip_suffix("\n```"))
        .expect("模型 user message 必须是单一 JSON 围栏");
    serde_json::from_str(json).expect("模型 user message 围栏内部必须是稳定 JSON")
}

fn assert_pretty_translation_user_message(message: &str) {
    assert!(message.starts_with("```json\n{\n  \""));
    assert!(message.contains("\n  \"groups\": ["));
    assert!(message.ends_with("\n}\n```"));
}

fn user_message_texts(message: &Value) -> Vec<&str> {
    message["groups"]
        .as_array()
        .expect("user message groups 必须是数组")
        .iter()
        .flat_map(|group| {
            group["units"]
                .as_array()
                .expect("user message units 必须是数组")
        })
        .flat_map(|unit| {
            unit["text"]
                .as_array()
                .expect("user message text 必须是数组")
        })
        .map(|text| text.as_str().expect("user message text 元素必须是字符串"))
        .collect()
}

fn read_single_task_record_sharing_log_run_id(workspace: &Path) -> String {
    let mut task_records = read_task_records_sharing_log_run_ids(workspace);
    assert_eq!(task_records.len(), 1, "测试项目应只有一个任务记录运行");
    task_records.pop().expect("唯一任务记录应存在").1
}

fn read_task_records_sharing_log_run_ids(workspace: &Path) -> Vec<(OsString, String)> {
    let task_records_root = workspace.join("task-records");
    let mut run_directories = fs::read_dir(&task_records_root)
        .expect("任务记录根应存在")
        .collect::<Result<Vec<_>, _>>()
        .expect("任务记录运行目录应可读取");
    run_directories.sort_by_key(|entry| entry.file_name());
    run_directories
        .into_iter()
        .map(|run_directory| {
            let run_id = run_directory.file_name();
            let log_path = workspace
                .join("logs")
                .join(Path::new(&run_id).with_extension("jsonl"));
            assert!(
                log_path.is_file(),
                "任务记录目录名必须与 Translate 项目日志 RunId 相同：{}",
                log_path.display()
            );
            let log = fs::read_to_string(&log_path).expect("Translate 项目日志应可读取");
            assert!(
                log.lines().any(|line| {
                    serde_json::from_str::<Value>(line)
                        .is_ok_and(|record| record["context"]["command"] == "translate")
                }),
                "同 RunId 项目日志必须属于 Translate"
            );
            let task_files = fs::read_dir(run_directory.path())
                .expect("任务记录运行目录应可读取")
                .collect::<Result<Vec<_>, _>>()
                .expect("任务记录文件应可读取");
            assert_eq!(task_files.len(), 1, "一个 TaskBlock 只能生成一份任务记录");
            assert_eq!(task_files[0].file_name(), OsString::from("task-000001.md"));
            let markdown =
                fs::read_to_string(task_files[0].path()).expect("任务记录 Markdown 应可读取");
            (run_id, markdown)
        })
        .collect()
}

fn assert_workspace_does_not_contain(root: &Path, sentinel: &str) {
    for entry in fs::read_dir(root)
        .expect("工作区诊断目录应可读取")
        .collect::<Result<Vec<_>, _>>()
        .expect("工作区诊断文件应可读取")
    {
        let content = fs::read(entry.path()).expect("工作区诊断文件应可读取");
        assert!(
            find_subslice(&content, sentinel.as_bytes()).is_none(),
            "{} 不得包含 Thinking 正文",
            entry.path().display()
        );
    }
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

fn write_partial_retry_generic_group(path: &Path) {
    let units = PARTIAL_RETRY_SOURCES
        .iter()
        .enumerate()
        .map(|(index, text)| {
            json!({
                "id": format!("line-{}", index + 1),
                "text": text,
            })
        })
        .collect::<Vec<_>>();
    let line = json!({
        "id": "scene-1",
        "kind": "dialogue",
        "units": units,
    });
    fs::write(path, format!("{line}\n")).expect("Generic Partial 重试夹具应可写入");
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

fn write_partial_retry_mz_game(game_root: &Path) {
    write_minimal_mz_game(game_root);
    let mut items = vec![Value::Null];
    items.extend(
        PARTIAL_RETRY_SOURCES
            .iter()
            .enumerate()
            .map(|(index, description)| {
                json!({
                    "id": index + 1,
                    "name": "",
                    "description": description,
                })
            }),
    );
    fs::write(
        game_root.join("data/Items.json"),
        serde_json::to_vec(&items).expect("MZ Partial 重试 Items 应可序列化"),
    )
    .expect("MZ Partial 重试 Items 应可写入");
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
              AND group_row.group_id = unit.group_id
             WHERE unit.owner = ?1
             ORDER BY group_row.semantic_order_key, unit.semantic_order_key",
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

fn read_generic_units(database: &Path) -> Vec<(String, Option<String>)> {
    let connection = Connection::open(database).expect("Generic 项目数据库应可打开");
    let mut statement = connection
        .prepare(
            "SELECT unit.source_text, unit.translation
             FROM generic_unit AS unit
             JOIN generic_group AS group_row ON group_row.group_id = unit.group_id
             JOIN generic_file AS file_row
               ON file_row.relative_path = group_row.relative_path
             ORDER BY file_row.ordinal, group_row.ordinal, unit.ordinal",
        )
        .expect("Generic Unit 查询应可准备");
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("Generic Unit 查询应可执行")
        .map(|row| row.expect("Generic Unit 应可读取"))
        .collect()
}

fn read_items(path: &Path) -> Value {
    serde_json::from_slice(
        &fs::read(path).unwrap_or_else(|error| panic!("{} 应可读取：{error}", path.display())),
    )
    .expect("Items.json 必须是有效 JSON")
}

fn read_project_log_records(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("{} 应可读取：{error}", path.display()))
        .lines()
        .map(|line| serde_json::from_str(line).expect("项目日志行必须是 JSON object"))
        .collect()
}

fn read_sqlite_logical_snapshot(database: &Path) -> BTreeMap<String, Vec<Vec<String>>> {
    let connection = Connection::open(database).expect("项目数据库应可打开");
    let tables = sqlite_user_tables(&connection);
    let mut snapshot = BTreeMap::new();
    for table in tables {
        let table_identifier = quote_sql_identifier(&table);
        let columns = sqlite_table_columns(&connection, &table_identifier);
        let projection = columns
            .iter()
            .map(|column| format!("quote({})", quote_sql_identifier(column)))
            .collect::<Vec<_>>()
            .join(", ");
        let mut statement = connection
            .prepare(&format!("SELECT {projection} FROM {table_identifier}"))
            .expect("逻辑快照数据查询应可准备");
        let mut rows = statement
            .query_map([], |row| {
                (0..columns.len())
                    .map(|index| row.get::<_, String>(index))
                    .collect::<Result<Vec<_>, _>>()
            })
            .expect("逻辑快照数据查询应可执行")
            .collect::<Result<Vec<_>, _>>()
            .expect("逻辑快照数据应可读取");
        rows.sort();
        snapshot.insert(table, rows);
    }
    snapshot
}

fn sqlite_contains_text(database: &Path, needle: &str) -> bool {
    let connection = Connection::open(database).expect("项目数据库应可打开");
    for table in sqlite_user_tables(&connection) {
        let table_identifier = quote_sql_identifier(&table);
        for column in sqlite_table_columns(&connection, &table_identifier) {
            let column_identifier = quote_sql_identifier(&column);
            let query = format!(
                "SELECT EXISTS(
                    SELECT 1 FROM {table_identifier}
                    WHERE instr(CAST({column_identifier} AS BLOB), CAST(?1 AS BLOB)) > 0
                )"
            );
            let found: bool = connection
                .query_row(&query, [needle], |row| row.get(0))
                .expect("数据库逻辑列值搜索应可执行");
            if found {
                return true;
            }
        }
    }
    false
}

fn sqlite_user_tables(connection: &Connection) -> Vec<String> {
    let mut statement = connection
        .prepare(
            "SELECT name FROM sqlite_schema
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
             ORDER BY name",
        )
        .expect("数据表列表查询应可准备");
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("数据表列表查询应可执行")
        .collect::<Result<Vec<_>, _>>()
        .expect("数据表列表应可读取")
}

fn sqlite_table_columns(connection: &Connection, table_identifier: &str) -> Vec<String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table_identifier})"))
        .expect("数据表列查询应可准备");
    statement
        .query_map([], |row| row.get::<_, String>(1))
        .expect("数据表列查询应可执行")
        .collect::<Result<Vec<_>, _>>()
        .expect("数据表列应可读取")
}

fn quote_sql_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn serve_one_translation(listener: TcpListener) -> Result<Value, String> {
    serve_one_response(listener, json!({ "0": [TRANSLATION] }))
}

fn serve_translation_after_installing_run_plan_rollback(
    listener: TcpListener,
    database: &Path,
) -> Result<Value, String> {
    let (mut stream, request) = accept_request(listener)?;
    let connection = Connection::open(database)
        .map_err(|error| format!("打开 RunPlan 回滚测试数据库失败：{error}"))?;
    connection
        .execute_batch(
            "CREATE TRIGGER att_e2e_reject_translate_run_plan
             BEFORE INSERT ON translate_run_plan
             BEGIN
               SELECT RAISE(ABORT, 'run plan rollback e2e');
             END;",
        )
        .map_err(|error| format!("安装 RunPlan 回滚测试触发器失败：{error}"))?;
    drop(connection);
    let content = serde_json::to_string(&json!({
        "think": THINKING_SENTINEL,
        "translations": { "0": [TRANSLATION] }
    }))
    .map_err(|error| error.to_string())?;
    write_chat_response(&mut stream, &content)?;
    Ok(request)
}

fn serve_one_generic_translation(
    listener: TcpListener,
    translation: &str,
) -> Result<Value, String> {
    let lines = translation.split('\n').collect::<Vec<_>>();
    serve_one_response(listener, json!({ "0": lines }))
}

fn serve_one_responses_output(listener: TcpListener, translations: Value) -> Result<Value, String> {
    let (mut stream, request) = accept_request(listener)?;
    let json = serde_json::to_string(&json!({
        "think": THINKING_SENTINEL,
        "translations": translations
    }))
    .map_err(|error| error.to_string())?;
    let content = format!("```json\n{json}\n```");
    write_responses_response(&mut stream, &content)?;
    Ok(request)
}

fn serve_two_responses_outputs(
    listener: TcpListener,
    first_output: Value,
    second_output: Value,
) -> Result<[Value; 2], String> {
    let first_listener = listener.try_clone().map_err(|error| error.to_string())?;
    let first_request = serve_one_responses_output(first_listener, first_output)?;
    let second_request = serve_one_responses_output(listener, second_output)?;
    Ok([first_request, second_request])
}

fn serve_two_responses(
    listener: TcpListener,
    first_output: Value,
    second_output: Value,
) -> Result<[Value; 2], String> {
    let first_listener = listener.try_clone().map_err(|error| error.to_string())?;
    let first_request = serve_one_response(first_listener, first_output)?;
    let second_request = serve_one_response(listener, second_output)?;
    Ok([first_request, second_request])
}

fn serve_provider_spy(
    listener: TcpListener,
    stop: mpsc::Receiver<()>,
) -> Result<Vec<Value>, String> {
    listener
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    let mut requests = Vec::new();
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                // Windows 会让新 socket 继承 listener 的非阻塞状态。
                stream
                    .set_nonblocking(false)
                    .map_err(|error| error.to_string())?;
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .map_err(|error| error.to_string())?;
                stream
                    .set_write_timeout(Some(Duration::from_secs(10)))
                    .map_err(|error| error.to_string())?;
                requests.push(read_http_json(&mut stream)?);
                write_chat_response(
                    &mut stream,
                    &json!({
                        "think": THINKING_SENTINEL,
                        "translations": {
                            "0": ["春日来信"],
                            "1": ["秋日来信"]
                        }
                    })
                    .to_string(),
                )?;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                match stop.recv_timeout(Duration::from_millis(10)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(requests)
}

fn serve_one_response(listener: TcpListener, model_output: Value) -> Result<Value, String> {
    let (mut stream, request) = accept_request(listener)?;
    let json = serde_json::to_string(&json!({
        "think": THINKING_SENTINEL,
        "translations": model_output
    }))
    .map_err(|error| error.to_string())?;
    let content = format!("```json\n{json}\n```");
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

fn write_responses_response(stream: &mut TcpStream, content: &str) -> Result<(), String> {
    let body = json!({
        "id": "response-e2e",
        "status": "completed",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": content }]
        }],
        "usage": { "input_tokens": 11, "output_tokens": 3, "total_tokens": 14 }
    })
    .to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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
