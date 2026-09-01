#![cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]

use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, WaitForSingleObject};

const SERVER_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECTION_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[test]
fn root_test_checks_each_unique_client_once_without_touching_projects() {
    let root = tempfile::tempdir().expect("应创建测试目录");
    let listener = TcpListener::bind("[::1]:0").expect("应建立 Mock HTTP 服务");
    let port = listener.local_addr().expect("应取得监听地址").port();
    let server = MockServer::responses(
        listener,
        vec![
            (
                200,
                r#"{"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"OK"}]}]}"#,
            ),
            (
                200,
                r#"{"choices":[{"index":0,"finish_reason":"stop","message":{"content":"OK"}}]}"#,
            ),
        ],
    );

    let distribution = root.path().join("distribution");
    fs::create_dir_all(distribution.join("projects")).expect("应建立发行目录");
    fs::write(distribution.join("projects").join("keep.txt"), "unchanged")
        .expect("应建立 projects 哨兵");
    let executable = stage_att_executable(&distribution);
    fs::write(distribution.join("config.toml"), test_configuration(port)).expect("应写入测试配置");

    let output = Command::new(executable)
        .args(["--ui-language", "zh-Hans", "test"])
        .output()
        .expect("应运行 att test");
    assert!(output.status.success(), "stderr={}", text(&output.stderr));
    let stdout = text(&output.stdout);
    assert!(stdout.starts_with("配置：通过\n"), "{stdout}");
    let responses = stdout
        .find("LLM a-responses：通过")
        .expect("应输出 Responses 结果");
    let chat = stdout.find("LLM z-chat：通过").expect("应输出 Chat 结果");
    assert!(responses < chat, "Client 应按 ID 稳定顺序执行：{stdout}");
    assert!(
        stdout.contains("汇总：2/2 通过，0 失败，0 未执行"),
        "{stdout}"
    );
    assert!(output.stderr.is_empty(), "stderr={}", text(&output.stderr));
    assert_eq!(
        fs::read_to_string(distribution.join("projects").join("keep.txt"))
            .expect("应读取 projects 哨兵"),
        "unchanged"
    );
    assert_eq!(
        directory_entries(&distribution.join("projects")),
        vec!["keep.txt"]
    );

    let requests = server.finish().expect("Mock 服务应在时限内完成");
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("POST /v1/responses HTTP/1.1"));
    assert!(requests[0].contains("\"input\""));
    assert!(requests[0].contains("\"background\":false"));
    assert!(requests[1].starts_with("POST /v1/chat/completions HTTP/1.1"));
    assert!(requests[1].contains("\"messages\""));
}

#[test]
fn root_test_continues_after_a_client_failure() {
    let root = tempfile::tempdir().expect("应创建测试目录");
    let listener = TcpListener::bind("[::1]:0").expect("应建立 Mock HTTP 服务");
    let port = listener.local_addr().expect("应取得监听地址").port();
    let server = MockServer::responses(
        listener,
        vec![
            (
                500,
                r#"{"error":{"message":"temporary"},"raw":"WIRE_BODY_SENTINEL"}"#,
            ),
            (
                200,
                r#"{"choices":[{"index":0,"finish_reason":"stop","message":{"content":"OK"}}]}"#,
            ),
        ],
    );

    let distribution = root.path().join("distribution");
    fs::create_dir_all(&distribution).expect("应建立发行目录");
    let executable = stage_att_executable(&distribution);
    fs::write(distribution.join("config.toml"), test_configuration(port)).expect("应写入测试配置");
    let output = Command::new(executable)
        .args(["--ui-language", "zh-Hans", "test"])
        .output()
        .expect("应运行 att test");

    assert_eq!(output.status.code(), Some(1));
    let stdout = text(&output.stdout);
    assert!(stdout.contains("LLM a-responses：失败"), "{stdout}");
    assert!(stdout.contains("LLM z-chat：通过"), "{stdout}");
    assert!(
        stdout.contains("汇总：1/2 通过，1 失败，0 未执行"),
        "{stdout}"
    );
    assert!(!text(&output.stderr).contains("WIRE_BODY_SENTINEL"));
    assert_eq!(server.finish().expect("Mock 服务应在时限内完成").len(), 2);
}

