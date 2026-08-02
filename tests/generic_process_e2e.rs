#![cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]

//! Generic CLI 的独立生产进程边界测试。

use std::collections::BTreeSet;
use std::fs;
use std::io::ErrorKind;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const PROJECT: &str = "generic-observable";
const MISSING_CAPTURE_PROJECT: &str = "generic-missing-text-capture";
const MISSING_CAPTURE_API_KEY: &str = "PRIVATE_MISSING_CAPTURE_API_KEY";
const MISSING_CAPTURE_SOURCE: &str = "秘密本文あ甲触发缺组乙";

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
thinking_output = true
source_echo = false

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
    let degraded_translate = run_att(
        root,
        "off",
        &["generic", "translate", "--name", PROJECT, "local"],
    );
    assert_success("任务记录降级 Generic Translate", &degraded_translate);
    let degraded_stderr =
        String::from_utf8(degraded_translate.stderr).expect("stderr 必须是 UTF-8");
    assert_eq!(
        degraded_stderr
            .matches("Translation task records are unavailable or degraded")
            .count(),
        1,
        "Generic 任务记录故障必须恰好警告一次：{degraded_stderr}"
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
            if record["code"] == "diagnostic.task_record" {
                assert_eq!(record["run_id"], expected_run_id);
                assert_eq!(record["context"]["command"], "translate");
                assert_eq!(record["level"], "warn");
                observed_same_run_log = true;
            }
        }
    }
    assert!(
        observed_same_run_log,
        "Generic 任务记录故障必须写入同一 RunId 的 Translate JSONL"
    );

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
        stderr.contains("generic.jsonl.invalid_json") && stderr.contains("operation=parse_jsonl"),
        "诊断必须使用 Generic JSONL 的稳定错误码和操作：{stderr}"
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
            "off",
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
            "off",
            &["generic", "extract", "--name", MISSING_CAPTURE_PROJECT],
        ),
    );

    let workspace = distribution_root(root)
        .join("projects/generic")
        .join(MISSING_CAPTURE_PROJECT);
    let database = workspace.join("project.db");
    let database_before = fs::read(&database).expect("Translate 前数据库应可读取");
    let logs_before = project_log_paths(&workspace.join("logs"));

    let placeholder_argument = placeholders
        .to_str()
        .expect("临时 Placeholder 路径应是 Unicode");
    let translate = run_att(
        root,
        "off",
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
        fs::read(&database).expect("Translate 后数据库应可读取"),
        database_before,
        "MissingTextCapture 失败前后 SQLite 文件字节必须完全一致"
    );
    assert!(
        !workspace.join("task-records").exists(),
        "零模型请求的规划失败不得建立任务记录"
    );

    let match_text = "触发缺组";
    let match_start = MISSING_CAPTURE_SOURCE
        .find(match_text)
        .expect("测试源文必须包含 Placeholder 完整匹配");
    let match_end = match_start + match_text.len();
    let stderr = String::from_utf8(translate.stderr).expect("诊断 stderr 必须是 UTF-8");
    for expected in [
        "translation.placeholder.missing_text_capture".to_owned(),
        "Translation".to_owned(),
        placeholders.display().to_string(),
        "relative_path=story.jsonl".to_owned(),
        "group_id=scene".to_owned(),
        "unit_id=broken-unit".to_owned(),
        "role=dialogue".to_owned(),
        "rule_number=1".to_owned(),
        format!("match_range={match_start}..{match_end}"),
        "State was not changed".to_owned(),
        "Correct the indicated Placeholder rule and retry".to_owned(),
    ] {
        assert!(
            stderr.contains(&expected),
            "stderr 必须保留 MissingTextCapture 事实 {expected:?}：{stderr}"
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
                "code",
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
        .filter(|record| record["code"] == "diagnostic.run_plan")
        .collect::<Vec<_>>();
    assert_eq!(
        diagnostic_records.len(),
        1,
        "MissingTextCapture 必须形成一条原子 RunPlan occurrence：{log_text}"
    );
    let occurrence = &diagnostic_records[0]["payload"];
    assert_eq!(diagnostic_records[0]["level"], "error");
    assert!(
        occurrence["id"].as_u64().is_some(),
        "occurrence ID 必须是 RunId 内单调编号：{occurrence}"
    );
    assert_eq!(occurrence["scope"], "run_plan");
    assert_eq!(occurrence["report"]["effect"], "unchanged");
    assert_eq!(
        occurrence["report"]["primary"]["code"],
        "translation.placeholder.missing_text_capture"
    );
    assert_eq!(occurrence["report"]["primary"]["stage"], "translate");
    assert_eq!(
        occurrence["report"]["primary"]["resolution"],
        "fix_placeholder_rules"
    );
    assert_eq!(occurrence["report"]["related"], serde_json::json!([]));
    let issue = &occurrence["report"]["primary"]["issue"];
    assert_eq!(issue["family"], "translation");
    assert_eq!(issue["details"]["kind"], "placeholder");
    assert_eq!(
        issue["details"]["rule_source"],
        serde_json::json!({
            "kind": "external_file",
            "path": placeholders.display().to_string(),
        })
    );
    assert_eq!(
        issue["details"]["unit"],
        serde_json::json!({
            "relative_path": "story.jsonl",
            "group_id": "scene",
            "unit_id": "broken-unit",
            "role": "dialogue",
        })
    );
    assert_eq!(
        issue["details"]["problem"],
        serde_json::json!({
            "kind": "missing_text_capture",
            "rule_number": 1,
            "match_range": {
                "start": match_start,
                "end": match_end,
            },
        })
    );

    assert!(
        records
            .iter()
            .all(|record| record["code"] != "task.started"),
        "规划失败不得声明模型 Task 已开始：{log_text}"
    );
    assert!(
        records
            .iter()
            .all(|record| record["code"] != "retry.summary"),
        "零模型请求不得产生 Retry 汇总：{log_text}"
    );
    assert!(
        records.iter().all(|record| {
            record["code"] != "phase.completed" || record["payload"]["phase"] != "planning"
        }),
        "失败的 planning phase 不得伪造 completed：{log_text}"
    );
    let stopped = records
        .iter()
        .filter(|record| {
            record["code"] == "phase.stopped" && record["payload"]["phase"] == "planning"
        })
        .collect::<Vec<_>>();
    assert_eq!(
        stopped.len(),
        1,
        "失败 planning 必须恰好停止一次：{log_text}"
    );
    assert_eq!(
        stopped[0]["payload"]["outcome"]["diagnostic"],
        occurrence["id"]
    );
    let translation_finished = records
        .iter()
        .filter(|record| record["code"] == "translation.finished")
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
    assert_eq!(
        translation_finished[0]["payload"]["result"]["diagnostic"],
        occurrence["id"]
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
quote_repair_pairs = [["“", "”"], ["‘", "’"]]

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
