#![cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]

//! Generic 已有工作区命令的项目日志建立时机测试。

use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, WaitForSingleObject};

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
    let (unavailable_log, unavailable_records) = read_jsonl_records(&unavailable_logs[0]);
    assert!(
        unavailable_records
            .iter()
            .any(|record| record["event"] == "diagnostic.translation_task")
            && unavailable_records
                .iter()
                .any(|record| record["event"] == "task.finished"
                    && record["payload"]["outcome"]["kind"] == "unavailable")
            && unavailable_records
                .iter()
                .any(|record| record["event"] == "translation.finished"
                    && record["payload"]["result"]["kind"] == "incomplete"),
        "任务记录关闭时，Unavailable 的模型错误仍必须进入项目 JSONL：{unavailable_log}"
    );
    assert_log_has_only_readable_diagnostics(&unavailable_log, &unavailable_records);
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
    let (failure_log, failure_records) = read_jsonl_records(&new_logs[0]);
    let canonical_database = database
        .canonicalize()
        .expect("错误类型的项目数据库路径必须可规范化");
    let readable_database = readable_windows_path(&canonical_database);
    assert!(
        failure_records.iter().any(|record| {
            record["event"] == "diagnostic.extract"
                && record["context"]["command"] == "extract"
                && record["payload"]["object"] == readable_database
        }),
        "项目打开失败必须把命令、对象和处理方法写入本次 JSONL：{failure_log}"
    );
    assert_log_has_only_readable_diagnostics(&failure_log, &failure_records);
}

#[test]
fn generic_extract_succeeds_when_project_log_cannot_be_created_and_reports_readable_diagnostic() {
    let temporary = tempfile::tempdir().expect("应可建立项目日志建立失败进程测试目录");
    let root = temporary.path();
    let input = root.join("input");
    fs::create_dir(&input).expect("应可建立 Generic 输入目录");
    fs::write(
        input.join("story.jsonl"),
        "{\"id\":\"story\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"line\",\"text\":\"こんにちは\"}]}\n",
    )
    .expect("应可写入 Generic JSONL");

    let distribution = write_distribution(root, test_configuration());
    let project = "generic-log-create-failure";
    assert_success(
        "日志建立失败前的 Generic Init",
        &run_att(
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
        ),
    );
    assert_success(
        "日志建立失败前的 Generic Extract",
        &run_att(root, &["generic", "extract", "--name", project]),
    );

    let workspace = distribution.join("projects/generic").join(project);
    let logs_root = workspace.join("logs");
    fs::rename(&logs_root, workspace.join("logs.saved")).expect("应可暂存既有日志目录");
    fs::write(&logs_root, b"not a directory").expect("应可用普通文件阻止日志目录建立");
    fs::write(
        input.join("extra.jsonl"),
        "{\"id\":\"extra\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"line\",\"text\":\"追加\"}]}\n",
    )
    .expect("应可增加待提取的 Generic JSONL");
    let database = workspace.join("project.db");
    let database_before = fs::read(&database).expect("Extract 前数据库必须可读取");

    let extract = run_att(root, &["generic", "extract", "--name", project]);
    assert_success("项目日志无法建立时的 Generic Extract", &extract);
    let database_after = fs::read(&database).expect("Extract 后数据库必须可读取");
    assert_ne!(
        database_after, database_before,
        "日志建立失败不得阻止新增 JSONL 被提取并保存"
    );

    let stderr = String::from_utf8(extract.stderr).expect("日志降级诊断必须是 UTF-8");
    let stderr = stderr.replace(['\u{2068}', '\u{2069}'], "");
    let canonical_logs_root = logs_root
        .canonicalize()
        .expect("阻止日志建立的文件路径必须可规范化");
    let expected_location = format!("Object: {}", readable_windows_path(&canonical_logs_root));
    for expected in [
        "Warning:",
        expected_location.as_str(),
        "Reason: The target object already exists",
        "Impact: Business state was not changed",
        "Action: Check the path, filesystem state, and permissions",
    ] {
        assert!(
            stderr.contains(expected),
            "日志建立失败必须保留可读诊断字段 {expected:?}：{stderr}"
        );
    }
    for forbidden in [
        "observability.project_log.create",
        "Stage:",
        "component=",
        "operation=",
        "io_kind=",
        "raw_os_code=",
        "Project logging is unavailable or degraded",
    ] {
        assert!(
            !stderr.contains(forbidden),
            "日志建立失败不得公开内部字段 {forbidden:?}：{stderr}"
        );
    }
}