#[test]
fn root_test_ctrl_break_finishes_the_active_client_and_skips_the_rest() {
    let root = tempfile::tempdir().expect("应创建测试目录");
    let listener = TcpListener::bind("[::1]:0").expect("应建立 Mock HTTP 服务");
    let port = listener.local_addr().expect("应取得监听地址").port();
    let (server, first_request, release_response) = MockServer::gated_first_response(listener);

    let distribution = root.path().join("distribution");
    fs::create_dir_all(distribution.join("projects")).expect("应建立发行目录");
    fs::write(distribution.join("projects").join("keep.txt"), "unchanged")
        .expect("应建立 projects 哨兵");
    let executable = stage_att_executable(&distribution);
    fs::write(distribution.join("config.toml"), test_configuration(port)).expect("应写入测试配置");

    let mut child = spawn_att_in_new_process_group(&executable, &distribution);
    if let Err(error) = first_request.recv_timeout(SERVER_TIMEOUT) {
        let _ = child.kill();
        let output = child.wait_with_output().expect("ATT 测试子进程应可回收");
        drop(release_response);
        let server_result = server.finish();
        panic!(
            "首个 Client 请求应在时限内到达：{error}\nserver={server_result:?}\nstdout:\n{}\nstderr:\n{}",
            text(&output.stdout),
            text(&output.stderr)
        );
    }
    if let Err(error) = send_ctrl_break(&child) {
        let _ = child.kill();
        let output = child.wait_with_output().expect("ATT 测试子进程应可回收");
        drop(release_response);
        let server_result = server.finish();
        panic!(
            "应能向 ATT 独立进程组发送 Ctrl-Break：{error}\nserver={server_result:?}\nstdout:\n{}\nstderr:\n{}",
            text(&output.stdout),
            text(&output.stderr)
        );
    }
    release_response
        .send(())
        .expect("发送 Ctrl-Break 后应允许首个响应返回");
    let output =
        wait_for_child(child, Duration::from_secs(15)).expect("att test 应在合作取消后及时结束");
    let requests = server.finish().expect("取消 Mock 服务应在时限内结束");

    assert_eq!(output.status.code(), Some(130));
    let stdout = text(&output.stdout);
    assert!(stdout.contains("LLM a-responses：通过"), "{stdout}");
    assert!(!stdout.contains("LLM z-chat"), "{stdout}");
    assert!(
        stdout.contains("汇总：1/2 通过，0 失败，1 未执行"),
        "{stdout}"
    );
    assert!(
        text(&output.stderr).contains("命令已在安全收尾后取消"),
        "stderr={}",
        text(&output.stderr)
    );
    assert_eq!(requests.len(), 1, "第二个 Client 不应建立连接");
    assert_eq!(
        fs::read_to_string(distribution.join("projects").join("keep.txt"))
            .expect("应读取 projects 哨兵"),
        "unchanged"
    );
    assert_eq!(
        directory_entries(&distribution.join("projects")),
        vec!["keep.txt"]
    );
}

#[test]
fn root_test_rejects_an_empty_client_catalog() {
    let root = tempfile::tempdir().expect("应创建测试目录");
    let distribution = root.path().join("distribution");
    fs::create_dir_all(&distribution).expect("应建立发行目录");
    let executable = stage_att_executable(&distribution);
    fs::write(distribution.join("config.toml"), "[llm.clients]\n").expect("应写入空 Client 配置");
    let output = Command::new(executable)
        .args(["--ui-language", "zh-Hans", "test"])
        .output()
        .expect("应运行 att test");

    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output.stdout).contains("配置：失败"));
    assert!(text(&output.stderr).contains("llm.clients"));
}

