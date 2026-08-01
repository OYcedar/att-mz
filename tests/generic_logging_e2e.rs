#![cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]

//! Generic 已有工作区命令的项目日志建立时机测试。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[test]
fn generic_unavailable_and_project_open_failures_are_persisted_without_task_records() {
    let temporary = tempfile::tempdir().expect("应可建立 Generic 日志进程测试目录");
    let root = temporary.path();
    let distribution = root.join("release");
    let input = root.join("input");
    fs::create_dir_all(&distribution).expect("应可建立测试发行目录");
    fs::create_dir(&input).expect("应可建立 Generic 输入目录");
    fs::write(distribution.join("config.toml"), test_configuration()).expect("应可写入测试配置");
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
        "{\"id\":\"story\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"line\",\"text\":\"こんにちは\"}]}\n",
    )
    .expect("应可写入 Generic JSONL");

    let project = "generic-open-failure-log";
    let init = run_att(
        root,
        &[
            "generic",
            "init",
            "--name",
            project,
            "--path",
            input.to_str().expect("临时路径应是 Unicode"),
            "--source-language",
            "ja",
            "--target-language",
            "zh-Hans",
        ],
    );
    assert_success("Generic Init", &init);

    let workspace = distribution.join("projects/generic").join(project);
    let logs_root = workspace.join("logs");
    assert_success(
        "Generic Extract",
        &run_att(root, &["generic", "extract", "--name", project]),
    );

    let logs_before_unavailable = jsonl_files(&logs_root);
    let unavailable = run_att(root, &["generic", "translate", "--name", project, "local"]);
    assert_success("Generic unavailable Translate", &unavailable);
    let unavailable_logs = jsonl_files(&logs_root)
        .into_iter()
        .filter(|path| !logs_before_unavailable.contains(path))
        .collect::<Vec<_>>();
    assert_eq!(
        unavailable_logs.len(),
        1,
        "Unavailable Translate 必须建立恰好一份新运行日志"
    );
    let unavailable_log =
        fs::read_to_string(&unavailable_logs[0]).expect("Unavailable 运行日志必须可读取");
    assert!(
        unavailable_log.contains("\"code\":\"task.diagnostic\"")
            && unavailable_log.contains("\"code\":\"model.request\"")
            && unavailable_log.contains("\"outcome\":\"unavailable\""),
        "任务记录关闭时，Unavailable 的模型错误仍必须进入项目 JSONL：{unavailable_log}"
    );
    assert!(
        !workspace.join("task-records").exists(),
        "显式关闭任务记录的测试不得依赖 Markdown 保存诊断"
    );

    let logs_before_failure = jsonl_files(&logs_root);
    let database = workspace.join("project.db");
    fs::rename(&database, workspace.join("project.db.saved")).expect("应可暂存 Generic 项目数据库");
    fs::create_dir(&database).expect("应可用错误类型制造项目打开失败");

    let failed = run_att(root, &["generic", "extract", "--name", project]);
    assert_eq!(
        failed.status.code(),
        Some(1),
        "错误类型的项目数据库必须让 Extract 失败：{}",
        String::from_utf8_lossy(&failed.stderr)
    );
    let new_logs = jsonl_files(&logs_root)
        .into_iter()
        .filter(|path| !logs_before_failure.contains(path))
        .collect::<Vec<_>>();
    assert_eq!(new_logs.len(), 1, "项目打开失败必须建立恰好一份新运行日志");
    let failure_log = fs::read_to_string(&new_logs[0]).expect("失败运行日志必须可读取");
    assert!(
        failure_log.contains("\"code\":\"failure.reported\"")
            && failure_log.contains("\"command\":\"extract\"")
            && failure_log.contains("\"stage\":\"project_opening\""),
        "项目打开失败必须把命令、阶段和结构化原因写入本次 JSONL：{failure_log}"
    );
}

fn test_configuration() -> &'static str {
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
record_translation_tasks = false

[[translation.profiles]]
id = "local"
llm_client = "local"
target_task_user_message_characters = 10000
"#
}

fn run_att(root: &Path, arguments: &[&str]) -> Output {
    Command::new(stage_att_executable(root))
        .current_dir(root)
        .args(["--ui-language", "en", "--progress", "off"])
        .args(arguments)
        .output()
        .expect("att.exe 应可执行")
}

fn stage_att_executable(root: &Path) -> PathBuf {
    let source = Path::new(env!("CARGO_BIN_EXE_att"));
    let release = root.join("release");
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

fn jsonl_files(root: &Path) -> Vec<PathBuf> {
    let mut files = fs::read_dir(root)
        .expect("项目日志目录必须可读取")
        .map(|entry| entry.expect("项目日志目录项必须可读取").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn assert_success(stage: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{stage} 应成功\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
