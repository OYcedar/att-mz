#![cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]

//! Generic CLI 的独立生产进程边界测试。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::Duration;

use rusqlite::Connection;

const PROJECT: &str = "generic-observable";
const MISSING_CAPTURE_PROJECT: &str = "generic-missing-text-capture";
const MISSING_CAPTURE_API_KEY: &str = "PRIVATE_MISSING_CAPTURE_API_KEY";
const MISSING_CAPTURE_SOURCE: &str = "秘密本文あ甲触发缺组乙";
const LUA_LANGUAGE_CONFIGURATION: &str = r#"[[languages]]
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
"#;

#[test]
fn removed_progress_argument_is_rejected_by_the_process_cli() {
    let output = Command::new(env!("CARGO_BIN_EXE_att"))
        .args([
            "--ui-language",
            "en",
            "--progress",
            "off",
            "generic",
            "extract",
            "--name",
            "demo",
        ])
        .output()
        .expect("att.exe 应可执行");

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("CLI 错误必须是 UTF-8");
    assert!(
        stderr.contains("Unexpected argument") && stderr.contains("--progress"),
        "已删除的进度参数必须作为未知参数报告：{stderr}"
    );
}

#[test]
fn manual_export_check_and_apply_work_for_generic() {
    let temporary = tempfile::tempdir().expect("应可建立 Generic Manual 测试目录");
    let root = temporary.path();
    let input = root.join("input");
    fs::create_dir(&input).expect("应可建立 Generic Manual 输入目录");
    let source_file = input.join("story.jsonl");
    fs::write(
        &source_file,
        concat!(
            r#"{"id":"story","kind":"dialogue","units":["#,
            r#"{"id":"body","text":"こんにちは\n世界"},"#,
            r#"{"id":"name","text":"名前"},"#,
            r#"{"id":"english","text":"Already done"},"#,
            r#"{"id":"blank","text":"   "}]}"#,
            "\n"
        ),
    )
    .expect("应可写入 Generic Manual 输入");
    let distribution = distribution_root(root);
    fs::create_dir_all(&distribution).expect("应可建立 Generic Manual 发行目录");
    fs::write(distribution.join("config.toml"), LUA_LANGUAGE_CONFIGURATION)
        .expect("应可写入 Generic Manual 配置");

    assert_success(
        "Generic Manual Init",
        &run_att(
            root,
            &[
                "generic",
                "init",
                "--name",
                PROJECT,
                "--path",
                input.to_str().expect("临时输入路径应是 Unicode"),
                "--source-language",
                "ja",
                "--target-language",
                "zh-Hans",
            ],
        ),
    );
    assert_success(
        "Generic Manual Extract",
        &run_att(root, &["generic", "extract", "--name", PROJECT]),
    );

    let manual = root.join("generic-manual.toml");
    assert_success(
        "Generic Manual export",
        &run_att(
            root,
            &[
                "generic",
                "manual",
                "export",
                "--name",
                PROJECT,
                manual.to_str().expect("Manual 路径应是 Unicode"),
            ],
        ),
    );
    let document = read_manual_toml(&manual);
    let entries = document["translation"]
        .as_array()
        .expect("Manual translation 必须是数组");
    assert_eq!(entries.len(), 2, "只应导出真正需要翻译的日文条目");
    let body = find_manual_entry(entries, "story.jsonl:line1:unit1:text");
    assert_eq!(body["type"].as_str(), Some("free"));
    assert_eq!(
        body["source"].as_array().expect("body source 必须是数组"),
        &[
            toml::Value::String("こんにちは".to_owned()),
            toml::Value::String("世界".to_owned()),
        ]
    );
    let name = find_manual_entry(entries, "story.jsonl:line1:unit2:text");
    assert_eq!(name["type"].as_str(), Some("free"));
    for entry in entries {
        let table = entry.as_table().expect("Manual 条目必须是 table");
        assert_eq!(table.len(), 4);
        assert!(
            table
                .keys()
                .all(|key| { matches!(key.as_str(), "id" | "type" | "source" | "translation") })
        );
    }

    assert_success(
        "Generic Manual check 未填写",
        &run_att(
            root,
            &[
                "generic",
                "manual",
                "check",
                "--name",
                PROJECT,
                manual.to_str().expect("Manual 路径应是 Unicode"),
            ],
        ),
    );
    set_manual_toml_field(
        &manual,
        "story.jsonl:line1:unit1:text",
        "translation",
        toml::Value::Array(vec![toml::Value::String("合并后的译文".to_owned())]),
    );
    assert_success(
        "Generic Manual apply 单项",
        &run_att(
            root,
            &[
                "generic",
                "manual",
                "apply",
                "--name",
                PROJECT,
                manual.to_str().expect("Manual 路径应是 Unicode"),
            ],
        ),
    );

    let workspace = distribution.join("projects/generic").join(PROJECT);
    let database = workspace.join("project.db");
    let connection = Connection::open(&database).expect("Generic Manual 数据库应可打开");
    let count: i64 = connection
        .query_row(
            "SELECT count(*) FROM generic_manual_translation",
            [],
            |row| row.get(0),
        )
        .expect("Generic 人工译文数量应可读取");
    assert_eq!(count, 1);
    drop(connection);

    set_manual_toml_field(
        &manual,
        "story.jsonl:line1:unit2:text",
        "translation",
        toml::Value::Array(vec![toml::Value::String("名称".to_owned())]),
    );
    set_manual_toml_field(
        &manual,
        "story.jsonl:line1:unit1:text",
        "source",
        toml::Value::Array(vec![toml::Value::String("错误原文".to_owned())]),
    );
    let invalid = run_att(
        root,
        &[
            "generic",
            "manual",
            "apply",
            "--name",
            PROJECT,
            manual.to_str().expect("Manual 路径应是 Unicode"),
        ],
    );
    assert_eq!(invalid.status.code(), Some(1));
    let stderr = String::from_utf8(invalid.stderr).expect("Manual 错误必须是 UTF-8");
    assert!(stderr.contains("story.jsonl:line1:unit1:text") && stderr.contains("manual export"));
    assert!(!stderr.contains("group_id") && !stderr.contains("unit_id"));
    let connection = Connection::open(&database).expect("失败后 Generic 数据库应可打开");
    let count: i64 = connection
        .query_row(
            "SELECT count(*) FROM generic_manual_translation",
            [],
            |row| row.get(0),
        )
        .expect("失败后 Generic 人工译文数量应可读取");
    assert_eq!(count, 1, "混合有效与无效条目必须原子失败");
    drop(connection);

    set_manual_toml_field(
        &manual,
        "story.jsonl:line1:unit1:text",
        "source",
        toml::Value::Array(vec![
            toml::Value::String("こんにちは".to_owned()),
            toml::Value::String("世界".to_owned()),
        ]),
    );
    assert_success(
        "Generic Manual apply 全部",
        &run_att(
            root,
            &[
                "generic",
                "manual",
                "apply",
                "--name",
                PROJECT,
                manual.to_str().expect("Manual 路径应是 Unicode"),
            ],
        ),
    );
    assert_success(
        "Generic Manual WriteBack",
        &run_att(root, &["generic", "write-back", "--name", PROJECT]),
    );
    let output = workspace.join("write_back/story.jsonl");
    let written = read_single_jsonl_group(&output);
    assert_eq!(written["units"][0]["text"], "合并后的译文");
    assert_eq!(written["units"][1]["text"], "名称");
    assert_eq!(written["units"][2]["text"], "Already done");
    assert_eq!(written["units"][3]["text"], "   ");

    fs::write(
        &source_file,
        concat!(
            r#"{"id":"story","kind":"dialogue","units":["#,
            r#"{"id":"body","text":"新しい\n本文"},"#,
            r#"{"id":"name","text":"名前"},"#,
            r#"{"id":"english","text":"Already done"},"#,
            r#"{"id":"blank","text":"   "}]}"#,
            "\n"
        ),
    )
    .expect("变化后的 Generic Manual 输入应可写入");
    assert_success(
        "Generic Manual 原文变化后 Extract",
        &run_att(root, &["generic", "extract", "--name", PROJECT]),
    );
    let after_change_manual = root.join("generic-after-change.toml");
    assert_success(
        "Generic Manual 原文变化后 export",
        &run_att(
            root,
            &[
                "generic",
                "manual",
                "export",
                "--name",
                PROJECT,
                after_change_manual
                    .to_str()
                    .expect("变化后 Manual 路径应是 Unicode"),
            ],
        ),
    );
    let after_change = read_manual_toml(&after_change_manual);
    let after_change_entries = after_change["translation"]
        .as_array()
        .expect("变化后 Manual translation 必须是数组");
    assert_eq!(after_change_entries.len(), 1);
    find_manual_entry(after_change_entries, "story.jsonl:line1:unit1:text");
    assert_success(
        "Generic Manual 原文变化后 WriteBack",
        &run_att(root, &["generic", "write-back", "--name", PROJECT]),
    );
    let written = read_single_jsonl_group(&output);
    assert_eq!(written["units"][0]["text"], "新しい\n本文");
    assert_eq!(written["units"][1]["text"], "名称");
    let connection = Connection::open(&database).expect("过期 Generic 人工译文数据库应可打开");
    let count: i64 = connection
        .query_row(
            "SELECT count(*) FROM generic_manual_translation",
            [],
            |row| row.get(0),
        )
        .expect("过期 Generic 人工译文数量应可读取");
    assert_eq!(count, 2, "过期人工译文必须保留，不能静默删除");
    let (source, translation): (String, String) = connection
        .query_row(
            "SELECT source_json, translation_json FROM generic_manual_translation
             WHERE group_id = 'story' AND unit_id = 'body'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("过期人工译文快照应可读取");
    assert_eq!(source, r#"["こんにちは","世界"]"#);
    assert_eq!(translation, r#"["合并后的译文"]"#);
}

#[test]
fn generic_non_tty_progress_uses_plain_lines_and_jsonl_diagnostic_is_observable() {
    let temporary = tempfile::tempdir().expect("应可建立 Generic 进程测试目录");
    let root = temporary.path();
    let input = root.join("input");
    fs::create_dir(&input).expect("应可建立 Generic 输入目录");
    let distribution = distribution_root(root);
    fs::create_dir_all(&distribution).expect("测试发行目录应可建立");
    fs::write(
        distribution.join("config.toml"),
        r#"[prompts]
thinking_output = true
source_echo = false

[llm.clients.local]
url = "http://127.0.0.1:9/v1/chat/completions"
api_key = "unused-test-secret"
model = "unused-test-model"
stream = false
max_concurrent_requests = 1
connect_timeout_ms = 1000
read_timeout_ms = 1000
request_timeout_ms = 1000
proxy = false
additional_pem_files = []
retry_delays_ms = [1]
max_retry_after_ms = 1
parameters = '''
{}
'''

[[languages]]
type = "japanese"
id = "ja"
minimum_kana_characters = 1
allowed_terms = []

[translation]

[[translation.profiles]]
id = "local"
llm_client = "local"
target_task_user_message_characters = 10000
"#,
    )
    .expect("应可写入测试配置");
    let prompt_root = distribution.join("prompts/translation");
    fs::create_dir_all(prompt_root.join("rules")).expect("应可建立 Prompt 规则目录");
    fs::create_dir_all(prompt_root.join("examples")).expect("应可建立 Prompt 示例目录");
    fs::write(
        prompt_root.join("system.md"),
        "把 {{source_language}} 翻译成 {{target_language}}。",
    )
    .expect("应可写入共享 system Prompt");
    fs::write(
        prompt_root.join("thinking.md"),
        "在 think 中写出影响译文的判断。",
    )
    .expect("应可写入共享 Thinking Prompt");
    fs::write(
        prompt_root.join("rules/thinking.md"),
        "只输出带 think 和 translations 的 JSON object。",
    )
    .expect("应可写入思考模式规则");
    fs::write(
        prompt_root.join("examples/thinking.md"),
        "# 示例\n\n输入：{}\n\n输出：{\"think\":\"判断\",\"translations\":{}}",
    )
    .expect("应可写入思考模式示例");
    fs::write(
        input.join("story.jsonl"),
        concat!(
            r#"{"id":"story","kind":"dialogue","units":["#,
            r#"{"id":"line","text":"原文"}]}"#,
            "\n"
        ),
    )
    .expect("应可写入 Generic JSONL");

    let init = run_att(
        root,
        &[
            "generic",
            "init",
            "--name",
            PROJECT,
            "--path",
            input.to_str().expect("临时路径应是 Unicode"),
            "--source-language",
            "ja",
            "--target-language",
            "zh-Hans",
        ],
    );
    assert_success("Generic Init", &init);
    assert_plain_progress_lines(
        &init.stderr,
        &[
            "Initializing the Generic project",
            "Finalizing required resources",
        ],
    );

    let extract = run_att(root, &["generic", "extract", "--name", PROJECT]);
    assert_success("Generic Extract", &extract);
    assert_plain_progress_lines(
        &extract.stderr,
        &[
            "Scanning Generic JSONL input",
            "Finalizing required resources",
        ],
    );

    let lua_script = root.join("noop.lua");
    fs::write(&lua_script, "return\n").expect("应可写入 Lua 脚本");
    let lua = run_att(
        root,
        &[
            "generic",
            "lua",
            "--name",
            PROJECT,
            lua_script.to_str().expect("临时路径应是 Unicode"),
        ],
    );
    assert_success("Generic Lua", &lua);
    assert_plain_progress_lines(
        &lua.stderr,
        &[
            "Running the project Lua program",
            "Finalizing required resources",
        ],
    );

    let write_back = run_att(root, &["generic", "write-back", "--name", PROJECT]);
    assert_success("Generic WriteBack", &write_back);
    assert_plain_progress_lines(
        &write_back.stderr,
        &[
            "Planning document rewrites",
            "Publishing output",
            "Finalizing required resources",
        ],
    );

    let empty_input = root.join("empty-input");
    fs::create_dir(&empty_input).expect("应可建立空 Generic 输入目录");
    let no_work_project = "generic-no-work";
    assert_success(
        "空 Generic Init",
        &run_att(
            root,
            &[
                "generic",
                "init",
                "--name",
                no_work_project,
                "--path",
                empty_input.to_str().expect("临时路径应是 Unicode"),
                "--source-language",
                "ja",
                "--target-language",
                "zh-Hans",
            ],
        ),
    );
    assert_success(
        "空 Generic Extract",
        &run_att(root, &["generic", "extract", "--name", no_work_project]),
    );
    let translate = run_att(
        root,
        &["generic", "translate", "--name", no_work_project, "local"],
    );
    assert_success("无请求 Generic Translate", &translate);
    let translate_stderr = assert_plain_progress_lines(
        &translate.stderr,
        &[
            "Planning translation tasks",
            "Confirmed translation tasks: No work is needed",
            "Finalizing required resources",
        ],
    );
    assert!(
        !translate_stderr.contains("No model request is needed"),
        "无需模型请求属于最终结果，不得作为进度阶段重复呈现：{translate_stderr}"
    );
    let translate_stdout =
        String::from_utf8(translate.stdout).expect("Translate stdout 必须是 UTF-8");
    assert!(
        translate_stdout.contains(
            "All translation units are current; no model request was needed in this run."
        ),
        "最终结果仍应说明没有发送模型请求：{translate_stdout}"
    );
    assert!(
        !translate_stderr.contains("0/0") && !translate_stderr.contains("100%"),
        "零工作量不得显示 0/0 或伪造 100%：{translate_stderr}"
    );
    assert!(
        !distribution
            .join("projects/generic")
            .join(no_work_project)
            .join("task-records")
            .exists(),
        "默认开启任务记录但没有模型任务时不得建立空目录"
    );

    let workspace = distribution.join("projects/generic").join(PROJECT);
    fs::write(workspace.join("task-records"), b"not-a-directory")
        .expect("普通文件应可稳定触发 Generic 任务记录写入失败");
    let degraded_translate = run_att(root, &["generic", "translate", "--name", PROJECT, "local"]);
    assert_success("任务记录降级 Generic Translate", &degraded_translate);
    let degraded_stderr =
        String::from_utf8(degraded_translate.stderr).expect("stderr 必须是 UTF-8");
    assert_plain_progress_text(
        &degraded_stderr,
        &[
            "Planning translation tasks",
            "Confirmed translation tasks",
            "Finalizing required resources",
        ],
    );
    assert!(
        degraded_stderr.contains("0% (0/1)") && degraded_stderr.contains("100% (1/1)"),
        "非 TTY Translate 必须打印真实观测到的整数百分比：{degraded_stderr}"
    );
    for expected in [
        "Warning:",
        "task-records",
        "Reason:",
        "The required object does not exist",
        "Impact:",
        "Business state was not changed",
        "Action:",
        "Check the path, filesystem state, and permissions",
    ] {
        assert!(
            degraded_stderr.contains(expected),
            "Generic 任务记录四字段警告缺少 {expected:?}：{degraded_stderr}"
        );
    }
    assert_eq!(
        degraded_stderr.matches("task-records").count(),
        1,
        "Generic 任务记录故障必须恰好呈现一次自然路径：{degraded_stderr}"
    );
    assert!(
        !degraded_stderr.contains("Translation task records are unavailable or degraded"),
        "Generic 任务记录故障不得保留旧的一行模糊文案：{degraded_stderr}"
    );
    let mut observed_same_run_log = false;
    for entry in fs::read_dir(workspace.join("logs")).expect("Generic 项目日志目录应存在")
    {
        let path = entry.expect("Generic 项目日志项应可读取").path();
        let expected_run_id = path
            .file_stem()
            .expect("项目日志应以 RunId 命名")
            .to_string_lossy()
            .into_owned();
        for line in fs::read_to_string(&path)
            .expect("Generic 项目日志应可读取")
            .lines()
        {
            let record: serde_json::Value =
                serde_json::from_str(line).expect("Generic 项目日志行应为 JSON");
            if record["event"] == "diagnostic.task_record" {
                assert_eq!(record["run_id"], expected_run_id);
                assert_eq!(record["context"]["command"], "translate");
                assert_eq!(record["level"], "warn");
                let payload = record["payload"]
                    .as_object()
                    .expect("Generic 任务记录诊断 payload 必须是对象");
                assert_eq!(payload.len(), 5);
                assert_eq!(payload["relation"], "primary");
                for field in ["object", "reason", "impact", "help"] {
                    assert!(
                        payload[field]
                            .as_str()
                            .is_some_and(|value| !value.is_empty()),
                        "Generic 任务记录诊断 {field} 必须是非空可读文本"
                    );
                }
                observed_same_run_log = true;
            }
        }
    }
    assert!(
        observed_same_run_log,
        "Generic 任务记录故障必须写入同一 RunId 的 Translate JSONL"
    );

    let output = run_att(root, &["generic", "extract", "--name", PROJECT]);
    assert_success("非 TTY Generic Extract", &output);
    assert_plain_progress_lines(
        &output.stderr,
        &[
            "Scanning Generic JSONL input",
            "Finalizing required resources",
        ],
    );

    let nested = input.join("nested");
    fs::create_dir(&nested).expect("应可建立嵌套输入目录");
    const SENTINEL: &str = "PRIVATE_JSON_SENTINEL";
    fs::write(
        nested.join("bad.jsonl"),
        format!(
            "{{\"id\":\"bad\",\"kind\":\"dialogue\",\"units\":[{{\"id\":\"line\",\"text\":\"x\"}}],\"{SENTINEL}\":true}}"
        ),
    )
    .expect("应可写入含未知字段的 JSONL");
    let invalid = run_att(root, &["generic", "extract", "--name", PROJECT]);
    assert_eq!(invalid.status.code(), Some(1));
    let stderr = String::from_utf8(invalid.stderr).expect("stderr 必须是 UTF-8");
    assert!(
        stderr.contains("bad.jsonl"),
        "诊断必须指出损坏文件：{stderr}"
    );
    assert!(
        stderr.contains("nested/bad.jsonl:line1"),
        "诊断必须指出损坏行号：{stderr}"
    );
    assert!(stderr.contains("Reason:") && stderr.contains("Action:"));
    for internal in [
        "generic.jsonl",
        "operation=",
        "json_category=",
        "json_column=",
    ] {
        assert!(
            !stderr.contains(internal),
            "公开诊断不得显示内部字段 {internal:?}：{stderr}"
        );
    }
    assert!(
        !stderr.contains(SENTINEL),
        "公开诊断不得包含原始 JSON 字段或 serde 自由文本：{stderr}"
    );
    assert!(
        !stderr.contains("RPG Maker"),
        "Generic JSONL 诊断不得复用 RPG Maker 文案：{stderr}"
    );
}

#[test]
fn generic_write_back_materializes_manual_translation_exactly() {
    let temporary = tempfile::tempdir().expect("应可建立 Generic 逐字写回进程测试目录");
    let root = temporary.path();
    let input = root.join("input");
    fs::create_dir(&input).expect("应可建立 Generic 逐字物化输入目录");
    fs::write(
        input.join("settings.jsonl"),
        "{\"id\":\"settings\",\"kind\":\"settings\",\"units\":[{\"id\":\"categories\",\"text\":\"General, Misc, Audio, Toggle\"}]}\n",
    )
    .expect("应可写入 Generic 逐字物化输入");

    let distribution = distribution_root(root);
    fs::create_dir_all(&distribution).expect("应可建立测试发行目录");
    fs::write(distribution.join("config.toml"), LUA_LANGUAGE_CONFIGURATION)
        .expect("Lua 高级 API 语言配置应可写入");
    let script = root.join("set-categories.lua");
    fs::write(
        &script,
        concat!(
            "ctx.translation.set(\n",
            "  \"settings.jsonl:line1:unit1:text\",\n",
            "  { \"常规、杂项、声音、开关\" }\n",
            ")\n",
        ),
    )
    .expect("应可写入 Generic 精确修订脚本");

    for (project, source_language) in [
        ("generic-exact-materialization-en", "en"),
        ("generic-exact-materialization-ja", "ja"),
    ] {
        assert_success(
            "Generic 逐字物化 Init",
            &run_att(
                root,
                &[
                    "generic",
                    "init",
                    "--name",
                    project,
                    "--path",
                    input.to_str().expect("临时输入路径应是 Unicode"),
                    "--source-language",
                    source_language,
                    "--target-language",
                    "zh-Hans",
                ],
            ),
        );
        assert_success(
            "Generic 逐字物化 Extract",
            &run_att(root, &["generic", "extract", "--name", project]),
        );
        assert_success(
            "Generic 逐字物化 Lua",
            &run_att(
                root,
                &[
                    "generic",
                    "lua",
                    "--name",
                    project,
                    script.to_str().expect("临时脚本路径应是 Unicode"),
                ],
            ),
        );

        let workspace = distribution.join("projects/generic").join(project);
        let logs_before = project_log_paths(&workspace.join("logs"));
        let write_back = run_att(root, &["generic", "write-back", "--name", project]);
        assert_success("Generic 逐字物化 WriteBack", &write_back);
        assert_plain_progress_lines(
            &write_back.stderr,
            &[
                "Planning document rewrites",
                "Publishing output",
                "Finalizing required resources",
            ],
        );
        assert_eq!(
            fs::read_to_string(workspace.join("write_back/settings.jsonl"))
                .expect("Generic 逐字物化输出应可读取"),
            "{\"id\":\"settings\",\"kind\":\"settings\",\"units\":[{\"id\":\"categories\",\"text\":\"常规、杂项、声音、开关\"}]}\n",
            "WriteBack 必须原样采用人工译文"
        );

        let new_logs = project_log_paths(&workspace.join("logs"))
            .difference(&logs_before)
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(new_logs.len(), 1, "一次 WriteBack 只能新增一份项目日志");
        let publication = fs::read_to_string(&new_logs[0])
            .expect("WriteBack 项目日志应可读取")
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).expect("项目日志行必须是 JSON")
            })
            .find(|record| record["event"] == "publication.finished")
            .expect("成功 WriteBack 必须记录 publication.finished");
        assert_eq!(publication["payload"]["result"]["kind"], "published");
        assert_eq!(
            publication["payload"]["result"]["summary"],
            serde_json::json!({
                "engine": "generic",
                "summary": {
                    "files": 1,
                    "translated_units": 1,
                    "retained_source_units": 0,
                },
            }),
            "项目日志只保存实际物化结果"
        );
    }
}