struct MockServer {
    stop: Arc<AtomicBool>,
    result: mpsc::Receiver<Result<Vec<String>, String>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl MockServer {
    fn responses(listener: TcpListener, responses: Vec<(u16, &'static str)>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (result_tx, result) = mpsc::channel();
        let thread = thread::spawn(move || {
            let outcome = run_response_server(&listener, &responses, &worker_stop);
            let _ = result_tx.send(outcome);
        });
        Self {
            stop,
            result,
            thread: Some(thread),
        }
    }

    fn gated_first_response(listener: TcpListener) -> (Self, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (result_tx, result) = mpsc::channel();
        let (first_request_tx, first_request) = mpsc::channel();
        let (release_response, release_response_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            let outcome = run_gated_server(
                &listener,
                &worker_stop,
                &first_request_tx,
                &release_response_rx,
            );
            let _ = result_tx.send(outcome);
        });
        (
            Self {
                stop,
                result,
                thread: Some(thread),
            },
            first_request,
            release_response,
        )
    }

    fn finish(mut self) -> Result<Vec<String>, String> {
        let result = self
            .result
            .recv_timeout(SERVER_TIMEOUT + Duration::from_secs(3))
            .map_err(|error| format!("Mock 服务未在时限内结束：{error}"));
        self.stop.store(true, Ordering::Release);
        let joined = self
            .thread
            .take()
            .expect("Mock 服务线程必须存在")
            .join()
            .map_err(|_| "Mock 服务线程发生 panic".to_owned());
        joined?;
        result?
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

fn run_response_server(
    listener: &TcpListener,
    responses: &[(u16, &'static str)],
    stop: &AtomicBool,
) -> Result<Vec<String>, String> {
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("无法启用非阻塞 Mock listener：{error}"))?;
    let deadline = Instant::now() + SERVER_TIMEOUT;
    let mut requests = Vec::with_capacity(responses.len());
    for &(status, response) in responses {
        let Some(mut stream) = poll_connection(listener, stop, deadline)? else {
            return Err(format!(
                "Mock 服务只收到 {}/{} 个请求",
                requests.len(),
                responses.len()
            ));
        };
        configure_stream(&stream)?;
        requests.push(read_request(&mut stream)?);
        write_response(&mut stream, status, response)?;
    }
    Ok(requests)
}

fn run_gated_server(
    listener: &TcpListener,
    stop: &AtomicBool,
    first_request: &mpsc::Sender<()>,
    release_response: &mpsc::Receiver<()>,
) -> Result<Vec<String>, String> {
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("无法启用非阻塞 Mock listener：{error}"))?;
    let Some(mut stream) = poll_connection(listener, stop, Instant::now() + SERVER_TIMEOUT)? else {
        return Err("首个 Client 没有在时限内连接 Mock 服务".to_owned());
    };
    configure_stream(&stream)?;
    let mut requests = vec![read_request(&mut stream)?];
    first_request
        .send(())
        .map_err(|error| format!("无法报告首个请求到达：{error}"))?;
    release_response
        .recv_timeout(SERVER_TIMEOUT)
        .map_err(|error| format!("未在时限内收到首个响应放行信号：{error}"))?;
    write_response(
        &mut stream,
        200,
        r#"{"status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"OK"}]}]}"#,
    )?;

    if let Some(mut second) =
        poll_connection(listener, stop, Instant::now() + Duration::from_secs(1))?
    {
        configure_stream(&second)?;
        requests.push(read_request(&mut second)?);
        write_response(
            &mut second,
            200,
            r#"{"choices":[{"index":0,"finish_reason":"stop","message":{"content":"OK"}}]}"#,
        )?;
    }
    Ok(requests)
}

fn poll_connection(
    listener: &TcpListener,
    stop: &AtomicBool,
    deadline: Instant,
) -> Result<Option<TcpStream>, String> {
    loop {
        if stop.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Ok(None);
        }
        match listener.accept() {
            Ok((stream, _)) => return Ok(Some(stream)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(CONNECTION_POLL_INTERVAL);
            }
            Err(error) => return Err(format!("Mock listener 接收连接失败：{error}")),
        }
    }
}

fn configure_stream(stream: &TcpStream) -> Result<(), String> {
    let timeout = Some(Duration::from_secs(3));
    stream
        .set_read_timeout(timeout)
        .map_err(|error| format!("无法设置 Mock 读取超时：{error}"))?;
    stream
        .set_write_timeout(timeout)
        .map_err(|error| format!("无法设置 Mock 写入超时：{error}"))
}

fn write_response(stream: &mut TcpStream, status: u16, response: &str) -> Result<(), String> {
    let reply = format!(
        "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response.len(),
        response
    );
    stream
        .write_all(reply.as_bytes())
        .map_err(|error| format!("无法写入 Mock 响应：{error}"))
}

fn spawn_att_in_new_process_group(executable: &Path, current_directory: &Path) -> Child {
    let mut command = Command::new(executable);
    command
        .current_dir(current_directory)
        .args(["--ui-language", "zh-Hans", "test"])
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
                text(&output.stdout),
                text(&output.stderr)
            ))
        }
        status => {
            let _ = child.kill();
            let _ = child.wait();
            Err(format!("等待 ATT 子进程失败，Windows wait status={status}"))
        }
    }
}

