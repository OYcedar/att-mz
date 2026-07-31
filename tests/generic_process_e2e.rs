#![cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]

//! Generic CLI 的独立生产进程边界测试。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const PROJECT: &str = "generic-observable";

#[test]
fn generic_progress_modes_and_jsonl_diagnostic_are_observable() {
    let temporary = tempfile::tempdir().expect("应可建立 Generic 进程测试目录");
    let root = temporary.path();
    let input = root.join("input");
    fs::create_dir(&input).expect("应可建立 Generic 输入目录");
    let distribution = distribution_root(root);
    fs::create_dir_all(&distribution).expect("测试发行目录应可建立");
    fs::write(
        distribution.join("config.toml"),
        r#"[prompts]
locale = "en"
thinking_output = false

[llm.clients.local]
url = "http://127.0.0.1:9/v1/chat/completions"
api_key = "unused-test-secret"
model = "unused-test-model"
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
quote_repair_pairs = [["“", "”"], ["‘", "’"]]

[translation]
record_translation_tasks = false

[[translation.profiles]]
id = "local"
llm_client = "local"
target_task_user_message_characters = 10000
"#,
    )
    .expect("应可写入测试配置");
    let prompt_root = distribution.join("prompts/generic/en");
    fs::create_dir_all(&prompt_root).expect("应可建立 Generic Prompt 目录");
    fs::write(
        prompt_root.join("system.md"),
        "Translate {{source_language}} into {{target_language}}. Return string values.",
    )
    .expect("应可写入 Generic system Prompt");
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
        "plain",
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
    assert_stderr_contains(&init, "Initializing the Generic project");

    let extract = run_att(root, "plain", &["generic", "extract", "--name", PROJECT]);
    assert_success("Generic Extract", &extract);
    assert_stderr_contains(&extract, "Scanning Generic JSONL input");

    let lua_script = root.join("noop.lua");
    fs::write(&lua_script, "return\n").expect("应可写入 Lua 脚本");
    let lua = run_att(
        root,
        "plain",
        &[
            "generic",
            "lua",
            "--name",
            PROJECT,
            lua_script.to_str().expect("临时路径应是 Unicode"),
        ],
    );
    assert_success("Generic Lua", &lua);
    assert_stderr_contains(&lua, "Running the project Lua program");

    let write_back = run_att(root, "plain", &["generic", "write-back", "--name", PROJECT]);
    assert_success("Generic WriteBack", &write_back);
    assert_stderr_contains(&write_back, "Planning document rewrites");
    assert_stderr_contains(&write_back, "Publishing output");
    let write_back_progress = String::from_utf8_lossy(&write_back.stderr);
    let preparing = write_back_progress
        .find("Planning document rewrites")
        .expect("WriteBack 必须先报告候选准备");
    let publishing = write_back_progress
        .find("Publishing output")
        .expect("WriteBack 必须在进入目录发布前报告发布阶段");
    assert!(
        preparing < publishing,
        "WriteBack 必须先准备和复查候选，再报告发布：{write_back_progress}"
    );

    let empty_input = root.join("empty-input");
    fs::create_dir(&empty_input).expect("应可建立空 Generic 输入目录");
    let no_work_project = "generic-no-work";
    assert_success(
        "空 Generic Init",
        &run_att(
            root,
            "off",
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
        &run_att(
            root,
            "off",
            &["generic", "extract", "--name", no_work_project],
        ),
    );
    let translate = run_att(
        root,
        "plain",
        &["generic", "translate", "--name", no_work_project, "local"],
    );
    assert_success("无请求 Generic Translate", &translate);
    assert_stderr_contains(&translate, "Planning translation tasks");
    assert_stderr_contains(&translate, "No model request is needed");

    for mode in ["auto", "off"] {
        let output = run_att(root, mode, &["generic", "extract", "--name", PROJECT]);
        assert_success("静默 Generic Extract", &output);
        assert!(
            output.stderr.is_empty(),
            "{mode} 在非 TTY 或显式关闭时不得输出实时进度：{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

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
    let invalid = run_att(root, "off", &["generic", "extract", "--name", PROJECT]);
    assert_eq!(invalid.status.code(), Some(1));
    let stderr = String::from_utf8(invalid.stderr).expect("stderr 必须是 UTF-8");
    assert!(
        stderr.contains("bad.jsonl"),
        "诊断必须指出损坏文件：{stderr}"
    );
    assert!(
        stderr.contains("line=1") || stderr.contains(":1"),
        "诊断必须指出损坏行号：{stderr}"
    );
    assert!(
        stderr.contains("Generic JSONL source document"),
        "诊断必须使用 Generic JSONL 文案：{stderr}"
    );
    assert!(
        stderr.contains("json_category=data")
            && stderr.contains("json_line=1")
            && stderr.contains("json_column="),
        "诊断必须保留类型化 JSON 类别和解析坐标：{stderr}"
    );
    assert!(
        !stderr.contains(SENTINEL),
        "公开诊断不得包含原始 JSON 字段或 serde 自由文本：{stderr}"
    );
    assert!(
        !stderr.contains("RPG Maker"),
        "Generic JSONL 诊断不得复用 RPG Maker 文案：{stderr}"
    );
}

fn run_att(root: &Path, progress: &str, arguments: &[&str]) -> Output {
    Command::new(stage_att_executable(root))
        .current_dir(root)
        .args(["--ui-language", "en", "--progress", progress])
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

fn assert_stderr_contains(output: &Output, expected: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(expected),
        "stderr 必须包含 {expected:?}：{stderr}"
    );
}
