#![cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]

//! OpenAI-compatible 流式协议从真实 CLI 入口到持久结果的最小纵向回归。

use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::Duration;

use rusqlite::Connection;
use serde_json::{Value, json};

const SOURCE: &str = "こんにちは";

#[derive(Clone, Copy, Debug)]
enum StreamingProtocol {
    Chat,
    Responses,
}

impl StreamingProtocol {
    const fn name(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Responses => "responses",
        }
    }

    const fn request_path(self) -> &'static str {
        match self {
            Self::Chat => "/v1/chat/completions",
            Self::Responses => "/v1/responses",
        }
    }

    const fn translation(self) -> &'static str {
        match self {
            Self::Chat => "Chat流式译文",
            Self::Responses => "Responses流式译文",
        }
    }

    const fn thinking(self) -> &'static str {
        match self {
            Self::Chat => "chat-stream-private",
            Self::Responses => "responses-stream-private",
        }
    }
}

struct ObservedRequest {
    request_line: String,
    headers: String,
    body: Value,
}

#[test]
fn chat_and_responses_streams_commit_translation_and_record_one_aggregated_assistant() {
    let temporary = tempfile::tempdir().expect("应可建立流式 E2E 临时目录");

    for protocol in [StreamingProtocol::Chat, StreamingProtocol::Responses] {
        let root = temporary.path().join(protocol.name());
        fs::create_dir(&root).expect("应可建立协议独立根目录");
        let input = root.join("input");
        fs::create_dir(&input).expect("应可建立 Generic 输入目录");
        fs::write(
            input.join("story.jsonl"),
            format!(
                "{}\n",
                json!({
                    "id": "story",
                    "kind": "dialogue",
                    "units": [{"id": "line", "text": SOURCE}],
                })
            ),
        )
        .expect("应可写入 Generic JSONL");

        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("流式 Provider 应可绑定");
        let endpoint = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().expect("Provider 地址应可读取")
        );
        write_distribution(&root, protocol, &endpoint);
        let project = format!("stream-{}", protocol.name());

        assert_success(
            "Generic Init",
            &run_att(
                &root,
                &[
                    "generic",
                    "init",
                    "--name",
                    &project,
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
            "Generic Extract",
            &run_att(&root, &["generic", "extract", "--name", &project]),
        );

        let assistant = json!({
            "think": protocol.thinking(),
            "translations": {"0": [protocol.translation()]},
        })
        .to_string();
        let server_assistant = assistant.clone();
        let server =
            thread::spawn(move || serve_streaming_response(listener, protocol, &server_assistant));
        let translate = run_att(
            &root,
            &["generic", "translate", "--name", &project, "local"],
        );
        assert_success("Generic streaming Translate", &translate);
        let request = server
            .join()
            .expect("流式 Provider 线程不得 panic")
            .expect("流式 Provider 必须完成请求");

        assert!(
            request
                .request_line
                .starts_with(&format!("POST {} HTTP/1.1", protocol.request_path())),
            "实际请求行：{}",
            request.request_line
        );
        assert_eq!(request.body["stream"], true);
        assert_eq!(request.body["model"], "stream-e2e-model");
        match protocol {
            StreamingProtocol::Chat => {
                assert!(request.body.get("messages").is_some());
                assert!(request.body.get("input").is_none());
            }
            StreamingProtocol::Responses => {
                assert!(request.body.get("messages").is_none());
                assert!(request.body.get("input").is_some());
                assert_eq!(request.body["background"], false);
            }
        }

        let workspace = distribution_root(&root)
            .join("projects/generic")
            .join(&project);
        let translation: Option<String> = Connection::open(workspace.join("project.db"))
            .expect("项目数据库应可打开")
            .query_row("SELECT translation FROM generic_unit", [], |row| row.get(0))
            .expect("应可读取唯一 Generic 译文");
        assert_eq!(translation.as_deref(), Some(protocol.translation()));

        let task_record = read_single_task_record(&workspace);
        assert_eq!(task_record.matches("## Assistant").count(), 1);
        assert_eq!(task_record.matches(&assistant).count(), 1);
        assert!(task_record.contains(protocol.thinking()));
        assert!(task_record.contains("Upstream provider: not provided"));
        assert!(!task_record.contains("data:"));
        assert!(!task_record.contains("response.output_text.delta"));
        assert!(!task_record.contains("[DONE]"));

        let log = read_latest_project_log(&workspace);
        let finished = log
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("日志行必须是 JSON"))
            .find(|record| record["event"] == "task.finished")
            .expect("流式任务必须有终态");
        assert_eq!(finished["payload"]["provider"], Value::Null);
    }
}