#[test]
fn generic_client_switch_preserves_translation_and_language_change_only_makes_it_unpublishable() {
    const PROJECT: &str = "generic-client-switch";
    const SOURCE: &str = "こんにちは";
    const TRANSLATION: &str = "已接受译文";

    let temporary = tempfile::tempdir().expect("应可建立 Client 切换进程测试目录");
    let root = temporary.path();
    let input = root.join("input");
    fs::create_dir(&input).expect("应可建立 Client 切换输入目录");
    fs::write(
        input.join("story.jsonl"),
        format!(
            "{}\n",
            serde_json::json!({
                "id": "story",
                "kind": "dialogue",
                "units": [{"id": "line", "text": SOURCE}],
            })
        ),
    )
    .expect("应可写入 Client 切换 Generic JSONL");

    let client_a = TcpListener::bind(("127.0.0.1", 0)).expect("Client A 模型服务应可绑定");
    let client_a_endpoint = format!(
        "http://{}/v1/chat/completions",
        client_a.local_addr().expect("Client A 地址应可读取")
    );
    write_client_switch_distribution(root, "client_a", &client_a_endpoint, "model-a");
    assert_success(
        "Client 切换 Generic Init",
        &run_att(
            root,
            &[
                "generic",
                "init",
                "--name",
                PROJECT,
                "--path",
                input.to_str().expect("临时输入路径应是 Unicode"),
                "--source-language",
                "ja",
                "--target-language",
                "zh-Hans",
            ],
        ),
    );
    assert_success(
        "Client 切换 Generic Extract",
        &run_att(root, &["generic", "extract", "--name", PROJECT]),
    );

    let client_a_server = thread::spawn(move || serve_chat_completion(client_a, TRANSLATION));
    assert_success(
        "Client A Generic Translate",
        &run_att(root, &["generic", "translate", "--name", PROJECT, "local"]),
    );
    client_a_server
        .join()
        .expect("Client A 模型服务不得 panic")
        .expect("Client A 模型服务必须完成请求");

    let workspace = distribution_root(root)
        .join("projects/generic")
        .join(PROJECT);
    let database = workspace.join("project.db");
    assert_eq!(
        read_single_generic_automatic_translation(&database),
        (Some(TRANSLATION.to_owned()), true),
        "Client A 的已验收译文必须保存为可识别的自动译文状态"
    );
    assert_success(
        "Client A Generic WriteBack",
        &run_att(root, &["generic", "write-back", "--name", PROJECT]),
    );
    assert_eq!(
        read_single_jsonl_group(&workspace.join("write_back/story.jsonl"))["units"][0]["text"],
        TRANSLATION
    );

    let client_b = TcpListener::bind(("127.0.0.1", 0)).expect("Client B spy 应可绑定");
    client_b
        .set_nonblocking(true)
        .expect("Client B spy 应可设为非阻塞");
    let client_b_endpoint = format!(
        "http://{}/v1/chat/completions",
        client_b.local_addr().expect("Client B 地址应可读取")
    );
    write_client_switch_distribution(root, "client_b", &client_b_endpoint, "model-b");

    assert_success(
        "Client B 配置下 Generic WriteBack",
        &run_att(root, &["generic", "write-back", "--name", PROJECT]),
    );
    assert_eq!(
        read_single_jsonl_group(&workspace.join("write_back/story.jsonl"))["units"][0]["text"],
        TRANSLATION,
        "WriteBack 必须使用已接受正文，不得用未来请求的 Client 重新判断它"
    );
    let no_work = run_att(root, &["generic", "translate", "--name", PROJECT, "local"]);
    assert_success("Client B 配置下无工作 Translate", &no_work);
    assert!(
        matches!(client_b.accept(), Err(error) if error.kind() == ErrorKind::WouldBlock),
        "单纯更换 Client 不得为已验收译文发出新请求"
    );
    assert_eq!(
        read_single_generic_automatic_translation(&database),
        (Some(TRANSLATION.to_owned()), true),
        "单纯更换 Client 后无工作 Translate 不得改写已接受译文"
    );
    drop(client_b);

    assert_success(
        "Generic 目标语言变化 Init",
        &run_att(
            root,
            &[
                "generic",
                "init",
                "--name",
                PROJECT,
                "--target-language",
                "en",
            ],
        ),
    );
    assert_eq!(
        read_single_generic_automatic_translation(&database),
        (Some(TRANSLATION.to_owned()), true),
        "目标语言变化应保留旧译文正文，但不能因保留而继续发布"
    );

    let failing_client = TcpListener::bind(("127.0.0.1", 0)).expect("失败 Client 模型服务应可绑定");
    let failing_endpoint = format!(
        "http://{}/v1/chat/completions",
        failing_client
            .local_addr()
            .expect("失败 Client 地址应可读取")
    );
    write_client_switch_distribution(root, "client_c", &failing_endpoint, "model-c");
    let failing_server = thread::spawn(move || serve_authentication_failure(failing_client));
    let failed = run_att(root, &["generic", "translate", "--name", PROJECT, "local"]);
    assert_eq!(
        failed.status.code(),
        Some(1),
        "旧语言译文需要替换时，永久认证错误必须使 Translate 失败"
    );
    failing_server
        .join()
        .expect("失败 Client 模型服务不得 panic")
        .expect("失败 Client 模型服务必须完成请求");
    assert_eq!(
        read_single_generic_automatic_translation(&database),
        (Some(TRANSLATION.to_owned()), true),
        "新译文请求失败不得先删除可供追溯和后续处理的旧正文"
    );

    assert_success(
        "目标语言变化后 Generic WriteBack",
        &run_att(root, &["generic", "write-back", "--name", PROJECT]),
    );
    assert_eq!(
        read_single_jsonl_group(&workspace.join("write_back/story.jsonl"))["units"][0]["text"],
        SOURCE,
        "旧目标语言的译文虽保留在数据库中，WriteBack 也不得把它发布到新语言输出"
    );
}