#[test]
fn project_log_warning_presentation_failure_returns_one_after_business_change_is_saved() {
    let temporary = tempfile::tempdir().expect("应可建立 stderr 故障进程测试目录");
    let root = temporary.path();
    let input = root.join("input");
    fs::create_dir(&input).expect("应可建立 Generic 输入目录");
    fs::write(
        input.join("story.jsonl"),
        "{\"id\":\"story\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"line\",\"text\":\"こんにちは\"}]}\n",
    )
    .expect("应可写入 Generic JSONL");

    let distribution = write_distribution(root, test_configuration());
    let project = "generic-log-warning-presentation-failure";
    assert_success(
        "stderr 故障前的 Generic Init",
        &run_att(
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
        ),
    );
    assert_success(
        "stderr 故障前的 Generic Extract",
        &run_att(root, &["generic", "extract", "--name", project]),
    );

    let workspace = distribution.join("projects/generic").join(project);
    let logs_root = workspace.join("logs");
    fs::rename(&logs_root, workspace.join("logs.saved")).expect("应可暂存既有日志目录");
    fs::write(&logs_root, b"not a directory").expect("应可用普通文件阻止日志目录建立");
    fs::write(
        input.join("extra.jsonl"),
        "{\"id\":\"extra\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"line\",\"text\":\"追加\"}]}\n",
    )
    .expect("应可增加待提取的 Generic JSONL");
    let database = workspace.join("project.db");
    let database_before = fs::read(&database).expect("Extract 前数据库必须可读取");

    let output = run_att_with_closed_stderr(root, &["generic", "extract", "--name", project]);
    assert_eq!(
        output.status.code(),
        Some(1),
        "项目日志警告无法通过 stderr 呈现时必须产生独立进程失败"
    );
    let database_after = fs::read(&database).expect("Extract 后数据库必须可读取");
    assert_ne!(
        database_after, database_before,
        "stderr 呈现失败不得回滚已经成功保存的 Extract 业务结果"
    );
}

#[test]
fn generic_fixed_http_503_is_unavailable_and_preserves_the_structured_status() {
    let temporary = tempfile::tempdir().expect("应可建立 HTTP 503 进程测试目录");
    let root = temporary.path();
    let input = root.join("input");
    fs::create_dir(&input).expect("应可建立 Generic 输入目录");
    fs::write(
        input.join("story.jsonl"),
        "{\"id\":\"story\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"line\",\"text\":\"こんにちは\"}]}\n",
    )
    .expect("应可写入 Generic JSONL");

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("本地 HTTP 503 服务应可绑定");
    let server_address = listener.local_addr().expect("HTTP 503 服务地址应可读取");
    let endpoint_port = u64::from(server_address.port());
    let endpoint = format!("http://{server_address}/v1/chat/completions");
    let server = thread::spawn(move || serve_http_503(listener));

    let distribution = write_distribution(root, &http_503_configuration(&endpoint));
    let project = "generic-http-503";
    assert_success(
        "HTTP 503 Generic Init",
        &run_att(
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
        ),
    );
    assert_success(
        "HTTP 503 Generic Extract",
        &run_att(root, &["generic", "extract", "--name", project]),
    );

    let logs_root = distribution
        .join("projects/generic")
        .join(project)
        .join("logs");
    let before = jsonl_files(&logs_root);
    let translate = run_att(root, &["generic", "translate", "--name", project, "local"]);
    assert_success("固定 HTTP 503 Generic Translate", &translate);
    assert_eq!(
        translate.status.code(),
        Some(0),
        "已明确的 HTTP 503 Unavailable 必须保持业务成功退出语义"
    );
    server
        .join()
        .expect("HTTP 503 服务线程不得 panic")
        .expect("HTTP 503 服务必须完成一条请求");

    let after = jsonl_files(&logs_root);
    let logs = after
        .into_iter()
        .filter(|path| !before.contains(path))
        .collect::<Vec<_>>();
    assert_eq!(logs.len(), 1, "Translate 必须新建一份运行日志");
    let (log, records) = read_jsonl_records(&logs[0]);
    assert!(
        records.iter().all(|record| {
            record["context"]
                == serde_json::json!({
                    "locale": "en",
                    "engine": "generic",
                    "project": project,
                    "command": "translate",
                })
        }),
        "HTTP 503 运行的每条记录必须保留完整 Generic Translate context：{log}"
    );

    let task_started = records
        .iter()
        .enumerate()
        .filter(|(_, record)| record["event"] == "task.started")
        .collect::<Vec<_>>();
    assert_eq!(
        task_started.len(),
        1,
        "一个已发出的模型任务必须恰好开始一次：{log}"
    );
    assert_eq!(
        task_started[0].1["payload"],
        serde_json::json!({"task": {"ordinal": 1, "total": 1}})
    );

    let diagnostics = records
        .iter()
        .enumerate()
        .filter(|(_, record)| record["event"] == "diagnostic.translation_task")
        .collect::<Vec<_>>();
    assert_eq!(
        diagnostics.len(),
        1,
        "固定 HTTP 503 必须形成一条原子任务诊断：{log}"
    );
    let diagnostic = diagnostics[0].1;
    assert_eq!(diagnostic["level"], "warn");
    assert_eq!(
        diagnostic["payload"],
        serde_json::json!({
            "relation": "primary",
            "object": format!("http://127.0.0.1:{endpoint_port}"),
            "reason": "The external service rejected the request (HTTP status 503; Provider code: service_unavailable; Provider type: server_error; Provider message: temporarily unavailable)",
            "impact": "Previously confirmed progress was preserved; the indicated content was not completed",
            "help": "Check the model service response and account limits",
        }),
        "HTTP 503 必须保留可读对象、状态和安全的服务端原因：{log}"
    );
    assert_log_has_only_readable_diagnostics(&log, &records);

    let task_finished = records
        .iter()
        .enumerate()
        .filter(|(_, record)| record["event"] == "task.finished")
        .collect::<Vec<_>>();
    assert_eq!(
        task_finished.len(),
        1,
        "一个已开始的模型任务必须恰好结束一次：{log}"
    );
    assert_eq!(task_finished[0].1["level"], "warn");
    assert_eq!(
        task_finished[0].1["payload"],
        serde_json::json!({
            "task": {"ordinal": 1, "total": 1},
            "attempts": 1,
            "outcome": {"kind": "unavailable"},
        }),
        "task.finished 只需说明当前任务结果：{log}"
    );

    let translation_finished = records
        .iter()
        .enumerate()
        .filter(|(_, record)| record["event"] == "translation.finished")
        .collect::<Vec<_>>();
    assert_eq!(
        translation_finished.len(),
        1,
        "每次 Translate 必须恰好产生一个翻译终态：{log}"
    );
    assert_eq!(translation_finished[0].1["level"], "warn");
    assert_eq!(
        translation_finished[0].1["payload"]["result"],
        serde_json::json!({
            "kind": "incomplete",
            "tasks": {
                "planned": 1,
                "started": 1,
                "complete": 0,
                "partial": 0,
                "unavailable": 1,
                "failed": 0,
                "cancelled": 0,
                "not_started": 0,
            },
            "summary": {
                "engine": "generic",
                "summary": {
                    "planned_units": 1,
                    "remaining_units": 1,
                    "cleared_units": 0,
                    "reused_units": 0,
                    "accepted_units": 0,
                    "written_units": 0,
                    "conflicted_units": 0,
                    "response_problems": 0,
                    "recoverable_request_exhaustions": 1,
                    "request_admission_stopped": false,
                },
            },
        }),
        "Unavailable 必须保留完整任务恒等式及 Generic 专属汇总：{log}"
    );
    let tasks = &translation_finished[0].1["payload"]["result"]["tasks"];
    let started_breakdown = ["complete", "partial", "unavailable", "failed", "cancelled"]
        .into_iter()
        .map(|field| tasks[field].as_u64().expect("任务计数必须是 u64"))
        .sum::<u64>();
    assert_eq!(tasks["started"], started_breakdown);
    assert_eq!(
        tasks["planned"].as_u64().expect("planned 必须是 u64"),
        tasks["started"].as_u64().expect("started 必须是 u64")
            + tasks["not_started"]
                .as_u64()
                .expect("not_started 必须是 u64")
    );

    let run_plan_finalized = records
        .iter()
        .filter(|record| record["event"] == "run_plan.finalized")
        .collect::<Vec<_>>();
    assert_eq!(
        run_plan_finalized.len(),
        1,
        "Unavailable 的 Translate run plan 必须有唯一明确终态：{log}"
    );
    assert_eq!(
        run_plan_finalized[0]["payload"]["result"],
        serde_json::json!({
            "kind": "saved",
            "transaction": "committed",
            "run_continues": false,
        }),
        "Unavailable 不得把已保存的运行计划退化成失败或未知：{log}"
    );

    let run_finished = records
        .iter()
        .enumerate()
        .filter(|(_, record)| record["event"] == "run.finished")
        .collect::<Vec<_>>();
    assert_eq!(run_finished.len(), 1, "运行必须只有一个终态：{log}");
    assert_eq!(
        run_finished[0].1["payload"]["result"],
        serde_json::json!({"kind": "succeeded"})
    );
    assert_eq!(
        run_finished[0].0,
        records.len() - 1,
        "run.finished 必须是日志最后一条记录：{log}"
    );
    assert!(
        task_started[0].0 < diagnostics[0].0
            && diagnostics[0].0 < task_finished[0].0
            && task_finished[0].0 < translation_finished[0].0
            && translation_finished[0].0 < run_finished[0].0,
        "任务开始、诊断、任务终态、翻译终态和运行终态必须保持因果顺序：{log}"
    );
}

