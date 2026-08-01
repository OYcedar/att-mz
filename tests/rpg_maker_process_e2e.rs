#![cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]

//! Windows x64 生产进程边界的多引擎 CLI 与 RPG Maker 主流程黑盒测试。

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
use sha2::{Digest, Sha256};

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
const THINKING_PROMPT: &str = "Explain the checks inside the required why envelope.";
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
        for command in ["init", "extract", "translate", "write-back", "lua"] {
            assert!(
                help.contains(command),
                "{engine} 帮助必须列出 {command}：\n{help}"
            );
        }
    }
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
    assert_eq!(removed_argument.status.code(), Some(2));
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
    assert!(
        messages[1]["content"]
            .as_str()
            .is_some_and(|content| content.contains(SOURCE_TEXT)),
        "模型 user message 必须包含待译原文"
    );
    assert!(
        messages[0]["content"]
            .as_str()
            .is_some_and(|content| content.contains(THINKING_PROMPT)),
        "正式成功路径必须加载 Thinking Prompt"
    );
    let task_record = read_single_task_record_sharing_log_run_id(&mz_workspace);
    assert!(task_record.contains("# 翻译任务 000001 · 完成"));
    assert!(task_record.contains("## Thinking"));
    assert!(task_record.contains(THINKING_SENTINEL));
    assert!(task_record.contains("## Assistant"));
    assert!(task_record.contains("## 最终结果"));
    assert!(
        !String::from_utf8_lossy(&translate.stdout).contains(THINKING_SENTINEL)
            && !String::from_utf8_lossy(&translate.stderr).contains(THINKING_SENTINEL),
        "Thinking 正文不得进入终端输出"
    );
    assert_workspace_does_not_contain(&mz_workspace.join("logs"), THINKING_SENTINEL);
    assert!(
        find_subslice(
            &fs::read(mz_workspace.join("project.db")).expect("项目数据库应可读取"),
            THINKING_SENTINEL.as_bytes(),
        )
        .is_none(),
        "Thinking 正文不得进入权威数据库"
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
                "1": [PARTIAL_RETRY_TRANSLATIONS[0]],
                "3": [PARTIAL_RETRY_TRANSLATIONS[2]],
                "4": [PARTIAL_RETRY_TRANSLATIONS[3]]
            }),
            json!({ "1": [PARTIAL_RETRY_TRANSLATIONS[1]] }),
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
        first_user,
        expected_rpg_maker_description_user_message(&[
            (PARTIAL_RETRY_SOURCES[0], Some(1)),
            (PARTIAL_RETRY_SOURCES[1], Some(2)),
            (PARTIAL_RETRY_SOURCES[2], Some(3)),
            (PARTIAL_RETRY_SOURCES[3], Some(4)),
        ]),
        "首次请求必须按 A、B、C、D 的自然顺序发送完整 TaskBlock"
    );
    let second_user = second_request["messages"][1]["content"]
        .as_str()
        .expect("MZ 第二次请求 user message 必须是字符串");
    assert_eq!(
        second_user,
        expected_rpg_maker_description_user_message(&[
            (PARTIAL_RETRY_TRANSLATIONS[0], None),
            (PARTIAL_RETRY_SOURCES[1], Some(1)),
            (PARTIAL_RETRY_TRANSLATIONS[2], None),
            (PARTIAL_RETRY_TRANSLATIONS[3], None),
        ]),
        "第二次请求必须保留原 TaskBlock，并只给 B 分配 [1]"
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
    assert!(second_task_record.contains("# 翻译任务 000001 · 完成"));
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
                "1": PARTIAL_RETRY_TRANSLATIONS[0],
                "3": PARTIAL_RETRY_TRANSLATIONS[2],
                "4": PARTIAL_RETRY_TRANSLATIONS[3]
            }),
            json!({ "1": PARTIAL_RETRY_TRANSLATIONS[1] }),
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
    assert_eq!(
        first_user,
        expected_generic_user_message(&[
            (PARTIAL_RETRY_SOURCES[0], Some(1)),
            (PARTIAL_RETRY_SOURCES[1], Some(2)),
            (PARTIAL_RETRY_SOURCES[2], Some(3)),
            (PARTIAL_RETRY_SOURCES[3], Some(4)),
        ]),
        "首次 Generic 请求必须按 A、B、C、D 的自然顺序发送完整 TaskBlock"
    );
    let second_user = second_request["messages"][1]["content"]
        .as_str()
        .expect("Generic 第二次请求 user message 必须是字符串");
    assert_eq!(
        second_user,
        expected_generic_user_message(&[
            (PARTIAL_RETRY_TRANSLATIONS[0], None),
            (PARTIAL_RETRY_SOURCES[1], Some(1)),
            (PARTIAL_RETRY_TRANSLATIONS[2], None),
            (PARTIAL_RETRY_TRANSLATIONS[3], None),
        ]),
        "第二次 Generic 请求必须保留原 TaskBlock，并只给 B 分配 [1]"
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
    assert!(second_task_record.contains("# 翻译任务 000001 · 完成"));
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
            "pattern = '\\{[^}]+\\}'\n",
            "\n",
            "[[rule]]\n",
            "scopes = [\"dialogue\"]\n",
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
    for expected in [
        "建立 Generic 翻译计划",
        "必要前置条件未满足",
        "状态未改变",
        "修正指出的输入后重试",
    ] {
        assert!(
            stderr.contains(expected),
            "命令必须保留现有的规划失败语义 {expected:?}：{stderr}"
        );
    }
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
    assert_eq!(
        stderr.matches("翻译任务记录不可用或已降级").count(),
        1,
        "任务记录故障必须恰好警告一次：{stderr}"
    );
    assert!(
        stderr.contains("task-records"),
        "任务记录警告必须包含失败路径：{stderr}"
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
        .filter(|record| record["code"] == "observability.task_record_failed")
        .collect::<Vec<_>>();
    assert!(
        !task_record_failures.is_empty(),
        "任务记录故障必须写入 Translate 的同 RunId JSONL"
    );
    assert!(task_record_failures.iter().all(|record| {
        record["command"] == "translate"
            && record["level"] == "warn"
            && record["payload"]["diagnostic"]["stage"] == "logging"
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
    let workspace = distribution_root(root)
        .join("projects/generic")
        .join(PROJECT);
    let task_record = read_single_task_record_sharing_log_run_id(&workspace);
    assert!(task_record.contains("# 翻译任务 000001 · 完成"));
    assert!(task_record.contains(THINKING_SENTINEL));
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
    assert!(
        stderr.contains("near '='"),
        "语法诊断必须保留 Lua 编译器给出的具体原因：{stderr}"
    );

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
        records.iter().any(|record| record["code"] == "lua.script"),
        "语法失败日志必须保存脚本身份和哈希"
    );
    let summaries = records
        .iter()
        .filter(|record| record["code"] == "lua.summary")
        .collect::<Vec<_>>();
    assert_eq!(summaries.len(), 1, "语法失败必须且只能写入一条 Lua 摘要");
    let summary = summaries[0];
    for field in [
        "database_calls",
        "changed_rows",
        "translation_calls",
        "printed_lines",
    ] {
        assert_eq!(summary["payload"][field], 0, "{field} 必须明确记录为零");
    }
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
    let project_open_summaries = project_open_records
        .iter()
        .filter(|record| record["code"] == "lua.summary")
        .collect::<Vec<_>>();
    assert_eq!(
        project_open_summaries.len(),
        1,
        "预检成功但项目打开失败时必须且只能写入一条零调用摘要"
    );
    for field in [
        "database_calls",
        "changed_rows",
        "translation_calls",
        "printed_lines",
    ] {
        assert_eq!(
            project_open_summaries[0]["payload"][field], 0,
            "项目打开失败时 {field} 必须为零"
        );
    }
}

#[test]
fn mv_lua_clear_trusts_unchanged_current_with_additional_custom_placeholder_bytes() {
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
    let connection = Connection::open(&database).expect("MV 项目数据库应可打开");
    let mut statement = connection
        .prepare(
            "SELECT owner, group_location, unit_role, source_content_json
             FROM rpg_maker_text_unit
             ORDER BY owner, group_location, unit_role",
        )
        .expect("MV Unit locator 查询应可准备");
    let placeholder_units = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .expect("MV Unit locator 查询应可执行")
        .map(|row| row.expect("MV Unit locator 应可读取"))
        .filter(|(_, _, _, source)| {
            serde_json::from_str::<Value>(source)
                .expect("MV Unit source 必须是规范 JSON")
                .as_str()
                .is_some_and(|source| source.matches('\n').count() == 1)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        placeholder_units.len(),
        2,
        "测试夹具必须且只能提取两个含单个 LF 的标量 Unit"
    );
    let (corrupted_owner, corrupted_group_location, corrupted_unit_role, _) = &placeholder_units[1];
    drop(statement);
    let newline_placeholder = r#"[{"scopes":["database_entry"],"pattern":"\\n"}]"#;
    assert_eq!(
        connection
            .execute(
                "UPDATE rpg_maker_translation_resource
                 SET canonical_json = ?1
                 WHERE resource_kind = 'placeholder_rules'",
                [newline_placeholder],
            )
            .expect("应可安装 LF Placeholder 资源"),
        1
    );
    drop(connection);

    let set_script = root.join("set-custom-placeholder-current.lua");
    let set_calls = placeholder_units
        .iter()
        .enumerate()
        .map(|(index, (owner, group_location, unit_role, _))| {
            let ordinal = index + 1;
            format!(
                "ctx.translation.set(\n  {{ owner = [==[{owner}]==], group_location = [==[{group_location}]==], unit_role = [==[{unit_role}]==] }},\n  [==[治疗{ordinal}一行\n治疗{ordinal}二行]==]\n)"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&set_script, set_calls).expect("建立含 LF Placeholder Current 的 Lua 脚本应可写入");
    let mut set_arguments = arguments(&["mv", "lua", "--name", PROJECT]);
    set_arguments.push(set_script.into_os_string());
    assert_success(
        "实际 att.exe 建立含 LF Placeholder Current",
        &run_att(root, set_arguments),
    );

    let connection = Connection::open(&database).expect("建立 Current 后数据库应可重新打开");
    let (valid_translation, state_before): (String, Vec<u8>) = connection
        .query_row(
            "SELECT translation_content_json, translation_state
             FROM rpg_maker_text_unit
             WHERE owner = ?1 AND group_location = ?2 AND unit_role = ?3",
            (
                corrupted_owner,
                corrupted_group_location,
                corrupted_unit_role,
            ),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("含 LF Placeholder Current 应可读取");
    assert_eq!(
        serde_json::from_str::<Value>(&valid_translation).expect("Current 译文必须是规范 JSON"),
        json!("治疗2一行\n治疗2二行")
    );
    assert_eq!(state_before.len(), 32, "Current 必须保存完整翻译状态");

    // Translate 允许在保留 Custom Placeholder token 后，让自然正文额外出现相同字节。
    // 这里保留刚建立的合法 Current state，只把一处 LF 译文改成两处 LF，复现跨契约现场。
    let translation_with_additional_lf =
        serde_json::to_string("治疗2一行\n治疗2二行\n正文新增换行").expect("现场译文应可编码");
    assert_eq!(
        connection
            .execute(
                "UPDATE rpg_maker_text_unit
                 SET translation_content_json = ?1
                 WHERE owner = ?2 AND group_location = ?3 AND unit_role = ?4",
                (
                    &translation_with_additional_lf,
                    corrupted_owner,
                    corrupted_group_location,
                    corrupted_unit_role,
                ),
            )
            .expect("应可安装含额外 Custom 原片段的 Current 译文"),
        1
    );
    let (current_translation, state_after): (String, Vec<u8>) = connection
        .query_row(
            "SELECT translation_content_json, translation_state
             FROM rpg_maker_text_unit
             WHERE owner = ?1 AND group_location = ?2 AND unit_role = ?3",
            (
                corrupted_owner,
                corrupted_group_location,
                corrupted_unit_role,
            ),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("含额外 Custom 原片段的 Current 应可读取");
    assert_eq!(current_translation, translation_with_additional_lf);
    assert_eq!(
        state_after, state_before,
        "现场夹具只能修订译文正文，必须保留原 Current 状态"
    );
    let current_before_selective_clear: i64 = connection
        .query_row(
            "SELECT count(*)
             FROM rpg_maker_text_unit
             WHERE translation_content_json IS NOT NULL OR translation_state IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("全量清理前 Current 数量应可读取");
    assert_eq!(
        current_before_selective_clear, 2,
        "测试前必须同时存在一项含额外 Custom 原片段的 Current 和一项合法 Current"
    );
    drop(connection);

    let (first_owner, first_group_location, first_unit_role, _) = &placeholder_units[0];
    let selective_clear_script = root.join("clear-one-mv-translation.lua");
    fs::write(
        &selective_clear_script,
        format!(
            "ctx.translation.clear({{ owner = [==[{first_owner}]==], group_location = [==[{first_group_location}]==], unit_role = [==[{first_unit_role}]==] }})\n"
        ),
    )
    .expect("MV 单项清理 Lua 脚本应可写入");
    let mut selective_clear_arguments = arguments(&["mv", "lua", "--name", PROJECT]);
    selective_clear_arguments.push(selective_clear_script.into_os_string());
    assert_success(
        "实际 att.exe 清除一项并保留未改现场 Current",
        &run_att(root, selective_clear_arguments),
    );

    let connection = Connection::open(&database).expect("单项清理后数据库应可重新打开");
    let first_current: (Option<String>, Option<Vec<u8>>) = connection
        .query_row(
            "SELECT translation_content_json, translation_state
             FROM rpg_maker_text_unit
             WHERE owner = ?1 AND group_location = ?2 AND unit_role = ?3",
            (first_owner, first_group_location, first_unit_role),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("单项清理目标应可读取");
    assert_eq!(first_current, (None, None), "指定 Current 必须清除");
    let unchanged_current: (String, Vec<u8>) = connection
        .query_row(
            "SELECT translation_content_json, translation_state
             FROM rpg_maker_text_unit
             WHERE owner = ?1 AND group_location = ?2 AND unit_role = ?3",
            (
                corrupted_owner,
                corrupted_group_location,
                corrupted_unit_role,
            ),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("未改现场 Current 应继续存在");
    assert_eq!(unchanged_current.0, translation_with_additional_lf);
    assert_eq!(unchanged_current.1, state_before);
    let expected: i64 = connection
        .query_row(
            "SELECT count(*)
             FROM rpg_maker_text_unit
             WHERE translation_content_json IS NOT NULL OR translation_state IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("全量清理前剩余 Current 数量应可读取");
    assert_eq!(expected, 1, "单项清理后必须保留一项未改现场 Current");
    drop(connection);

    let clear_script = root.join("clear-all-mv-translations.lua");
    fs::write(
        &clear_script,
        r#"local expected = assert(tonumber(arg[1]), "missing expected count")
local rows = ctx.db.query([=[
SELECT owner, group_location, unit_role
FROM rpg_maker_text_unit
WHERE translation_content_json IS NOT NULL OR translation_state IS NOT NULL
ORDER BY owner, group_location, unit_role
]=])
assert(#rows == expected, "Current count changed before clear")
print("selected Current", #rows)
for _, row in ipairs(rows) do
  ctx.translation.clear({
    owner = row[1],
    group_location = row[2],
    unit_role = row[3]
  })
end
local remaining = ctx.db.query([=[
SELECT count(*)
FROM rpg_maker_text_unit
WHERE translation_content_json IS NOT NULL OR translation_state IS NOT NULL
]=])[1][1]
assert(remaining == 0, "Current remained after clear")
print("remaining Current", remaining)
"#,
    )
    .expect("MV 全量清理 Lua 脚本应可写入");
    let clear_script_sha256 =
        Sha256::digest(fs::read(&clear_script).expect("MV 全量清理 Lua 脚本应可重新读取"))
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();

    let logs = workspace.join("logs");
    let logs_before = fs::read_dir(&logs)
        .expect("MV 项目日志目录应可读取")
        .map(|entry| entry.expect("MV 项目日志目录项应可读取").path())
        .collect::<Vec<_>>();
    let mut clear_arguments = arguments(&["mv", "lua", "--name", PROJECT]);
    clear_arguments.push(clear_script.into_os_string());
    clear_arguments.push("--".into());
    clear_arguments.push(expected.to_string().into());
    assert_success(
        "实际 att.exe 执行 ctx.translation.clear 全量清理",
        &run_att(root, clear_arguments),
    );

    let connection = Connection::open(&database).expect("全量清理后数据库应可重新打开");
    let remaining: i64 = connection
        .query_row(
            "SELECT count(*)
             FROM rpg_maker_text_unit
             WHERE translation_content_json IS NOT NULL OR translation_state IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("全量清理后 Current 数量应可读取");
    assert_eq!(remaining, 0, "译文正文与翻译状态必须全部清空");
    let quick_check: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .expect("SQLite quick_check 应可执行");
    assert_eq!(quick_check, "ok", "清理后的数据库必须保持完整");
    let mut foreign_keys = connection
        .prepare("PRAGMA foreign_key_check")
        .expect("SQLite foreign_key_check 应可准备");
    assert!(
        foreign_keys
            .query([])
            .expect("SQLite foreign_key_check 应可执行")
            .next()
            .expect("SQLite foreign_key_check 结果应可读取")
            .is_none(),
        "清理后的数据库不得存在外键错误"
    );
    drop(foreign_keys);
    drop(connection);

    let new_logs = fs::read_dir(&logs)
        .expect("全量清理后 MV 项目日志目录应可读取")
        .map(|entry| entry.expect("MV 项目日志目录项应可读取").path())
        .filter(|path| !logs_before.contains(path))
        .collect::<Vec<_>>();
    assert_eq!(new_logs.len(), 1, "一次 Lua 命令只应新增一份项目日志");
    let records = fs::read_to_string(&new_logs[0])
        .expect("全量清理项目日志应可读取")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("项目日志行必须是 JSON"))
        .collect::<Vec<_>>();
    assert_eq!(
        records
            .iter()
            .filter(|record| record["code"] == "lua.script")
            .count(),
        1,
        "成功运行必须记录一次 Lua 脚本身份"
    );
    let script_record = records
        .iter()
        .find(|record| record["code"] == "lua.script")
        .expect("成功运行必须保留 Lua 脚本记录");
    assert_eq!(
        script_record["payload"]["fingerprint"], clear_script_sha256,
        "日志中的 SHA-256 必须等于脚本文件原始字节的普通 SHA-256"
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record["code"] == "lua.print")
            .count(),
        2,
        "脚本的两次 print 必须各写入一条日志"
    );
    let summaries = records
        .iter()
        .filter(|record| record["code"] == "lua.summary")
        .collect::<Vec<_>>();
    assert_eq!(summaries.len(), 1, "成功运行必须且只能记录一条 Lua 摘要");
    let summary = summaries[0];
    assert_eq!(summary["payload"]["database_calls"], 2);
    assert_eq!(summary["payload"]["changed_rows"], expected);
    assert_eq!(summary["payload"]["translation_calls"], expected);
    assert_eq!(summary["payload"]["printed_lines"], 2);
    let succeeded = records
        .iter()
        .position(|record| {
            record["code"] == "run.finished" && record["payload"]["outcome"] == "succeeded"
        })
        .expect("成功运行必须记录 succeeded 终态");
    assert_eq!(
        succeeded,
        records.len() - 1,
        "succeeded 必须是本次日志的最后一条记录"
    );
    assert!(
        records[..succeeded].iter().all(|record| {
            record["code"] != "failure.reported"
                && !(record["code"] == "run.finished" && record["payload"]["outcome"] == "failed")
        }),
        "成功终态之前不得伪报失败"
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

    let database = distribution_root(root)
        .join("projects/mz")
        .join(PROJECT)
        .join("project.db");
    let connection = Connection::open(&database).expect("项目数据库应可重新打开");
    let note: String = connection
        .query_row("SELECT note FROM lua_notes WHERE key = 'menu'", [], |row| {
            row.get(0)
        })
        .expect("文档示例应提交私有表内容");
    assert_eq!(note, "checked");
    drop(connection);

    let logs = database
        .parent()
        .expect("项目数据库应有父目录")
        .join("logs");
    let logs_before_rollback = fs::read_dir(&logs)
        .expect("回滚前项目日志目录应可读取")
        .map(|entry| entry.expect("回滚前项目日志目录项应可读取").path())
        .collect::<Vec<_>>();
    let rollback_script = workspace_root().join("docs/lua/examples/rollback.lua");
    let mut rollback_arguments = arguments(&["mz", "lua", "--name", PROJECT]);
    rollback_arguments.push(rollback_script.into_os_string());
    let rollback = run_att(root, rollback_arguments);
    assert!(!rollback.status.success(), "未捕获 Lua 错误必须让命令失败");
    let rollback_logs = fs::read_dir(&logs)
        .expect("回滚后项目日志目录应可读取")
        .map(|entry| entry.expect("回滚后项目日志目录项应可读取").path())
        .filter(|path| !logs_before_rollback.contains(path))
        .collect::<Vec<_>>();
    assert_eq!(rollback_logs.len(), 1, "失败 Lua 命令只应新增一份项目日志");
    let rollback_records = fs::read_to_string(&rollback_logs[0])
        .expect("失败 Lua 项目日志应可读取")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("失败项目日志行必须是 JSON"))
        .collect::<Vec<_>>();
    assert!(
        rollback_records
            .iter()
            .any(|record| record["code"] == "phase.started"),
        "失败 Lua 必须记录阶段开始"
    );
    assert!(
        rollback_records
            .iter()
            .all(|record| record["code"] != "phase.finished"),
        "失败 Lua 不得伪报阶段完成"
    );
    let rollback_summaries = rollback_records
        .iter()
        .filter(|record| record["code"] == "lua.summary")
        .collect::<Vec<_>>();
    assert_eq!(
        rollback_summaries.len(),
        1,
        "失败 Lua 必须且只能记录一条 Host 调用摘要"
    );
    let rollback_summary = rollback_summaries[0];
    assert_eq!(rollback_summary["payload"]["database_calls"], 2);
    assert_eq!(rollback_summary["payload"]["changed_rows"], 1);
    assert_eq!(rollback_summary["payload"]["translation_calls"], 0);
    assert_eq!(rollback_summary["payload"]["printed_lines"], 0);
    assert!(
        rollback_summary["message"]
            .as_str()
            .is_some_and(|message| message.contains("Lua 统计") && !message.contains("已提交")),
        "失败摘要必须使用不宣称事务成功的中性文案"
    );
    assert!(
        rollback_records
            .iter()
            .any(|record| record["code"] == "failure.reported"),
        "失败 Lua 必须及时记录结构化失败"
    );
    assert!(
        rollback_records.iter().any(|record| {
            record["code"] == "run.finished" && record["payload"]["outcome"] == "failed"
        }),
        "失败 Lua 必须记录 failed 终态"
    );

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
        r#"[prompts]
locale = "zh-Hans"
thinking_output = true

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

[[translation.profiles]]
id = "local"
llm_client = "primary"
target_task_user_message_characters = 10000
"#
    );
    let distribution = distribution_root(root);
    fs::create_dir_all(&distribution).expect("测试发行目录应可建立");
    fs::write(distribution.join("config.toml"), configuration).expect("测试配置应可写入");
}

fn write_rpg_maker_prompt(root: &Path) {
    let prompt_root = distribution_root(root).join("prompts/rpg_maker/zh-Hans");
    fs::create_dir_all(&prompt_root).expect("Prompt 目录应可建立");
    fs::write(
        prompt_root.join("system.md"),
        "Translate {{source_language}} into {{target_language}}. Return the required JSON object.",
    )
    .expect("system Prompt 应可写入");
    fs::write(prompt_root.join("thinking.md"), THINKING_PROMPT).expect("Thinking Prompt 应可写入");
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
    let prompt_root = distribution_root(root).join("prompts/generic/zh-Hans");
    fs::create_dir_all(&prompt_root).expect("Generic Prompt 目录应可建立");
    fs::write(
        prompt_root.join("system.md"),
        "Translate {{source_language}} into {{target_language}}. Return string values.",
    )
    .expect("Generic system Prompt 应可写入");
    fs::write(prompt_root.join("thinking.md"), THINKING_PROMPT)
        .expect("Generic Thinking Prompt 应可写入");
}

fn expected_rpg_maker_description_user_message(units: &[(&str, Option<usize>)]) -> String {
    let mut message = String::new();
    for (index, (text, task_id)) in units.iter().copied().enumerate() {
        if index > 0 {
            message.push('\n');
        }
        message.push_str("## Database Text\n\nDescription [");
        match task_id {
            Some(task_id) => message.push_str(&task_id.to_string()),
            None => message.push('-'),
        }
        message.push_str("] (free line breaking):\n\n> ");
        message.push_str(text);
        message.push('\n');
    }
    message
}

fn expected_generic_user_message(units: &[(&str, Option<usize>)]) -> String {
    let mut message = "Groups:\nkind=\"dialogue\"\nunits:\n".to_owned();
    for (text, task_id) in units.iter().copied() {
        message.push('[');
        match task_id {
            Some(task_id) => message.push_str(&task_id.to_string()),
            None => message.push('-'),
        }
        message.push_str("] ");
        message.push_str(
            &serde_json::to_string(text).expect("Generic user message 文本应可编码为 JSON"),
        );
        message.push('\n');
    }
    message.push('\n');
    message
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
                        .is_ok_and(|record| record["command"] == "translate")
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
              AND group_row.group_location = unit.group_location
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

fn serve_one_translation(listener: TcpListener) -> Result<Value, String> {
    serve_one_response(listener, json!({ "1": [TRANSLATION] }))
}

fn serve_one_generic_translation(
    listener: TcpListener,
    translation: &str,
) -> Result<Value, String> {
    serve_one_response(listener, json!({ "1": translation }))
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
                    &format!(
                        "<why>{THINKING_SENTINEL}</why>\n{}",
                        json!({ "1": "春日来信", "2": "秋日来信" })
                    ),
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
    let assistant_json = serde_json::to_string(&model_output).map_err(|error| error.to_string())?;
    let content = format!("<why>{THINKING_SENTINEL}</why>\n{assistant_json}");
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