#[test]
fn generic_lua_can_reopen_project_database_after_att_schema_is_destroyed() {
    let temporary = tempfile::tempdir().expect("应可建立 Generic Lua 破坏性测试目录");
    let root = temporary.path();
    let input = root.join("input");
    fs::create_dir(&input).expect("应可建立 Generic Lua 输入目录");
    fs::write(
        input.join("story.jsonl"),
        "{\"id\":\"story\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"line\",\"text\":\"原文\"}]}\n",
    )
    .expect("应可写入 Generic Lua 输入");

    let distribution = distribution_root(root);
    fs::create_dir_all(&distribution).expect("应可建立测试发行目录");
    fs::write(distribution.join("config.toml"), LUA_LANGUAGE_CONFIGURATION)
        .expect("Lua 高级 API 语言配置应可写入");
    let project = "generic-lua-destroyed-schema";
    assert_success(
        "Generic Lua 破坏性测试 Init",
        &run_att(
            root,
            &[
                "generic",
                "init",
                "--name",
                project,
                "--path",
                input.to_str().expect("临时输入路径应是 Unicode"),
                "--source-language",
                "ja",
                "--target-language",
                "zh-Hans",
            ],
        ),
    );
    assert_success(
        "Generic Lua 破坏性测试 Extract",
        &run_att(root, &["generic", "extract", "--name", project]),
    );

    let destroy = root.join("destroy.lua");
    fs::write(&destroy, "ctx.db.execute(\"DROP TABLE generic_unit\")\n")
        .expect("应可写入破坏 schema 的 Lua");
    assert_success(
        "Generic Lua 删除 ATT 表",
        &run_att(
            root,
            &[
                "generic",
                "lua",
                "--name",
                project,
                destroy.to_str().expect("临时脚本路径应是 Unicode"),
            ],
        ),
    );

    let reopen = root.join("reopen.lua");
    fs::write(
        &reopen,
        concat!(
            "local missing = ctx.db.query(\"SELECT count(*) FROM sqlite_schema ",
            "WHERE type = 'table' AND name = 'generic_unit'\")\n",
            "assert(missing[1][1] == 0)\n",
            "ctx.db.execute(\"CREATE TABLE lua_after_damage(value TEXT)\")\n",
            "ctx.db.execute(\"INSERT INTO lua_after_damage VALUES ('kept')\")\n",
        ),
    )
    .expect("应可写入重开数据库的 Lua");
    assert_success(
        "Generic Lua 破坏后重开 project.db",
        &run_att(
            root,
            &[
                "generic",
                "lua",
                "--name",
                project,
                reopen.to_str().expect("临时脚本路径应是 Unicode"),
            ],
        ),
    );

    let database = distribution
        .join("projects/generic")
        .join(project)
        .join("project.db");
    let marker: String = Connection::open(database)
        .expect("应可直接打开被破坏的 project.db")
        .query_row("SELECT value FROM lua_after_damage", [], |row| row.get(0))
        .expect("第二次 Lua 应已写入标记");
    assert_eq!(marker, "kept");
}