#[test]
fn generic_fatal_http_error_fails_and_stops_unscheduled_tasks() {
    let temporary = tempfile::tempdir().expect("应可建立 HTTP 400 进程测试目录");
    let root = temporary.path();
    let input = root.join("input");
    fs::create_dir(&input).expect("应可建立 Generic 输入目录");
    fs::write(
        input.join("first.jsonl"),
        "{\"id\":\"first\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"line\",\"text\":\"こんにちは\"}]}\n",
    )
    .expect("应可写入第一个 Generic TaskBlock");
    fs::write(
        input.join("second.jsonl"),
        "{\"id\":\"second\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"line\",\"text\":\"さようなら\"}]}\n",
    )
    .expect("应可写入第二个 Generic TaskBlock");

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("本地 HTTP 400 服务应可绑定");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("HTTP 400 服务地址应可读取")
    );
    let server = thread::spawn(move || serve_single_http_400(listener));
    let distribution = write_distribution(root, &http_configuration(&endpoint, 1));
    let project = "generic-http-400-fatal";
    assert_success(
        "HTTP 400 Generic Init",
        &run_att(
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
        ),
    );
    assert_success(
        "HTTP 400 Generic Extract",
        &run_att(root, &["generic", "extract", "--name", project]),
    );

    let logs_root = distribution
        .join("projects/generic")
        .join(project)
        .join("logs");
    let before = jsonl_files(&logs_root);
    let translate = run_att(root, &["generic", "translate", "--name", project, "local"]);
    assert_eq!(
        translate.status.code(),
        Some(1),
        "Fatal HTTP 400 必须使 Generic Translate 失败：{}",
        String::from_utf8_lossy(&translate.stderr)
    );
    server
        .join()
        .expect("HTTP 400 服务线程不得 panic")
        .expect("Fatal 后不得再发送下一项任务");

    let logs = jsonl_files(&logs_root)
        .into_iter()
        .filter(|path| !before.contains(path))
        .collect::<Vec<_>>();
    assert_eq!(logs.len(), 1, "Fatal Translate 必须新建一份运行日志");
    let (log, records) = read_jsonl_records(&logs[0]);
    assert_log_has_only_readable_diagnostics(&log, &records);
    assert_eq!(
        records
            .iter()
            .filter(|record| record["event"] == "task.started")
            .count(),
        1,
        "串行执行的首个 Fatal 必须阻止后续任务开始：{log}"
    );
    let task_finished = records
        .iter()
        .find(|record| record["event"] == "task.finished")
        .expect("Fatal 任务必须有终态");
    assert_eq!(task_finished["payload"]["outcome"]["kind"], "failed");

    let translation = records
        .iter()
        .filter(|record| record["event"] == "translation.finished")
        .collect::<Vec<_>>();
    assert_eq!(
        translation.len(),
        1,
        "Translate 必须只有一个翻译终态：{log}"
    );
    let result = &translation[0]["payload"]["result"];
    assert_eq!(result["kind"], "failed");
    assert_eq!(
        result["tasks"],
        serde_json::json!({
            "planned": 2,
            "started": 1,
            "complete": 0,
            "partial": 0,
            "unavailable": 0,
            "failed": 1,
            "cancelled": 0,
            "not_started": 1,
        })
    );
    assert_eq!(result["summary"]["engine"], "generic");
    assert_eq!(
        records.last().expect("Fatal 日志不得为空")["payload"]["result"],
        serde_json::json!({"kind": "failed"})
    );
}