fn stage_att_executable(distribution: &Path) -> PathBuf {
    let source = Path::new(env!("CARGO_BIN_EXE_att"));
    let executable = distribution.join("att.exe");
    fs::copy(source, &executable).expect("测试 att.exe 应可复制到独立发行目录");
    executable
}

fn test_configuration(port: u16) -> String {
    format!(
        r#"[llm.clients.z-chat]
protocol = "chat_completions"
url = "http://[::1]:{port}/v1"
api_key = "chat-key"
model = "test-model"
stream = false
max_concurrent_requests = 1
connect_timeout_ms = 2000
read_timeout_ms = 2000
request_timeout_ms = 2000
proxy = false
additional_pem_files = []
retry_delays_ms = [1, 1]
max_retry_after_ms = 1
parameters = '''{{"temperature":0}}'''

[llm.clients.a-responses]
protocol = "responses"
url = "http://[::1]:{port}/v1"
api_key = "responses-key"
model = "test-model"
stream = false
max_concurrent_requests = 1
connect_timeout_ms = 2000
read_timeout_ms = 2000
request_timeout_ms = 2000
proxy = false
additional_pem_files = []
retry_delays_ms = [1, 1]
max_retry_after_ms = 1
parameters = '''{{"temperature":0}}'''
"#
    )
}

fn read_request(stream: &mut TcpStream) -> Result<String, String> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let count = stream
            .read(&mut buffer)
            .map_err(|error| format!("读取 Mock 请求 Header 失败：{error}"))?;
        if count == 0 {
            return Err("请求在完整 HTTP Header 前结束".to_owned());
        }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = find_bytes(&bytes, b"\r\n\r\n") {
            break end + 4;
        }
    };
    let header = String::from_utf8_lossy(&bytes[..header_end]);
    let content_length = header
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(str::trim)
                .map(|value| value.parse::<usize>())
        })
        .ok_or_else(|| "请求缺少 Content-Length".to_owned())?
        .map_err(|error| format!("Content-Length 无效：{error}"))?;
    while bytes.len() < header_end + content_length {
        let count = stream
            .read(&mut buffer)
            .map_err(|error| format!("读取 Mock 请求正文失败：{error}"))?;
        if count == 0 {
            return Err("请求正文提前结束".to_owned());
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    String::from_utf8(bytes).map_err(|error| format!("测试请求不是 UTF-8：{error}"))
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn directory_entries(path: &Path) -> Vec<String> {
    let mut entries = fs::read_dir(path)
        .expect("应读取目录")
        .map(|entry| {
            entry
                .expect("目录项应有效")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .replace("\r\n", "\n")
        .replace(['\u{2068}', '\u{2069}'], "")
}