#[test]
fn generic_missing_text_capture_reports_exact_leaf_without_model_request_or_state_change() {
    let temporary = tempfile::tempdir().expect("应可建立 MissingTextCapture 进程测试目录");
    let root = temporary.path();
    let input = root.join("input");
    fs::create_dir(&input).expect("应可建立 MissingTextCapture 输入目录");
    fs::write(
        input.join("story.jsonl"),
        format!(
            "{}\n",
            serde_json::json!({
                "id": "scene",
                "kind": "dialogue",
                "units": [{
                    "id": "broken-unit",
                    "text": MISSING_CAPTURE_SOURCE,
                }],
            })
        ),
    )
    .expect("应可写入 MissingTextCapture Generic JSONL");

    let placeholders = root.join("external-placeholders.toml");
    fs::write(
        &placeholders,
        concat!(
            "[[rule]]\n",
            "scopes = [\"dialogue\"]\n",
            "order = \"preserve\"\n",
            "pattern = '(?:(?<text>保留)|触发缺组)'\n",
        ),
    )
    .expect("应可写入缺少 text 捕获的外部 Placeholder 规则");

    let provider = TcpListener::bind(("127.0.0.1", 0)).expect("本地 Provider 端口应可绑定");
    provider
        .set_nonblocking(true)
        .expect("Provider spy 应可设为非阻塞");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        provider.local_addr().expect("本地 Provider 地址应可读取")
    );
    write_missing_capture_distribution(root, &endpoint);

    let input_argument = input.to_str().expect("临时输入路径应是 Unicode");
    assert_success(
        "MissingTextCapture Generic Init",
        &run_att(
            root,
            &[
                "generic",
                "init",
                "--name",
                MISSING_CAPTURE_PROJECT,
                "--path",
                input_argument,
                "--source-language",
                "ja",
                "--target-language",
                "zh-Hans",
            ],
        ),
    );
    assert_success(
        "MissingTextCapture Generic Extract",
        &run_att(
            root,
            &["generic", "extract", "--name", MISSING_CAPTURE_PROJECT],
        ),
    );

    let workspace = distribution_root(root)
        .join("projects/generic")
        .join(MISSING_CAPTURE_PROJECT);
    let database = workspace.join("project.db");
    let database_before = read_sqlite_logical_snapshot(&database);
    let logs_before = project_log_paths(&workspace.join("logs"));

    let placeholder_argument = placeholders
        .to_str()
        .expect("临时 Placeholder 路径应是 Unicode");
    let translate = run_att(
        root,
        &[
            "generic",
            "translate",
            "--name",
            MISSING_CAPTURE_PROJECT,
            "local",
            "--placeholders",
            placeholder_argument,
        ],
    );
    assert_eq!(
        translate.status.code(),
        Some(1),
        "MissingTextCapture 必须是普通失败\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&translate.stdout),
        String::from_utf8_lossy(&translate.stderr)
    );

    let mut provider_connections = 0_u64;
    loop {
        match provider.accept() {
            Ok((_stream, _address)) => provider_connections += 1,
            Err(error) if error.kind() == ErrorKind::WouldBlock => break,
            Err(error) => panic!("读取 Provider spy 连接失败：{error}"),
        }
    }
    assert_eq!(
        provider_connections, 0,
        "源文 Placeholder 规划失败前不得建立任何模型连接"
    );
    assert_eq!(
        read_sqlite_logical_snapshot(&database),
        database_before,
        "MissingTextCapture 规划失败不得改变任何 SQLite 逻辑业务状态"
    );
    assert!(
        !workspace.join("task-records").exists(),
        "零模型请求的规划失败不得建立任务记录"
    );

    let stderr = String::from_utf8(translate.stderr).expect("诊断 stderr 必须是 UTF-8");
    assert!(
        stderr.contains("story.jsonl:line1:unit1:text"),
        "stderr 必须指出实际失败 Unit：{stderr}"
    );
    assert!(stderr.contains("Reason:") && stderr.contains("Action:"));
    for internal in [
        "translation.placeholder",
        "relative_path=",
        "group_id=",
        "unit_id=",
        "role=",
        "rule_number=",
        "match_range=",
    ] {
        assert!(
            !stderr.contains(internal),
            "公开诊断不得显示内部字段 {internal:?}：{stderr}"
        );
    }
    for private in [MISSING_CAPTURE_SOURCE, MISSING_CAPTURE_API_KEY] {
        assert!(
            !stderr.contains(private),
            "stderr 不得泄露受保护内容 {private:?}：{stderr}"
        );
    }

    let logs_after = project_log_paths(&workspace.join("logs"));
    let new_logs = logs_after
        .difference(&logs_before)
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        new_logs.len(),
        1,
        "一次 Translate 必须新增且只新增一份 RunId 项目日志：{new_logs:?}"
    );
    let log_text = fs::read_to_string(&new_logs[0]).expect("Translate 项目日志应可读取");
    for private in [MISSING_CAPTURE_SOURCE, MISSING_CAPTURE_API_KEY] {
        assert!(
            !log_text.contains(private),
            "项目日志不得泄露受保护内容 {private:?}"
        );
    }
    let records = log_text
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("项目日志行必须是 JSON"))
        .collect::<Vec<_>>();
    assert!(!records.is_empty(), "Translate 项目日志不得为空");
    for record in &records {
        let fields = record
            .as_object()
            .expect("项目日志顶层必须是 object")
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            fields,
            BTreeSet::from([
                "timestamp",
                "sequence",
                "run_id",
                "level",
                "event",
                "context",
                "payload",
                "message",
            ]),
            "项目日志顶层必须使用唯一现行契约：{record}"
        );
        assert_eq!(record["context"]["locale"], "en");
        assert_eq!(record["context"]["engine"], "generic");
        assert_eq!(record["context"]["project"], MISSING_CAPTURE_PROJECT);
        assert_eq!(record["context"]["command"], "translate");
    }

    let diagnostic_records = records
        .iter()
        .filter(|record| record["event"] == "diagnostic.run_plan")
        .collect::<Vec<_>>();
    assert_eq!(
        diagnostic_records.len(),
        1,
        "MissingTextCapture 必须形成一条 RunPlan 诊断：{log_text}"
    );
    let diagnostic = &diagnostic_records[0]["payload"];
    assert_eq!(diagnostic_records[0]["level"], "error");
    let diagnostic_fields = diagnostic.as_object().expect("公开诊断 payload 必须是对象");
    assert_eq!(
        diagnostic_fields
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["relation", "object", "reason", "impact", "help"])
    );
    assert_eq!(diagnostic["relation"], "primary");
    for field in ["object", "reason", "impact", "help"] {
        assert!(
            diagnostic[field]
                .as_str()
                .is_some_and(|value| !value.is_empty()),
            "诊断 {field} 必须是非空可读文本：{diagnostic}"
        );
    }
    let diagnostic_text = serde_json::to_string(diagnostic).expect("诊断必须可序列化");
    for internal in [
        "occurrence",
        "report",
        "effect",
        "stage",
        "issue",
        "resolution",
        "group_id",
        "unit_id",
        "match_range",
    ] {
        assert!(!diagnostic_text.contains(internal));
    }

    assert!(
        records
            .iter()
            .all(|record| record["event"] != "task.started"),
        "规划失败不得声明模型 Task 已开始：{log_text}"
    );
    assert!(
        records
            .iter()
            .all(|record| record["event"] != "retry.summary"),
        "零模型请求不得产生 Retry 汇总：{log_text}"
    );
    assert!(
        records.iter().all(|record| {
            record["event"] != "phase.completed" || record["payload"]["phase"] != "planning"
        }),
        "失败的 planning phase 不得伪造 completed：{log_text}"
    );
    let stopped = records
        .iter()
        .filter(|record| {
            record["event"] == "phase.stopped" && record["payload"]["phase"] == "planning"
        })
        .collect::<Vec<_>>();
    assert_eq!(
        stopped.len(),
        1,
        "失败 planning 必须恰好停止一次：{log_text}"
    );
    assert!(stopped[0]["payload"]["outcome"].get("diagnostic").is_none());
    let translation_finished = records
        .iter()
        .filter(|record| record["event"] == "translation.finished")
        .collect::<Vec<_>>();
    assert_eq!(
        translation_finished.len(),
        1,
        "每次 Translate 必须恰好产生一个翻译终态：{log_text}"
    );
    assert_eq!(
        translation_finished[0]["payload"]["result"]["kind"],
        "failed"
    );
    assert!(
        translation_finished[0]["payload"]["result"]
            .get("diagnostic")
            .is_none()
    );
}