#[test]
fn generic_failure_after_resource_commit_keeps_saved_plan_and_summary() {
    let temporary = tempfile::tempdir().expect("应可建立资源提交后失败测试目录");
    let root = temporary.path();
    let input = root.join("input");
    fs::create_dir(&input).expect("应可建立 Generic 输入目录");
    fs::write(
        input.join("story.jsonl"),
        "{\"id\":\"story\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"line\",\"text\":\"こんにちは\"}]}\n",
    )
    .expect("应可写入 Generic JSONL");
    let placeholders = root.join("placeholders.toml");
    fs::write(&placeholders, "[[rule]]\npattern = 'NEVER_MATCH'\n")
        .expect("应可写入变更后的 Placeholder 规则");

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("Provider spy 应可绑定");
    listener
        .set_nonblocking(true)
        .expect("Provider spy 应可设为非阻塞");
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().expect("Provider spy 地址应可读取")
    );
    let missing_pem = root
        .join("missing.pem")
        .to_string_lossy()
        .replace('\\', "\\\\");
    let configuration = http_configuration(&endpoint, 10_000).replace(
        "additional_pem_files = []",
        &format!("additional_pem_files = [\"{missing_pem}\"]"),
    );
    let distribution = write_distribution(root, &configuration);
    let project = "generic-saved-resource-before-failure";
    assert_success(
        "资源失败 Generic Init",
        &run_att(
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
        ),
    );
    assert_success(
        "资源失败 Generic Extract",
        &run_att(root, &["generic", "extract", "--name", project]),
    );
    let workspace = distribution.join("projects/generic").join(project);
    let database = workspace.join("project.db");
    let database_before = fs::read(&database).expect("Translate 前数据库应可读取");
    let logs_root = workspace.join("logs");
    let before = jsonl_files(&logs_root);

    let translate = run_att(
        root,
        &[
            "generic",
            "translate",
            "--name",
            project,
            "local",
            "--placeholders",
            placeholders.to_str().expect("Placeholder 路径应是 Unicode"),
        ],
    );
    assert_eq!(
        translate.status.code(),
        Some(1),
        "资源已经提交后，缺失 PEM 仍必须明确失败"
    );
    assert_ne!(
        fs::read(&database).expect("失败后数据库应可读取"),
        database_before,
        "Placeholder 资源提交不得被后续 PEM 失败伪报为未保存"
    );
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock),
        "PEM 准备失败前不得发送模型请求"
    );

    let logs = jsonl_files(&logs_root)
        .into_iter()
        .filter(|path| !before.contains(path))
        .collect::<Vec<_>>();
    assert_eq!(logs.len(), 1, "失败 Translate 必须新建一份运行日志");
    let (log, records) = read_jsonl_records(&logs[0]);
    let finalized = records
        .iter()
        .find(|record| record["event"] == "run_plan.finalized")
        .expect("已提交资源必须有 run plan 终态");
    assert_eq!(
        finalized["payload"]["result"],
        serde_json::json!({
            "kind": "saved",
            "transaction": "committed",
            "run_continues": false,
        }),
        "后续失败不得把已提交资源写成 not_saved：{log}"
    );
    let translation = records
        .iter()
        .find(|record| record["event"] == "translation.finished")
        .expect("失败 Translate 必须有翻译终态");
    assert_eq!(translation["payload"]["result"]["kind"], "failed");
    assert_eq!(
        translation["payload"]["result"]["summary"]["engine"], "generic",
        "后续失败不得丢失已经建立的 Generic summary：{log}"
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record["event"] == "translation.finished")
            .count(),
        1,
        "失败 Translate 必须只有一个翻译终态：{log}"
    );
}