#[test]
fn chat_stream_late_content_retries_and_records_the_final_openrouter_provider() {
    let temporary = tempfile::tempdir().expect("应可建立流式重试 E2E 临时目录");
    let root = temporary.path();
    let input = root.join("input");
    fs::create_dir(&input).expect("应可建立 Generic 输入目录");
    fs::write(
        input.join("story.jsonl"),
        format!(
            "{}\n",
            json!({
                "id": "story",
                "kind": "dialogue",
                "units": [{"id": "line", "text": SOURCE}],
            })
        ),
    )
    .expect("应可写入 Generic JSONL");

    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("流式重试 Provider 应可绑定");
    let proxy = format!(
        "http://{}",
        listener.local_addr().expect("Provider 地址应可读取")
    );
    let endpoint = "http://openrouter.ai/v1/chat/completions";
    write_distribution_with_retries(root, StreamingProtocol::Chat, endpoint, "[1]", Some(&proxy));
    let project = "stream-chat-retry";
    assert_success(
        "Generic retry Init",
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
        "Generic retry Extract",
        &run_att(root, &["generic", "extract", "--name", project]),
    );

    let assistant = json!({
        "think": "retry-complete",
        "translations": {"0": ["重试后的流式译文"]},
    })
    .to_string();
    let server_assistant = assistant.clone();
    let server = thread::spawn(move || serve_late_then_valid_chat(listener, &server_assistant));
    assert_success(
        "Generic retry streaming Translate",
        &run_att(root, &["generic", "translate", "--name", project, "local"]),
    );
    let requests = server
        .join()
        .expect("流式重试 Provider 线程不得 panic")
        .expect("两次流式 attempt 都必须完成");
    assert_eq!(requests.len(), 2);
    for request in requests {
        assert!(
            request
                .headers
                .to_ascii_lowercase()
                .contains("x-openrouter-metadata: enabled"),
            "OpenRouter attempt 必须显式请求 Router Metadata"
        );
    }

    let workspace = distribution_root(root)
        .join("projects/generic")
        .join(project);
    let translation: Option<String> = Connection::open(workspace.join("project.db"))
        .expect("项目数据库应可打开")
        .query_row("SELECT translation FROM generic_unit", [], |row| row.get(0))
        .expect("应可读取唯一 Generic 译文");
    assert_eq!(translation.as_deref(), Some("重试后的流式译文"));

    let log = read_latest_project_log(&workspace);
    let finished = log
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("日志行必须是 JSON"))
        .find(|record| record["event"] == "task.finished")
        .expect("流式重试任务必须有终态");
    assert_eq!(finished["payload"]["attempts"], 2);
    assert_eq!(finished["payload"]["provider"], "FinalProvider");
    assert_eq!(finished["payload"]["outcome"]["kind"], "complete");
    let message = finished["message"]
        .as_str()
        .expect("task.finished message 应为字符串");
    assert!(message.contains("Upstream provider:"));
    assert!(message.contains("FinalProvider"));
    assert!(!log.contains("FirstProvider"));

    let task_record = read_single_task_record(&workspace);
    assert!(task_record.contains("Upstream provider:"));
    assert!(task_record.contains("FinalProvider"));
    assert!(!task_record.contains("FirstProvider"));
}

fn write_distribution(root: &Path, protocol: StreamingProtocol, endpoint: &str) {
    write_distribution_with_retries(root, protocol, endpoint, "[]", None);
}

fn write_distribution_with_retries(
    root: &Path,
    protocol: StreamingProtocol,
    endpoint: &str,
    retry_delays: &str,
    proxy: Option<&str>,
) {
    let protocol_line = match protocol {
        StreamingProtocol::Chat => String::new(),
        StreamingProtocol::Responses => "protocol = \"responses\"\n".to_owned(),
    };
    let proxy = proxy.map_or_else(|| "false".to_owned(), |url| format!("\"{url}\""));
    let distribution = distribution_root(root);
    fs::create_dir_all(&distribution).expect("发行目录应可建立");
    fs::write(
        distribution.join("config.toml"),
        format!(
            r#"[prompts]
thinking_output = true
source_echo = false

[llm.clients.streaming]
{protocol_line}url = "{endpoint}"
api_key = "stream-e2e-secret"
model = "stream-e2e-model"
stream = true
max_concurrent_requests = 1
connect_timeout_ms = 5000
read_timeout_ms = 10000
request_timeout_ms = 10000
proxy = {proxy}
additional_pem_files = []
retry_delays_ms = {retry_delays}
max_retry_after_ms = 1000
parameters = '''
{{}}
'''

[[languages]]
type = "japanese"
id = "ja"
minimum_kana_characters = 1
allowed_terms = []

[translation]
record_translation_tasks = true

[[translation.profiles]]
id = "local"
llm_client = "streaming"
target_task_user_message_characters = 10000
"#,
        ),
    )
    .expect("流式配置应可写入");

    let prompt = distribution.join("prompts/translation");
    fs::create_dir_all(prompt.join("rules")).expect("Prompt rules 目录应可建立");
    fs::create_dir_all(prompt.join("examples")).expect("Prompt examples 目录应可建立");
    fs::write(
        prompt.join("system.md"),
        "把 {{source_language}} 翻译成 {{target_language}}。",
    )
    .expect("System Prompt 应可写入");
    fs::write(prompt.join("thinking.md"), "先判断语义再输出译文。")
        .expect("Thinking Prompt 应可写入");
    fs::write(
        prompt.join("rules/thinking.md"),
        "只输出带 think 和 translations 的 JSON object。",
    )
    .expect("Thinking 规则应可写入");
    fs::write(
        prompt.join("examples/thinking.md"),
        "# 示例\n\n输入：{}\n\n输出：{\"think\":\"判断\",\"translations\":{}}",
    )
    .expect("Thinking 示例应可写入");
}

