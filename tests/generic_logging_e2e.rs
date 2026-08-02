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
    let unavailable_log =
        fs::read_to_string(&unavailable_logs[0]).expect("Unavailable 运行日志必须可读取");
    assert!(
        unavailable_log.contains("\"code\":\"diagnostic.translation_task\"")
            && unavailable_log.contains("\"code\":\"task.finished\"")
            && unavailable_log.contains("\"kind\":\"unavailable\"")
            && unavailable_log.contains("\"code\":\"translation.finished\"")
            && unavailable_log.contains("\"kind\":\"incomplete\""),
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
        failure_log.contains("\"code\":\"diagnostic.extract\"")
            && failure_log.contains("\"command\":\"extract\"")
            && failure_log.contains("\"code\":\"filesystem.not_found\"")
            && failure_log.contains("\"stage\":\"project_opening\""),
        "项目打开失败必须把命令、阶段和结构化原因写入本次 JSONL：{failure_log}"
    );
}

#[test]
fn generic_extract_succeeds_when_project_log_cannot_be_created_and_reports_full_diagnostic() {
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
    for expected in [
        "Project logging is unavailable or degraded",
        "project_log_path=",
        "Error [observability.project_log.create]",
        "Stage: Project logging",
        "Location: project_log",
        "component=project_log",
        "operation=create",
        "io_kind=already_exists",
        "Impact: State was not changed",
        "Action: Check the path, filesystem state, and permissions",
    ] {
        assert!(
            stderr.contains(expected),
            "日志建立失败必须保留完整诊断字段 {expected:?}：{stderr}"
        );
    }
    assert!(
        stderr.contains(project) && stderr.contains("\\logs"),
        "日志建立失败必须指出当前项目的具体日志路径：{stderr}"
    );
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
        .filter(|(_, record)| record["code"] == "task.started")
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
        .filter(|(_, record)| record["code"] == "diagnostic.translation_task")
        .collect::<Vec<_>>();
    assert_eq!(
        diagnostics.len(),
        1,
        "固定 HTTP 503 必须形成一条原子任务诊断：{log}"
    );
    let diagnostic = diagnostics[0].1;
    assert_eq!(diagnostic["level"], "warn");
    assert_eq!(diagnostic["payload"]["scope"], "translation_task");
    assert_eq!(
        diagnostic["payload"]["report"],
        serde_json::json!({
            "effect": "progress_preserved",
            "primary": {
                "code": "http.status",
                "stage": "model_request",
                "issue": {
                    "family": "http",
                    "details": {
                        "kind": "status",
                        "endpoint": {
                            "scheme": "http",
                            "host": "127.0.0.1",
                            "port": endpoint_port,
                        },
                        "status": 503,
                        "retry_after_seconds": null,
                        "provider_code": "service_unavailable",
                        "provider_type": "server_error",
                        "provider_message": "temporarily unavailable",
                        "response_read_failure": null,
                    },
                },
                "resolution": "check_model_service",
            },
            "related": [],
        }),
        "HTTP 503 必须保留状态、Endpoint、供应商安全字段和已确认进度影响：{log}"
    );
    let diagnostic_id = diagnostic["payload"]["id"]
        .as_u64()
        .expect("任务诊断 occurrence ID 必须是非零整数");
    assert_ne!(diagnostic_id, 0, "任务诊断 occurrence ID 不得为零");

    let task_finished = records
        .iter()
        .enumerate()
        .filter(|(_, record)| record["code"] == "task.finished")
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
            "outcome": {"kind": "unavailable", "diagnostic": diagnostic_id},
        }),
        "task.finished 必须引用同一条 HTTP 503 occurrence：{log}"
    );

    let translation_finished = records
        .iter()
        .enumerate()
        .filter(|(_, record)| record["code"] == "translation.finished")
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
                    "cleared_units": 0,
                    "reused_units": 0,
                    "accepted_units": 0,
                    "written_units": 0,
                    "conflicted_units": 0,
                    "response_problems": 0,
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
        .filter(|record| record["code"] == "run_plan.finalized")
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
        .filter(|(_, record)| record["code"] == "run.finished")
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
        .filter(|record| record["code"] == "diagnostic.publication")
        .collect::<Vec<_>>();
    assert_eq!(
        diagnostic_records.len(),
        1,
        "发布收尾失败必须形成一条原子 Publication occurrence"
    );
    let diagnostic = diagnostic_records[0];
    let occurrence = diagnostic["payload"]["id"]
        .as_u64()
        .expect("Publication occurrence ID 必须为正整数");
    let report = &diagnostic["payload"]["report"];
    assert_eq!(report["effect"], "applied_finalization_failed");
    assert_eq!(report["primary"]["code"], "publication.finalization_failed");
    assert_eq!(report["primary"]["stage"], "publication");
    assert_eq!(report["primary"]["issue"]["family"], "publication");
    let problem = &report["primary"]["issue"]["details"]["problem"];
    assert_eq!(problem["kind"], "published_finalization_failed");
    let canonical_output_root = output_file
        .parent()
        .expect("输出文件必须拥有父目录")
        .canonicalize()
        .expect("已发布输出根必须可规范化");
    assert_eq!(
        problem["output_root"],
        canonical_output_root.to_string_lossy().as_ref()
    );
    let residual = PathBuf::from(
        problem["residual_path"]
            .as_str()
            .expect("发布收尾诊断必须保存残留路径"),
    );
    assert!(residual.is_dir(), "诊断中的备份残留必须真实存在");

    let publication_finished = records
        .iter()
        .find(|record| record["code"] == "publication.finished")
        .expect("发布收尾失败必须写入 Publication 终态");
    assert_eq!(
        publication_finished["payload"]["result"]["kind"],
        "recovery_required"
    );
    assert_eq!(
        publication_finished["payload"]["result"]["diagnostic"],
        occurrence
    );
    let terminal = records.last().expect("运行日志必须有终态");
    assert_eq!(terminal["code"], "run.finished");
    assert_eq!(terminal["payload"]["result"]["kind"], "recovery_required");
    assert_eq!(terminal["payload"]["result"]["diagnostic"], occurrence);
    assert!(
        records.iter().all(|record| {
            !(record["code"] == "publication.finished"
                && record["payload"]["result"]["kind"] == "published")
                && !(record["code"] == "run.finished"
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

    let logs = jsonl_files(&logs_root)
        .into_iter()
        .filter(|path| !before.contains(path))
        .collect::<Vec<_>>();
    assert_eq!(logs.len(), 1, "取消的 Translate 必须建立恰好一份运行日志");
    let (log, records) = read_jsonl_records(&logs[0]);
    assert_eq!(
        records
            .iter()
            .filter(|record| record["code"] == "run.cancel_requested")
            .count(),
        1,
        "首个控制信号必须恰好记录一次取消请求：{log}"
    );

    let started = records
        .iter()
        .filter(|record| record["code"] == "task.started")
        .collect::<Vec<_>>();
    let finished = records
        .iter()
        .filter(|record| record["code"] == "task.finished")
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
        .filter(|record| record["code"] == "translation.finished")
        .collect::<Vec<_>>();
    assert_eq!(
        translation_finished.len(),
        1,
        "取消的 Translate 仍必须拥有唯一翻译终态：{log}"
    );
    let result = &translation_finished[0]["payload"]["result"];
    assert_eq!(result["kind"], "cancelled");
    assert!(
        result["summary"].is_null(),
        "取消终态不得伪造业务汇总：{log}"
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
        records.last().expect("取消日志不得为空")["code"],
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
quote_repair_pairs = [["“", "”"], ["‘", "’"]]

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
quote_repair_pairs = [["“", "”"], ["‘", "’"]]

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
        .args(["--ui-language", "en", "--progress", "off"])
        .args(arguments)
        .output()
        .expect("att.exe 应可执行")
}

fn spawn_att_in_new_process_group(root: &Path, arguments: &[&str]) -> Child {
    let mut command = Command::new(stage_att_executable(root));
    command
        .current_dir(root)
        .args(["--ui-language", "en", "--progress", "off"])
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
        .args(["--ui-language", "en", "--progress", "off"])
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

fn assert_success(stage: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{stage} 应成功\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