#[test]
#[allow(clippy::permissions_set_readonly_false)]
fn generic_write_back_reports_recovery_required_after_publish_cleanup_failure() {
    let temporary = tempfile::tempdir().expect("应可建立发布收尾失败进程测试目录");
    let root = temporary.path();
    let input = root.join("input");
    fs::create_dir(&input).expect("应可建立 Generic 输入目录");
    let source = input.join("story.jsonl");
    let source_bytes =
        b"{\"id\":\"story\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"line\",\"text\":\"hello\"}]}\n";
    fs::write(&source, source_bytes).expect("应可写入 Generic JSONL");

    let distribution = write_distribution(root, test_configuration());
    let project = "generic-publish-finalization";
    assert_success(
        "发布收尾失败前的 Generic Init",
        &run_att(
            root,
            &[
                "generic",
                "init",
                "--name",
                project,
                "--path",
                input.to_str().expect("临时路径应是 Unicode"),
                "--source-language",
                "en",
                "--target-language",
                "zh-Hans",
            ],
        ),
    );
    assert_success(
        "发布收尾失败前的 Generic Extract",
        &run_att(root, &["generic", "extract", "--name", project]),
    );
    assert_success(
        "建立既有 Generic WriteBack 输出",
        &run_att(root, &["generic", "write-back", "--name", project]),
    );

    let workspace = distribution.join("projects/generic").join(project);
    let output_file = workspace.join("write_back/story.jsonl");
    let mut permissions = fs::metadata(&output_file)
        .expect("既有 WriteBack 文件应可读取元数据")
        .permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&output_file, permissions).expect("应可让旧输出在备份清理时拒绝删除");

    let logs_root = workspace.join("logs");
    let logs_before = jsonl_files(&logs_root);
    let failed = run_att(root, &["generic", "write-back", "--name", project]);
    assert_eq!(
        failed.status.code(),
        Some(1),
        "输出已发布但收尾失败时必须返回失败\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&failed.stdout),
        String::from_utf8_lossy(&failed.stderr)
    );
    assert_eq!(
        fs::read(&source).expect("外部输入应可重新读取"),
        source_bytes,
        "WriteBack 发布失败不得修改外部输入"
    );
    assert_eq!(
        fs::read(&output_file).expect("新输出已发布后必须可读取"),
        source_bytes,
        "收尾失败不得掩盖已经生效的新输出"
    );

    let new_logs = jsonl_files(&logs_root)
        .into_iter()
        .filter(|path| !logs_before.contains(path))
        .collect::<Vec<_>>();
    assert_eq!(new_logs.len(), 1, "一次 WriteBack 只能新增一份运行日志");
    let records = fs::read_to_string(&new_logs[0])
        .expect("发布收尾失败日志应可读取")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("日志行必须是 JSON"))
        .collect::<Vec<_>>();
    let diagnostic_records = records
        .iter()
        .filter(|record| record["event"] == "diagnostic.publication")
        .collect::<Vec<_>>();
    assert_eq!(
        diagnostic_records.len(),
        1,
        "发布收尾失败必须形成一条可读 Publication 诊断"
    );
    let diagnostic = diagnostic_records[0];
    let canonical_output_root = output_file
        .parent()
        .expect("输出文件必须拥有父目录")
        .canonicalize()
        .expect("已发布输出根必须可规范化");
    let canonical_output_root = canonical_output_root.to_string_lossy();
    let public_output_root = canonical_output_root
        .strip_prefix(r"\\?\")
        .unwrap_or(canonical_output_root.as_ref());
    assert_eq!(diagnostic["payload"]["object"], public_output_root);
    assert_eq!(
        diagnostic["payload"]["reason"],
        "The operation result exists but finalization failed"
    );
    assert_eq!(
        diagnostic["payload"]["help"],
        "Do not delete the listed recovery artifacts; recover the output before retrying"
    );
    assert_log_has_only_readable_diagnostics(
        &fs::read_to_string(&new_logs[0]).expect("发布日志必须可读取"),
        &records,
    );
    let residual = workspace.join(".directory-publish/write_back/backup");
    assert!(residual.is_dir(), "诊断中的备份残留必须真实存在");

    let publication_finished = records
        .iter()
        .find(|record| record["event"] == "publication.finished")
        .expect("发布收尾失败必须写入 Publication 终态");
    assert_eq!(
        publication_finished["payload"]["result"]["kind"],
        "recovery_required"
    );
    assert!(
        publication_finished["payload"]["result"]
            .get("diagnostic")
            .is_none()
    );
    let terminal = records.last().expect("运行日志必须有终态");
    assert_eq!(terminal["event"], "run.finished");
    assert_eq!(terminal["payload"]["result"]["kind"], "recovery_required");
    assert!(terminal["payload"]["result"].get("diagnostic").is_none());
    assert!(
        records.iter().all(|record| {
            !(record["event"] == "publication.finished"
                && record["payload"]["result"]["kind"] == "published")
                && !(record["event"] == "run.finished"
                    && record["payload"]["result"]["kind"] == "succeeded")
        }),
        "发布收尾失败路径不得同时伪报发布或运行成功"
    );

    let residual_file = residual.join("story.jsonl");
    let mut permissions = fs::metadata(&residual_file)
        .expect("残留备份中的只读文件应存在")
        .permissions();
    permissions.set_readonly(false);
    fs::set_permissions(&residual_file, permissions).expect("测试结束前应可解除只读属性");
    assert_success(
        "同目标 Generic WriteBack 自动恢复",
        &run_att(root, &["generic", "write-back", "--name", project]),
    );
    assert!(
        !residual.exists(),
        "后续同目标 WriteBack 必须清理已修正的恢复现场"
    );
}