fn write_missing_capture_distribution(root: &Path, endpoint: &str) {
    let distribution = distribution_root(root);
    fs::create_dir_all(&distribution).expect("MissingTextCapture 发行目录应可建立");
    fs::write(
        distribution.join("config.toml"),
        format!(
            r#"[prompts]
thinking_output = true
source_echo = false

[llm.clients.local]
url = "{endpoint}"
api_key = "{MISSING_CAPTURE_API_KEY}"
model = "unused-model"
stream = false
max_concurrent_requests = 1
connect_timeout_ms = 1000
read_timeout_ms = 1000
request_timeout_ms = 1000
proxy = false
additional_pem_files = []
retry_delays_ms = [1]
max_retry_after_ms = 1
parameters = '''
{{}}
'''

[[languages]]
type = "japanese"
id = "ja"
minimum_kana_characters = 1
allowed_terms = []

[translation]

[[translation.profiles]]
id = "local"
llm_client = "local"
target_task_user_message_characters = 10000
"#
        ),
    )
    .expect("MissingTextCapture 测试配置应可写入");
    let prompt_root = distribution.join("prompts/translation");
    fs::create_dir_all(prompt_root.join("rules")).expect("Prompt 规则目录应可建立");
    fs::create_dir_all(prompt_root.join("examples")).expect("Prompt 示例目录应可建立");
    fs::write(
        prompt_root.join("system.md"),
        "把 {{source_language}} 翻译成 {{target_language}}。",
    )
    .expect("system Prompt 应可写入");
    fs::write(
        prompt_root.join("thinking.md"),
        "在 think 中写出影响译文的判断。",
    )
    .expect("Thinking Prompt 应可写入");
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

fn write_client_switch_distribution(root: &Path, client: &str, endpoint: &str, model: &str) {
    let distribution = distribution_root(root);
    fs::create_dir_all(&distribution).expect("Client 切换测试发行目录应可建立");
    fs::write(
        distribution.join("config.toml"),
        format!(
            r#"[prompts]
thinking_output = true
source_echo = false

[llm.clients.{client}]
url = "{endpoint}"
api_key = "client-switch-secret"
model = "{model}"
stream = false
max_concurrent_requests = 1
connect_timeout_ms = 1000
read_timeout_ms = 1000
request_timeout_ms = 1000
proxy = false
additional_pem_files = []
retry_delays_ms = []
max_retry_after_ms = 1
parameters = '''
{{}}
'''

[[languages]]
type = "japanese"
id = "ja"
minimum_kana_characters = 1
allowed_terms = []

[translation]
record_translation_tasks = false

[[translation.profiles]]
id = "local"
llm_client = "{client}"
target_task_user_message_characters = 10000
"#,
        ),
    )
    .expect("Client 切换测试配置应可写入");
    let prompt_root = distribution.join("prompts/translation");
    fs::create_dir_all(prompt_root.join("rules")).expect("Prompt 规则目录应可建立");
    fs::create_dir_all(prompt_root.join("examples")).expect("Prompt 示例目录应可建立");
    fs::write(
        prompt_root.join("system.md"),
        "把 {{source_language}} 翻译成 {{target_language}}。",
    )
    .expect("system Prompt 应可写入");
    fs::write(
        prompt_root.join("thinking.md"),
        "在 think 中写出影响译文的判断。",
    )
    .expect("Thinking Prompt 应可写入");
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

fn project_log_paths(directory: &Path) -> BTreeSet<PathBuf> {
    fs::read_dir(directory)
        .expect("项目日志目录应可读取")
        .map(|entry| entry.expect("项目日志项应可读取").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect()
}

fn read_single_generic_automatic_translation(database: &Path) -> (Option<String>, bool) {
    Connection::open(database)
        .expect("Generic 项目数据库应可打开")
        .query_row(
            "SELECT translation, translation_state IS NOT NULL FROM generic_unit",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("Generic 自动译文应可读取")
}

fn read_sqlite_logical_snapshot(database: &Path) -> BTreeMap<String, Vec<Vec<String>>> {
    let connection = Connection::open(database).expect("项目数据库应可打开");
    let tables = {
        let mut statement = connection
            .prepare(
                "SELECT name FROM sqlite_schema
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
                 ORDER BY name",
            )
            .expect("逻辑快照数据表查询应可准备");
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("逻辑快照数据表查询应可执行")
            .collect::<Result<Vec<_>, _>>()
            .expect("逻辑快照数据表应可读取")
    };
    let mut snapshot = BTreeMap::new();
    for table in tables {
        let table_identifier = quote_sql_identifier(&table);
        let columns = {
            let mut statement = connection
                .prepare(&format!("PRAGMA table_info({table_identifier})"))
                .expect("逻辑快照列查询应可准备");
            statement
                .query_map([], |row| row.get::<_, String>(1))
                .expect("逻辑快照列查询应可执行")
                .collect::<Result<Vec<_>, _>>()
                .expect("逻辑快照列应可读取")
        };
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

fn quote_sql_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn read_manual_toml(path: &Path) -> toml::Value {
    toml::from_str(&fs::read_to_string(path).expect("Manual TOML 应可读取"))
        .expect("Manual TOML 应可解析")
}

fn find_manual_entry<'a>(entries: &'a [toml::Value], id: &str) -> &'a toml::Value {
    entries
        .iter()
        .find(|entry| entry["id"].as_str() == Some(id))
        .unwrap_or_else(|| panic!("Manual 应包含 {id}"))
}

fn set_manual_toml_field(path: &Path, id: &str, field: &str, value: toml::Value) {
    let mut document = read_manual_toml(path);
    let entries = document["translation"]
        .as_array_mut()
        .expect("Manual translation 必须是数组");
    let entry = entries
        .iter_mut()
        .find(|entry| entry["id"].as_str() == Some(id))
        .unwrap_or_else(|| panic!("Manual 应包含 {id}"));
    entry
        .as_table_mut()
        .expect("Manual 条目必须是 table")
        .insert(field.to_owned(), value);
    fs::write(
        path,
        toml::to_string_pretty(&document).expect("Manual TOML 应可编码"),
    )
    .expect("Manual TOML 应可写入");
}

fn read_single_jsonl_group(path: &Path) -> serde_json::Value {
    let text = fs::read_to_string(path).expect("Generic WriteBack JSONL 应可读取");
    let mut lines = text.lines();
    let value = serde_json::from_str(lines.next().expect("Generic JSONL 应包含一行"))
        .expect("Generic JSONL 行应可解析");
    assert!(lines.next().is_none(), "Generic JSONL 应只包含一行");
    value
}

fn serve_chat_completion(listener: TcpListener, translation: &str) -> Result<(), String> {
    let (mut stream, _) = listener
        .accept()
        .map_err(|error| format!("接受模型请求失败：{error}"))?;
    read_http_json(&mut stream)?;
    let content = serde_json::json!({
        "think": "已核对用户结果",
        "translations": {"0": [translation]},
    })
    .to_string();
    let body = serde_json::json!({
        "id": "client-switch-response",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 3, "total_tokens": 13},
    })
    .to_string();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .map_err(|error| format!("写入模型响应失败：{error}"))?;
    stream
        .flush()
        .map_err(|error| format!("刷新模型响应失败：{error}"))
}

fn serve_authentication_failure(listener: TcpListener) -> Result<(), String> {
    let (mut stream, _) = listener
        .accept()
        .map_err(|error| format!("接受认证失败请求失败：{error}"))?;
    read_http_json(&mut stream)?;
    let body = serde_json::json!({
        "error": {
            "code": "invalid_api_key",
            "type": "authentication_error",
            "message": "invalid credentials",
        }
    })
    .to_string();
    write!(
        stream,
        "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .map_err(|error| format!("写入认证失败响应失败：{error}"))?;
    stream
        .flush()
        .map_err(|error| format!("刷新认证失败响应失败：{error}"))
}

fn read_http_json(stream: &mut TcpStream) -> Result<serde_json::Value, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| format!("设置 HTTP 请求读取超时失败：{error}"))?;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];
    let header_end = loop {
        let read = stream
            .read(&mut buffer)
            .map_err(|error| format!("读取 HTTP 请求失败：{error}"))?;
        if read == 0 {
            return Err("HTTP 请求在 header 完成前结束".to_owned());
        }
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(position) = find_subslice(&bytes, b"\r\n\r\n") {
            break position + 4;
        }
    };
    let header = std::str::from_utf8(&bytes[..header_end - 4])
        .map_err(|error| format!("HTTP header 不是 UTF-8：{error}"))?;
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
            .map_err(|error| format!("读取 HTTP 请求 body 失败：{error}"))?;
        if read == 0 {
            return Err("HTTP 请求 body 提前结束".to_owned());
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    serde_json::from_slice(&bytes[header_end..header_end + content_length])
        .map_err(|error| format!("HTTP 请求 body 必须是 JSON：{error}"))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn run_att(root: &Path, arguments: &[&str]) -> Output {
    Command::new(stage_att_executable(root))
        .current_dir(root)
        .args(["--ui-language", "en"])
        .args(arguments)
        .output()
        .expect("att.exe 应可执行")
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

fn assert_success(stage: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{stage} 应成功\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_plain_progress_lines(stderr: &[u8], expected: &[&str]) -> String {
    let text = String::from_utf8(stderr.to_vec()).expect("进度 stderr 必须是 UTF-8");
    assert_plain_progress_text(&text, expected);
    text
}

fn assert_plain_progress_text(text: &str, expected: &[&str]) {
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