fn serve_streaming_response(
    listener: TcpListener,
    protocol: StreamingProtocol,
    assistant: &str,
) -> Result<ObservedRequest, String> {
    let (mut stream, _) = listener
        .accept()
        .map_err(|error| format!("接受流式请求失败：{error}"))?;
    let request = read_http_request(&mut stream)?;
    let sse = match protocol {
        StreamingProtocol::Chat => chat_sse(assistant),
        StreamingProtocol::Responses => responses_sse(assistant),
    };
    write_chunked_sse(&mut stream, &sse)?;
    Ok(request)
}

fn serve_late_then_valid_chat(
    listener: TcpListener,
    assistant: &str,
) -> Result<Vec<ObservedRequest>, String> {
    let (mut first, _) = listener
        .accept()
        .map_err(|error| format!("接受首次流式请求失败：{error}"))?;
    let first_request = read_http_request(&mut first)?;
    write_chunked_sse(
        &mut first,
        &chat_sse_with_provider(assistant, Some("FirstProvider"), true),
    )?;

    let (mut second, _) = listener
        .accept()
        .map_err(|error| format!("接受重试流式请求失败：{error}"))?;
    let second_request = read_http_request(&mut second)?;
    write_chunked_sse(
        &mut second,
        &chat_sse_with_provider(assistant, Some("FinalProvider"), false),
    )?;
    Ok(vec![first_request, second_request])
}

fn chat_sse(assistant: &str) -> Vec<u8> {
    chat_sse_with_provider(assistant, None, false)
}

fn chat_sse_with_provider(assistant: &str, provider: Option<&str>, late_content: bool) -> Vec<u8> {
    let split_a = utf8_boundary_at_or_after(assistant, assistant.len() / 3);
    let split_b = utf8_boundary_at_or_after(assistant, assistant.len() * 2 / 3);
    let mut payload = Vec::new();
    for fragment in [
        &assistant[..split_a],
        &assistant[split_a..split_b],
        &assistant[split_b..],
    ] {
        let event = json!({
            "choices": [{
                "index": 0,
                "delta": {"content": fragment},
                "finish_reason": null,
            }],
        });
        payload.extend_from_slice(format!("data: {event}\n\n").as_bytes());
    }
    payload.extend_from_slice(
        format!(
            "data: {}\n\n",
            json!({
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "stop",
                }],
            })
        )
        .as_bytes(),
    );
    payload.extend_from_slice(
        format!(
            "data: {}\n\n",
            json!({
                "choices": [{
                    "index": 0,
                    "delta": {"role": "assistant", "content": null, "refusal": ""},
                    "finish_reason": null,
                }],
            })
        )
        .as_bytes(),
    );
    if let Some(provider) = provider {
        payload.extend_from_slice(
            format!(
                "data: {}\n\n",
                json!({
                    "choices": [],
                    "openrouter_metadata": {
                        "endpoints": {"available": [
                            {"provider": provider, "selected": true}
                        ]}
                    }
                })
            )
            .as_bytes(),
        );
    }
    if late_content {
        payload.extend_from_slice(
            format!(
                "data: {}\n\n",
                json!({
                    "choices": [{
                        "index": 0,
                        "delta": {"content": "late"},
                        "finish_reason": null,
                    }],
                })
            )
            .as_bytes(),
        );
    }
    payload.extend_from_slice(b"data: [DONE]\n\n");
    payload
}