#[test]
fn generic_cancellation_finishes_started_tasks_and_counts_unstarted_tasks() {
    let temporary = tempfile::tempdir().expect("应可建立 Generic 取消进程测试目录");
    let root = temporary.path();
    let input = root.join("input");
    fs::create_dir(&input).expect("应可建立 Generic 输入目录");
    fs::write(
        input.join("story.jsonl"),
        concat!(
            "{\"id\":\"story-1\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"line\",\"text\":\"こんにちは一\"}]}\n",
            "{\"id\":\"story-2\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"line\",\"text\":\"こんにちは二\"}]}\n",
            "{\"id\":\"story-3\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"line\",\"text\":\"こんにちは三\"}]}\n",
            "{\"id\":\"story-4\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"line\",\"text\":\"こんにちは四\"}]}\n",
        ),
    )
    .expect("应可写入多任务 Generic JSONL");

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("本地取消服务应可绑定");
    let server_address = listener.local_addr().expect("取消服务地址应可读取");
    let endpoint = format!("http://{server_address}/v1/chat/completions");
    let (request_arrived_tx, request_arrived_rx) = mpsc::channel();
    let server = thread::spawn(move || serve_until_client_disconnect(listener, request_arrived_tx));

    let distribution = write_distribution(root, &http_configuration(&endpoint, 1));
    let project = "generic-cancel-task-terminals";
    assert_success(
        "取消前的 Generic Init",
        &run_att(
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
        ),
    );
    assert_success(
        "取消前的 Generic Extract",
        &run_att(root, &["generic", "extract", "--name", project]),
    );

    let logs_root = distribution
        .join("projects/generic")
        .join(project)
        .join("logs");
    let before = jsonl_files(&logs_root);
    let mut child =
        spawn_att_in_new_process_group(root, &["generic", "translate", "--name", project, "local"]);
    if let Err(error) = request_arrived_rx.recv_timeout(Duration::from_secs(15)) {
        let _ = child.kill();
        let output = child.wait_with_output().expect("取消测试子进程必须可回收");
        let _ = TcpStream::connect(server_address);
        let _ = server.join();
        panic!(
            "首个模型请求必须在时限内到达服务端 gate：{error}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    if let Err(error) = send_ctrl_break(&child) {
        let _ = child.kill();
        let output = child.wait_with_output().expect("取消测试子进程必须可回收");
        server
            .join()
            .expect("取消服务线程不得 panic")
            .expect("终止子进程后服务端必须观察到连接关闭");
        panic!(
            "必须能向 ATT 独立进程组发送 Ctrl-Break：{error}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let output_result = wait_for_child(child, Duration::from_secs(20));
    let server_result = server.join().expect("取消服务线程不得 panic");
    let output = output_result.expect("ATT 必须在合作取消后及时结束");
    server_result.expect("合作取消必须关闭正在等待响应的 HTTP 连接");
    assert_eq!(
        output.status.code(),
        Some(130),
        "合作取消必须保持退出码 130\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr).replace(['\u{2068}', '\u{2069}'], "");
    for expected in [
        "command was cancelled",
        "4 planned tasks",
        "1 started",
        "3 not started",
        "1 cancelled",
        "4 remaining units",
    ] {
        assert!(
            stderr.contains(expected),
            "取消终端摘要缺少 {expected:?}：\n{stderr}"
        );
    }

    let logs = jsonl_files(&logs_root)
        .into_iter()
        .filter(|path| !before.contains(path))
        .collect::<Vec<_>>();
    assert_eq!(logs.len(), 1, "取消的 Translate 必须建立恰好一份运行日志");
    let (log, records) = read_jsonl_records(&logs[0]);
    assert_eq!(
        records
            .iter()
            .filter(|record| record["event"] == "run.cancel_requested")
            .count(),
        1,
        "首个控制信号必须恰好记录一次取消请求：{log}"
    );

    let started = records
        .iter()
        .filter(|record| record["event"] == "task.started")
        .collect::<Vec<_>>();
    let finished = records
        .iter()
        .filter(|record| record["event"] == "task.finished")
        .collect::<Vec<_>>();
    assert_eq!(
        started.len(),
        1,
        "并发度为 1 时 gate 前只能开始一个任务：{log}"
    );
    assert_eq!(
        finished.len(),
        started.len(),
        "每个已开始任务必须拥有完整终态：{log}"
    );
    assert_eq!(
        finished[0]["payload"]["outcome"],
        serde_json::json!({"kind": "cancelled"}),
        "正在等待 HTTP 响应的任务必须结束为 cancelled：{log}"
    );
    assert_eq!(
        finished[0]["payload"]["task"], started[0]["payload"]["task"],
        "task.finished 必须对应同一个已开始任务：{log}"
    );

    let translation_finished = records
        .iter()
        .filter(|record| record["event"] == "translation.finished")
        .collect::<Vec<_>>();
    assert_eq!(
        translation_finished.len(),
        1,
        "取消的 Translate 仍必须拥有唯一翻译终态：{log}"
    );
    let result = &translation_finished[0]["payload"]["result"];
    assert_eq!(result["kind"], "cancelled");
    assert_eq!(
        result["summary"],
        serde_json::json!({
            "engine": "generic",
            "summary": {
                "planned_units": 4,
                "remaining_units": 4,
                "cleared_units": 0,
                "reused_units": 0,
                "accepted_units": 0,
                "written_units": 0,
                "conflicted_units": 0,
                "response_problems": 0,
                "recoverable_request_exhaustions": 0,
                "request_admission_stopped": false,
            },
        }),
        "取消终态必须保存已确认的全零 Generic 业务汇总：{log}"
    );
    let tasks = &result["tasks"];
    let planned = tasks["planned"].as_u64().expect("planned 必须是 u64");
    let started_count = tasks["started"].as_u64().expect("started 必须是 u64");
    let not_started = tasks["not_started"]
        .as_u64()
        .expect("not_started 必须是 u64");
    assert!(planned > started_count, "样本必须确实保留未开始任务：{log}");
    assert_eq!(started_count, 1);
    assert_eq!(tasks["cancelled"], 1);
    for field in ["complete", "partial", "unavailable", "failed"] {
        assert_eq!(tasks[field], 0, "取消样本的 {field} 必须为零：{log}");
    }
    assert_eq!(planned, started_count + not_started);
    assert_eq!(
        started_count,
        tasks["complete"].as_u64().expect("complete 必须是 u64")
            + tasks["partial"].as_u64().expect("partial 必须是 u64")
            + tasks["unavailable"]
                .as_u64()
                .expect("unavailable 必须是 u64")
            + tasks["failed"].as_u64().expect("failed 必须是 u64")
            + tasks["cancelled"].as_u64().expect("cancelled 必须是 u64")
    );
    assert_eq!(
        records.last().expect("取消日志不得为空")["event"],
        "run.finished",
        "run.finished 必须是取消日志最后一条记录：{log}"
    );
    assert_eq!(
        records.last().expect("取消日志不得为空")["payload"]["result"],
        serde_json::json!({"kind": "cancelled"}),
        "唯一且最后的 run.finished 必须表达 cancelled：{log}"
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

[translation]
record_translation_tasks = false

[[translation.profiles]]
id = "local"
llm_client = "local"
target_task_user_message_characters = 10000
"#
}

fn http_503_configuration(endpoint: &str) -> String {
    http_configuration(endpoint, 10_000)
}

fn http_configuration(endpoint: &str, target_task_user_message_characters: usize) -> String {
    format!(
        r#"[prompts]
thinking_output = true
source_echo = false

[llm.clients.local]
url = "{endpoint}"
api_key = "unused-test-secret"
model = "unused-test-model"
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
llm_client = "local"
target_task_user_message_characters = {target_task_user_message_characters}
"#
    )
}

fn write_distribution(root: &Path, configuration: &str) -> PathBuf {
    let distribution = root.join("release");
    fs::create_dir_all(&distribution).expect("应可建立测试发行目录");
    fs::write(distribution.join("config.toml"), configuration).expect("应可写入测试配置");
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
    distribution
}

fn serve_http_503(listener: TcpListener) -> Result<(), String> {
    let (mut stream, _) = listener
        .accept()
        .map_err(|error| format!("接受 HTTP 请求失败：{error}"))?;
    read_http_request(&mut stream)?;
    let body = r#"{"error":{"code":"service_unavailable","type":"server_error","message":"temporarily unavailable"}}"#;
    write!(
        stream,
        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .map_err(|error| format!("写入 HTTP 503 响应失败：{error}"))?;
    stream
        .flush()
        .map_err(|error| format!("刷新 HTTP 503 响应失败：{error}"))
}

fn serve_single_http_400(listener: TcpListener) -> Result<(), String> {
    let (mut stream, _) = listener
        .accept()
        .map_err(|error| format!("接受 HTTP 400 请求失败：{error}"))?;
    read_http_request(&mut stream)?;
    let body = r#"{"error":{"code":"bad_request","type":"invalid_request_error","message":"invalid request"}}"#;
    write!(
        stream,
        "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .map_err(|error| format!("写入 HTTP 400 响应失败：{error}"))?;
    stream
        .flush()
        .map_err(|error| format!("刷新 HTTP 400 响应失败：{error}"))?;
    drop(stream);

    listener
        .set_nonblocking(true)
        .map_err(|error| format!("设置 HTTP 400 listener 非阻塞失败：{error}"))?;
    for _ in 0..20 {
        match listener.accept() {
            Ok(_) => return Err("Fatal 后仍发送了后续模型任务".to_owned()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(format!("检查后续 HTTP 请求失败：{error}")),
        }
    }
    Ok(())
}

fn serve_until_client_disconnect(
    listener: TcpListener,
    request_arrived: mpsc::Sender<()>,
) -> Result<(), String> {
    let (mut stream, _) = listener
        .accept()
        .map_err(|error| format!("接受取消测试 HTTP 请求失败：{error}"))?;
    read_http_request(&mut stream)?;
    request_arrived
        .send(())
        .map_err(|_| "取消测试 request gate 已关闭".to_owned())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .map_err(|error| format!("设置取消测试连接读取超时失败：{error}"))?;
    let mut buffer = [0_u8; 4096];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                return Err("合作取消后 HTTP 客户端没有关闭连接".to_owned());
            }
            // Windows 可用 EOF 或 connection reset 表达客户端主动放弃在途请求；
            // 两者都证明合作取消已经关闭这条连接。
            Err(_) => return Ok(()),
        }
    }
}

fn read_http_request(stream: &mut TcpStream) -> Result<(), String> {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .map_err(|error| format!("设置 HTTP 请求读取超时失败：{error}"))?;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let count = stream
            .read(&mut buffer)
            .map_err(|error| format!("读取 HTTP 请求失败：{error}"))?;
        if count == 0 {
            return Err("HTTP 请求在 header 完成前关闭".to_owned());
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            return Ok(());
        }
    }
}

fn run_att(root: &Path, arguments: &[&str]) -> Output {
    Command::new(stage_att_executable(root))
        .current_dir(root)
        .args(["--ui-language", "en"])
        .args(arguments)
        .output()
        .expect("att.exe 应可执行")
}

fn spawn_att_in_new_process_group(root: &Path, arguments: &[&str]) -> Child {
    let mut command = Command::new(stage_att_executable(root));
    command
        .current_dir(root)
        .args(["--ui-language", "en"])
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    command.spawn().expect("att.exe 独立进程组应可启动")
}

fn send_ctrl_break(child: &Child) -> io::Result<()> {
    // SAFETY: child.id() 是仍存活的独立 Windows process group ID；该调用不保存指针。
    if unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn wait_for_child(mut child: Child, timeout: Duration) -> Result<Output, String> {
    let timeout_millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
    // SAFETY: raw handle 在整个等待期间由 `child` 持有且有效，函数不会取得句柄所有权。
    let wait = unsafe { WaitForSingleObject(child.as_raw_handle(), timeout_millis) };
    match wait {
        WAIT_OBJECT_0 => child
            .wait_with_output()
            .map_err(|error| format!("读取已结束 ATT 子进程输出失败：{error}")),
        WAIT_TIMEOUT => {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .map_err(|error| format!("超时后回收 ATT 子进程失败：{error}"))?;
            Err(format!(
                "ATT 没有在 {} ms 内完成合作取消\nstdout:\n{}\nstderr:\n{}",
                timeout.as_millis(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ))
        }
        status => {
            let _ = child.kill();
            let _ = child.wait();
            Err(format!("等待 ATT 子进程失败，Windows wait status={status}"))
        }
    }
}

fn run_att_with_closed_stderr(root: &Path, arguments: &[&str]) -> Output {
    let mut child = Command::new(stage_att_executable(root))
        .current_dir(root)
        .args(["--ui-language", "en"])
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("att.exe 应可启动");
    drop(child.stderr.take().expect("测试子进程必须拥有 stderr 管道"));
    child.wait_with_output().expect("att.exe 应可结束")
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

fn read_jsonl_records(path: &Path) -> (String, Vec<serde_json::Value>) {
    let text = fs::read_to_string(path).expect("项目 JSONL 必须可读取");
    let records = text
        .lines()
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("项目 JSONL 第 {} 行必须有效：{error}", index + 1))
        })
        .collect::<Vec<_>>();
    assert!(!records.is_empty(), "项目 JSONL 不得为空");
    (text, records)
}

fn assert_log_has_only_readable_diagnostics(log: &str, records: &[serde_json::Value]) {
    let diagnostics = records
        .iter()
        .filter(|record| {
            record["event"]
                .as_str()
                .is_some_and(|event| event.starts_with("diagnostic."))
        })
        .collect::<Vec<_>>();
    assert!(!diagnostics.is_empty(), "样本必须包含诊断记录：{log}");
    for diagnostic in diagnostics {
        let payload = diagnostic["payload"]
            .as_object()
            .expect("诊断 payload 必须是对象");
        let mut fields = payload.keys().map(String::as_str).collect::<Vec<_>>();
        fields.sort_unstable();
        assert_eq!(fields, ["help", "impact", "object", "reason", "relation"]);
        for field in fields {
            assert!(
                payload[field]
                    .as_str()
                    .is_some_and(|value| !value.trim().is_empty()),
                "诊断字段 {field} 必须是非空自然文本：{log}"
            );
        }
    }
    for forbidden in [
        "\"occurrence\"",
        "\"report\"",
        "\"effect\"",
        "\"stage\"",
        "\"issue\"",
        "\"resolution\"",
        "\"query_id\"",
        "\"request_id\"",
        "expected_fingerprint",
        "actual_fingerprint",
        "provider_code",
        "provider_type",
        "provider_message",
        "group_location",
        "unit_role",
    ] {
        assert!(
            !log.contains(forbidden),
            "项目日志不得公开内部字段 {forbidden:?}：{log}"
        );
    }
    assert_no_opaque_identifier(log);
}

fn assert_no_opaque_identifier(text: &str) {
    let bytes = text.as_bytes();
    assert!(
        !bytes.windows(36).any(|candidate| {
            candidate
                .iter()
                .enumerate()
                .all(|(index, byte)| match index {
                    8 | 13 | 18 | 23 => *byte == b'-',
                    _ => byte.is_ascii_hexdigit(),
                })
        }),
        "用户可见日志不得包含 UUID：{text}"
    );
    assert!(
        !bytes.windows(64).any(|candidate| {
            candidate.iter().all(u8::is_ascii_hexdigit)
                && candidate.first().is_some_and(|byte| !byte.is_ascii_digit())
        }),
        "用户可见日志不得包含 64 位十六进制标识：{text}"
    );
}

fn readable_windows_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix(r"\\?\")
        .unwrap_or(text.as_ref())
        .to_owned()
}

fn assert_success(stage: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{stage} 应成功\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