fn responses_sse(assistant: &str) -> Vec<u8> {
    let created = json!({
        "type": "response.created",
        "response": {"id": "stream-response"},
    });
    let delta = json!({
        "type": "response.output_text.delta",
        "delta": &assistant[..utf8_boundary_at_or_after(assistant, assistant.len() / 2)],
    });
    let completed = json!({
        "type": "response.completed",
        "response": {
            "id": "stream-response",
            "status": "completed",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": assistant}],
            }],
        },
    });
    format!(
        "event: response.created\ndata: {created}\n\n\
         event: response.output_text.delta\ndata: {delta}\n\n\
         event: response.completed\ndata: {completed}\n\n"
    )
    .into_bytes()
}

fn utf8_boundary_at_or_after(text: &str, target: usize) -> usize {
    (target..=text.len())
        .find(|index| text.is_char_boundary(*index))
        .expect("字符串末尾必定是 UTF-8 边界")
}

fn write_chunked_sse(stream: &mut TcpStream, payload: &[u8]) -> Result<(), String> {
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    let mut response = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
    let pattern = [1_usize, 2, 7, 3, 11, 5, 17];
    let mut offset = 0usize;
    let mut index = 0usize;
    while offset < payload.len() {
        let end = (offset + pattern[index % pattern.len()]).min(payload.len());
        let chunk = &payload[offset..end];
        write!(response, "{:X}\r\n", chunk.len()).map_err(|error| error.to_string())?;
        response.extend_from_slice(chunk);
        response.extend_from_slice(b"\r\n");
        offset = end;
        index += 1;
    }
    response.extend_from_slice(b"0\r\n\r\n");
    stream
        .write_all(&response)
        .and_then(|()| stream.flush())
        .map_err(|error| error.to_string())?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

fn read_http_request(stream: &mut TcpStream) -> Result<ObservedRequest, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
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
    let header = std::str::from_utf8(&bytes[..header_end - 4])
        .map_err(|error| format!("HTTP header 不是 UTF-8：{error}"))?
        .to_owned();
    let request_line = header
        .lines()
        .next()
        .ok_or_else(|| "HTTP 请求缺少 request line".to_owned())?
        .to_owned();
    let content_length = header
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .ok_or_else(|| "HTTP 请求缺少 Content-Length".to_owned())?;
    while bytes.len() < header_end + content_length {
        let read = stream
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("HTTP 请求 body 提前结束".to_owned());
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    let body = serde_json::from_slice(&bytes[header_end..header_end + content_length])
        .map_err(|error| format!("HTTP 请求 body 不是 JSON：{error}"))?;
    Ok(ObservedRequest {
        request_line,
        headers: header,
        body,
    })
}

fn read_latest_project_log(workspace: &Path) -> String {
    fs::read_to_string(
        fs::read_dir(workspace.join("logs"))
            .expect("日志目录应可读取")
            .map(|entry| entry.expect("日志目录项应可读取").path())
            .max()
            .expect("Translate 日志必须存在"),
    )
    .expect("Translate 日志应可读取")
}

fn read_single_task_record(workspace: &Path) -> String {
    let root = workspace.join("task-records");
    let runs = fs::read_dir(&root)
        .expect("任务记录根应存在")
        .collect::<Result<Vec<_>, _>>()
        .expect("任务记录运行目录应可读取");
    assert_eq!(runs.len(), 1, "一次 Translate 应只有一个任务记录运行目录");
    let files = fs::read_dir(runs[0].path())
        .expect("任务记录运行目录应可读取")
        .collect::<Result<Vec<_>, _>>()
        .expect("任务记录文件应可读取");
    assert_eq!(files.len(), 1, "单个 TaskBlock 应只有一个任务记录");
    assert_eq!(files[0].file_name(), OsString::from("task-000001.md"));
    fs::read_to_string(files[0].path()).expect("任务记录 Markdown 应可读取")
}

fn run_att(root: &Path, arguments: &[&str]) -> Output {
    Command::new(stage_att_executable(root))
        .current_dir(root)
        .args(["--ui-language", "en"])
        .args(arguments)
        .output()
        .expect("att.exe 应可执行")
}

fn stage_att_executable(root: &Path) -> PathBuf {
    let source = Path::new(env!("CARGO_BIN_EXE_att"));
    let release = distribution_root(root);
    fs::create_dir_all(&release).expect("测试发行目录应可建立");
    let executable = release.join("att.exe");
    if !executable.exists() {
        fs::copy(source, &executable).expect("测试 att.exe 应可复制到发行目录");
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

fn distribution_root(root: &Path) -> PathBuf {
    root.join("release")
}

fn assert_success(stage: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{stage} 应成功\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
