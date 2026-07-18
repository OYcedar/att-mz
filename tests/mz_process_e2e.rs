#![cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]

//! Windows x64 生产进程边界的 MZ 纵向黑盒测试。

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use uuid::Uuid;
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};

const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const WSA_WOULD_BLOCK: i32 = 10035;

const PROJECT: &str = "e2e";
const PROFILE: &str = "local";
const SOURCE_TEXT: &str = "薬草です";
const UPDATED_SOURCE_TEXT: &str = "上薬草です";
const TRANSLATION: &str = "治疗药草";
const SYSTEM_PROMPT: &str = "E2E SYSTEM CONTRACT";
const UPDATED_SYSTEM_PROMPT: &str = "E2E SYSTEM CONTRACT UPDATED";
const JS_MARKER: &str = "/* ATT MZ process e2e */";
const EXPECTED_USER_MESSAGE: &str =
    "# 翻译任务\n\n\n# 文本\n\n## 语义组 1 · 数据库对象\n\n### [0] description\n> 薬草です\n";
const EXTRACT_LUA: &str = "scripts/extract.lua";
const TRANSLATE_LUA: &str = "scripts/translate.lua";
const WRITE_BACK_LUA: &str = "scripts/write_back.lua";
const RULES_JSON: &str = "rules.json";
const TERMS_JSON: &str = "terms.json";
const API_KEY: &str = "e2e-secret";
const E2E_EXTRA_SECRET: &str = "e2e-extra-secret";
const LEAK_SENTINEL: &str = "e2e-secret-must-not-leak";
const EMPTY_PARAMETERS: &str = "{}";
const E2E_PARAMETERS: &str = r#"{"temperature":0.0,"provider_extension":{"mode":"e2e","private_marker":"e2e-extra-secret"}}"#;

#[test]
fn init_extract_translate_and_write_back_cross_process_with_real_roots() {
    let temporary = tempfile::tempdir().expect("应可建立端到端测试目录");
    let root = temporary.path();
    let game_root = root.join("game");
    let projects_root = root.join("projects");
    let logs_root = root.join("logs");
    let prompt_root = root.join("prompts");
    fs::create_dir(&projects_root).expect("项目根应可建立");
    fs::create_dir(&logs_root).expect("日志根应可建立");
    fs::create_dir(&prompt_root).expect("提示词目录应可建立");
    write_minimal_mz_game(&game_root);
    write_lua_scripts(root);

    let cancellation_server = BoundCancellationChatServer::bind();
    write_configuration(root, cancellation_server.endpoint(), EMPTY_PARAMETERS);

    let init_arguments = mz_init_arguments(&game_root);
    let init = run_att(root, init_arguments.clone());
    let init_stdout = assert_success("init", &init);
    assert_eq!(init_stdout, "初始化完成：e2e\n项目状态：已创建\n");

    let workspace = projects_root.join(PROJECT);
    let database = workspace.join("project.db");
    assert!(workspace.join("source/data/Items.json").is_file());
    assert!(workspace.join("source/js/plugins.js").is_file());
    assert!(workspace.join("write_back/data").is_dir());
    assert!(workspace.join("write_back/js").is_dir());
    assert_metadata(&database);

    let extract = run_att(
        root,
        arguments(&[
            "mz",
            "extract",
            "--name",
            PROJECT,
            "--builtin",
            "--lua",
            EXTRACT_LUA,
        ]),
    );
    let extract_stdout = assert_success("extract", &extract);
    assert_eq!(extract_stdout, "提取完成：e2e\n");
    assert_extracted_database(&database);
    assert_lua_probes(&database, &["extract"]);

    fs::write(prompt_root.join("ja-zh.md"), SYSTEM_PROMPT).expect("系统提示词应可写入");
    let mut running_cancellation_server = cancellation_server.start();
    let cancelled_child = spawn_att_in_new_process_group(
        root,
        arguments(&["mz", "translate", "--name", PROJECT, PROFILE]),
    );
    running_cancellation_server.wait_until_request();
    // SAFETY: 子进程使用 CREATE_NEW_PROCESS_GROUP 且继承当前控制台；只向其进程组
    // 发送 CTRL_BREAK，不会把测试进程包含在目标组内。
    let generated = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, cancelled_child.id()) };
    assert_ne!(generated, 0, "应能向 att.exe 独立进程组发送 Ctrl-Break");
    let cancelled_request = running_cancellation_server.respond_and_finish();
    assert_exact_minimal_chat_request(&cancelled_request);
    let cancelled = wait_for_att(cancelled_child);
    assert_eq!(cancelled.status.code(), Some(130));
    assert!(cancelled.stdout.is_empty(), "Ctrl-C 不得打印业务完成文案");
    assert!(cancelled.stderr.is_empty(), "正常合作取消不应产生技术错误");
    assert_translation_absent(&database);

    let server = BoundChatServer::bind();
    write_configuration(root, server.endpoint(), E2E_PARAMETERS);
    let running_server = server.start();
    let translate = run_att(
        root,
        arguments(&[
            "mz",
            "translate",
            "--name",
            PROJECT,
            PROFILE,
            "--lua",
            TRANSLATE_LUA,
        ]),
    );
    let requests = running_server.finish();
    let translate_stdout = assert_success("translate", &translate);
    assert_eq!(
        translate_stdout,
        "翻译执行完成：e2e（Profile：local）\n标准翻译：任务 1，完整 1，部分 0，不可用 0；写入 1 处，剩余 0 处\n状态收敛：保留 0，失效 0，不适用 0，复用 0\nLua 翻译：已执行\n"
    );
    assert_eq!(requests.len(), 2, "Standard 与 Lua 必须各发出一个请求");
    assert_exact_standard_chat_request(&requests[0]);
    assert_exact_lua_chat_request(&requests[1]);
    assert_translation_committed(&database);
    assert_lua_probes(&database, &["extract", "translate"]);

    let write_back = run_att(
        root,
        arguments(&[
            "mz",
            "write-back",
            "--name",
            PROJECT,
            "--lua",
            WRITE_BACK_LUA,
        ]),
    );
    let write_back_stdout = assert_success("write-back", &write_back);
    assert!(write_back_stdout.starts_with("写回完成：e2e\n输出目录："));
    assert!(write_back_stdout.contains(
        "标准写回：应用译文 1 处，保留原文 0 处；自动换行 0 段，新增换行 0 处；续行全角缩进 0 处；需人工换行 0 段\n"
    ));
    assert!(write_back_stdout.ends_with("Lua 写回：已执行\n"));

    let output_root = workspace.join("write_back");
    assert_written_game(&workspace, &output_root);
    assert_lua_probes(&database, &["extract", "translate", "write_back"]);
    assert_json_lines(&logs_root, &output_root);

    let unchanged_init = run_att(root, init_arguments.clone());
    assert_eq!(
        assert_success("repeated init", &unchanged_init),
        "初始化完成：e2e\n项目状态：无变化\n"
    );
    assert!(
        output_root.join("js/lua-probe.txt").is_file(),
        "完全相同的 Init 必须保留既有写回输出"
    );

    let leaf_before_reextract = read_translation_leaf(&database);
    let repeated_extract = run_att(
        root,
        arguments(&["mz", "extract", "--name", PROJECT, "--builtin"]),
    );
    assert_eq!(
        assert_success("repeated builtin extract", &repeated_extract),
        "提取完成：e2e\n"
    );
    assert_eq!(
        read_translation_leaf(&database),
        leaf_before_reextract,
        "完全相同的 Builtin 快照必须精确继承译文与 translation_state"
    );

    install_translation_write_guard(&database);
    let repeated_translate = run_att(
        root,
        arguments(&["mz", "translate", "--name", PROJECT, PROFILE]),
    );
    remove_translation_write_guard(&database);
    assert_eq!(
        assert_success("converged translate", &repeated_translate),
        "翻译执行完成：e2e（Profile：local）\n标准翻译：任务 0，完整 0，部分 0，不可用 0；写入 0 处，剩余 0 处\n状态收敛：保留 1，失效 0，不适用 0，复用 0\n"
    );
    assert_eq!(
        read_translation_leaf(&database),
        leaf_before_reextract,
        "完全收敛的 Translate 不得发出请求，也不得重写译文或 translation_state"
    );

    let first_output_snapshot = read_output_tree(&output_root);
    let repeated_write_back = run_att(
        root,
        arguments(&[
            "mz",
            "write-back",
            "--name",
            PROJECT,
            "--lua",
            WRITE_BACK_LUA,
        ]),
    );
    assert_success("repeated write-back", &repeated_write_back);
    assert_eq!(
        read_output_tree(&output_root),
        first_output_snapshot,
        "相同项目状态与相同 Lua 必须重建出逐字一致的完整输出树"
    );

    write_items_source(&game_root, UPDATED_SOURCE_TEXT);
    let updated_init = run_att(root, init_arguments.clone());
    assert_eq!(
        assert_success("source-updated init", &updated_init),
        "初始化完成：e2e\n项目状态：已更新\n需重新提取：Builtin\n"
    );
    assert!(directory_is_empty(&workspace.join("write_back/data")));
    assert!(directory_is_empty(&workspace.join("write_back/js")));
    assert_builtin_owner_is_stale(&database);

    let stale_server = BoundChatServer::bind();
    write_configuration(root, stale_server.endpoint(), E2E_PARAMETERS);
    let stale_requests = stale_server.start_for_requests(0);
    let stale_translate = run_att(
        root,
        arguments(&["mz", "translate", "--name", PROJECT, PROFILE]),
    );
    assert!(stale_requests.finish().is_empty());
    assert_eq!(stale_translate.status.code(), Some(1));
    assert!(stale_translate.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&stale_translate.stderr).contains("标准资产提取已过期：builtin"),
        "来源变化后必须先刷新 active owner：{}",
        String::from_utf8_lossy(&stale_translate.stderr)
    );

    let refreshed_extract = run_att(
        root,
        arguments(&["mz", "extract", "--name", PROJECT, "--builtin"]),
    );
    assert_success("source-refreshed extract", &refreshed_extract);
    assert_builtin_owner_is_fresh(&database);
    assert_translation_for_original(&database, UPDATED_SOURCE_TEXT, None);

    let updated_server = BoundChatServer::bind();
    write_configuration(root, updated_server.endpoint(), E2E_PARAMETERS);
    let convergence_server = updated_server.start_for_requests(3);
    let updated_translate = run_att(
        root,
        arguments(&["mz", "translate", "--name", PROJECT, PROFILE]),
    );
    assert_success("source-refreshed translate", &updated_translate);
    assert_translation_for_original(&database, UPDATED_SOURCE_TEXT, Some(TRANSLATION));

    let updated_write_back = run_att(root, arguments(&["mz", "write-back", "--name", PROJECT]));
    assert_success("source-refreshed write-back", &updated_write_back);
    assert_updated_written_game(&workspace, &output_root);

    let builtin_before_rules = read_translation_leaf(&database);
    write_rules(root, "customShortName");
    let initial_rules_extract = run_att(
        root,
        arguments(&["mz", "extract", "--name", PROJECT, "--rules", RULES_JSON]),
    );
    assert_success("initial rules extract", &initial_rules_extract);
    assert_rules_leaf(&database, "customShortName", "Potion");

    write_rules(root, "customLongName");
    let updated_rules_extract = run_att(
        root,
        arguments(&["mz", "extract", "--name", PROJECT, "--rules", RULES_JSON]),
    );
    assert_success("updated rules extract", &updated_rules_extract);
    assert_rules_leaf(&database, "customLongName", "Restorative Potion");
    assert_eq!(
        read_translation_leaf(&database),
        builtin_before_rules,
        "Rules owner 的精确替换不得扰动 Builtin 叶"
    );

    write_terminology(root);
    let before_terminology = read_translation_leaf(&database);
    let terminology_translate = run_att(
        root,
        arguments(&[
            "mz",
            "translate",
            "--name",
            PROJECT,
            PROFILE,
            "--terms",
            TERMS_JSON,
        ]),
    );
    let terminology_stdout =
        assert_success("terminology-updated translate", &terminology_translate);
    assert!(terminology_stdout.contains("任务 1"));
    assert!(terminology_stdout.contains("失效 1"));
    let after_terminology = read_translation_leaf(&database);
    assert_eq!(after_terminology.1, TRANSLATION);
    assert_ne!(
        after_terminology.2, before_terminology.2,
        "实际触发的术语变化必须更新逐叶语义状态"
    );
    assert_persisted_terminology(&database);

    fs::write(prompt_root.join("ja-zh.md"), UPDATED_SYSTEM_PROMPT)
        .expect("更新后的系统提示词应可写入");
    let before_profile_semantics = read_translation_leaf(&database);
    let profile_semantics_translate = run_att(
        root,
        arguments(&["mz", "translate", "--name", PROJECT, PROFILE]),
    );
    let profile_stdout =
        assert_success("profile-semantics translate", &profile_semantics_translate);
    assert!(profile_stdout.contains("任务 1"));
    assert!(profile_stdout.contains("失效 1"));
    let after_profile_semantics = read_translation_leaf(&database);
    assert_eq!(after_profile_semantics.1, TRANSLATION);
    assert_ne!(
        after_profile_semantics.2, before_profile_semantics.2,
        "实际 system prompt 内容变化必须更新逐叶语义状态"
    );
    assert_persisted_terminology(&database);

    let convergence_requests = convergence_server.finish();
    assert_eq!(convergence_requests.len(), 3);
    assert_standard_request_semantics(
        &convergence_requests[0],
        SYSTEM_PROMPT,
        &[UPDATED_SOURCE_TEXT],
    );
    assert_standard_request_semantics(
        &convergence_requests[1],
        SYSTEM_PROMPT,
        &[UPDATED_SOURCE_TEXT, "上薬草", "高级药草"],
    );
    assert_standard_request_semantics(
        &convergence_requests[2],
        UPDATED_SYSTEM_PROMPT,
        &[UPDATED_SOURCE_TEXT, "上薬草", "高级药草"],
    );

    let before_layout = read_translation_leaf(&database);
    let layout_init = run_att(root, mz_init_arguments_with_layout(&game_root, 24, 30, 2));
    assert_eq!(
        assert_success("layout-updated init", &layout_init),
        "初始化完成：e2e\n项目状态：已更新\n"
    );
    assert_layout_metadata(&database, 24, 30, 2);
    assert_eq!(
        read_translation_leaf(&database),
        before_layout,
        "只改变布局必须保留标准译文及其语义状态"
    );
    assert_persisted_terminology(&database);
    assert!(directory_is_empty(&workspace.join("write_back/data")));
    assert!(directory_is_empty(&workspace.join("write_back/js")));

    let layout_write_back = run_att(root, arguments(&["mz", "write-back", "--name", PROJECT]));
    let layout_stdout = assert_success("layout-updated write-back", &layout_write_back);
    assert!(layout_stdout.contains("需人工换行 1 段"));
    assert_last_write_back_log(&logs_root, 2, false);

    write_updated_write_back_lua(root);
    let updated_lua_write_back = run_att(
        root,
        arguments(&[
            "mz",
            "write-back",
            "--name",
            PROJECT,
            "--lua",
            WRITE_BACK_LUA,
        ]),
    );
    let updated_lua_stdout = assert_success("updated-lua write-back", &updated_lua_write_back);
    assert!(updated_lua_stdout.ends_with("Lua 写回：已执行\n"));
    assert_eq!(
        fs::read_to_string(output_root.join("js/lua-probe.txt"))
            .expect("更新后的 Lua 候选产物应可读取"),
        "write-back candidate v2"
    );
    assert_write_back_lua_probe(&database, "|v2");
    assert_last_write_back_log(&logs_root, 2, true);

    fs::remove_file(prompt_root.join("ja-zh.md")).expect("应删除已消费的提示词夹具");

    let failed = run_att(
        root,
        arguments(&["mz", "translate", "--name", PROJECT, "missing-profile"]),
    );
    assert_eq!(failed.status.code(), Some(1));
    assert!(failed.stdout.is_empty(), "命令失败不得打印成功文案");
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("missing-profile"),
        "失败应呈现未找到的 Profile"
    );
    for (phase, output) in [
        ("init", &init),
        ("extract", &extract),
        ("cancelled translate", &cancelled),
        ("translate", &translate),
        ("write-back", &write_back),
        ("repeated init", &unchanged_init),
        ("repeated extract", &repeated_extract),
        ("repeated translate", &repeated_translate),
        ("repeated write-back", &repeated_write_back),
        ("updated init", &updated_init),
        ("stale translate", &stale_translate),
        ("refreshed extract", &refreshed_extract),
        ("refreshed translate", &updated_translate),
        ("refreshed write-back", &updated_write_back),
        ("initial rules extract", &initial_rules_extract),
        ("updated rules extract", &updated_rules_extract),
        ("terminology translate", &terminology_translate),
        ("profile semantics translate", &profile_semantics_translate),
        ("layout init", &layout_init),
        ("layout write-back", &layout_write_back),
        ("updated Lua write-back", &updated_lua_write_back),
        ("failed translate", &failed),
    ] {
        assert_process_output_does_not_contain_client_secrets(phase, output);
    }
}

#[test]
fn malformed_configuration_does_not_echo_api_key() {
    let temporary = tempfile::tempdir().expect("应可建立密钥泄漏测试目录");
    let root = temporary.path();
    fs::write(
        root.join("config.toml"),
        format!(
            r#"[llm.clients.leak-probe]
url = "https://example.invalid/v1/chat/completions"
api_key = "{LEAK_SENTINEL}" "invalid"
"#
        ),
    )
    .expect("无效配置夹具应可写入");

    let output = run_att(
        root,
        arguments(&["mz", "extract", "--name", PROJECT, "--builtin"]),
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty(), "配置失败不得打印成功文案");
    let stderr = String::from_utf8(output.stderr).expect("stderr 必须是 UTF-8");
    assert!(!stderr.is_empty(), "配置失败必须呈现诊断");
    assert!(
        !stderr.contains(LEAK_SENTINEL),
        "配置语法错误不得回显 API key：{stderr}"
    );
}

fn arguments(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

fn mz_init_arguments(game_root: &Path) -> Vec<OsString> {
    mz_init_arguments_with_layout(game_root, 24, 30, 40)
}

fn mz_init_arguments_with_layout(
    game_root: &Path,
    dialogue: u32,
    scrolling_text: u32,
    help_description: u32,
) -> Vec<OsString> {
    let mut values = arguments(&["mz", "init", "--name", PROJECT, "--path"]);
    values.push(game_root.as_os_str().to_owned());
    values.extend(arguments(&[
        "--source-language",
        "ja",
        "--target-language",
        "zh-Hans",
        "--dialogue-max-fullwidth-chars",
    ]));
    values.push(dialogue.to_string().into());
    values.push("--scrolling-text-max-fullwidth-chars".into());
    values.push(scrolling_text.to_string().into());
    values.push("--help-description-max-fullwidth-chars".into());
    values.push(help_description.to_string().into());
    values
}

fn run_att(root: &Path, arguments: Vec<OsString>) -> Output {
    let mut command = att_command(root, arguments);
    let child = command.spawn().expect("att.exe 应可启动");
    wait_for_att(child)
}

fn assert_process_output_does_not_contain_client_secrets(phase: &str, output: &Output) {
    for (stream, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
        let text = String::from_utf8_lossy(bytes);
        assert!(
            !text.contains(API_KEY),
            "{phase} 的 {stream} 不得包含配置内 API key：{text}"
        );
        assert!(
            !text.contains(E2E_EXTRA_SECRET),
            "{phase} 的 {stream} 不得包含 parameters 敏感值：{text}"
        );
    }
}

fn spawn_att_in_new_process_group(root: &Path, arguments: Vec<OsString>) -> Child {
    let mut command = att_command(root, arguments);
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    command.spawn().expect("独立进程组中的 att.exe 应可启动")
}

fn att_command(root: &Path, arguments: Vec<OsString>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_att"));
    command
        .current_dir(root)
        .arg("--config")
        .arg(root.join("config.toml"))
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn wait_for_att(mut child: Child) -> Output {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait().expect("应可查询 att.exe 状态").is_some() {
            return child.wait_with_output().expect("应可收集 att.exe 输出");
        }
        if Instant::now() >= deadline {
            child.kill().expect("超时的 att.exe 应可终止");
            let output = child.wait_with_output().expect("应可收集超时进程输出");
            panic!(
                "att.exe 在 30 秒内未退出\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn assert_success(stage: &str, output: &Output) -> String {
    let stdout = String::from_utf8(output.stdout.clone()).expect("stdout 必须是 UTF-8");
    let stderr = String::from_utf8(output.stderr.clone()).expect("stderr 必须是 UTF-8");
    assert_eq!(
        output.status.code(),
        Some(0),
        "{stage} 应以 0 退出\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.is_empty(),
        "{stage} 成功时 stderr 必须为空：{stderr}"
    );
    assert!(!stdout.is_empty(), "{stage} 成功时必须呈现最终结果");
    stdout
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
        fs::write(data.join(file), b"[null]").expect("空标准数据文件应可写入");
    }
    write_items_source(game_root, SOURCE_TEXT);
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
    fs::write(js.join("plugins.js"), JS_MARKER).expect("JS 夹具应可写入");
}

fn write_items_source(game_root: &Path, source_text: &str) {
    fs::write(
        game_root.join("data/Items.json"),
        serde_json::to_vec(&json!([
            null,
            {
                "id": 1,
                "name": "",
                "description": source_text,
                "customShortName": "Potion",
                "customLongName": "Restorative Potion",
                "fixture_marker": true
            }
        ]))
        .expect("Items 夹具应可序列化"),
    )
    .expect("Items 夹具应可写入");
}

fn write_lua_scripts(root: &Path) {
    let scripts = root.join("scripts");
    fs::create_dir(&scripts).expect("Lua 脚本目录应可建立");
    fs::write(
        root.join(EXTRACT_LUA),
        r#"
assert(ctx.phase == "extract")
assert(ctx.project.name == "e2e")
assert(ctx.project.source_language == "ja")
assert(ctx.project.target_language == "zh-Hans")
assert(ctx.project.output_root == nil)
assert(ctx.llm == nil)
assert(type(io.open) == "function")
assert(type(os.execute) == "function")
assert(type(debug.getinfo) == "function")

local metadata = ctx.db.query("SELECT name, source_language, target_language FROM metadata")
assert(#metadata == 1)
assert(metadata[1][1] == "e2e")
assert(metadata[1][2] == "ja")
assert(metadata[1][3] == "zh-Hans")

ctx.db.begin()
ctx.db.execute("CREATE TABLE lua_process_probe (phase TEXT NOT NULL PRIMARY KEY, detail TEXT NOT NULL)")
assert(ctx.db.execute(
  "INSERT INTO lua_process_probe (phase, detail) VALUES (?1, ?2) ON CONFLICT(phase) DO UPDATE SET detail = excluded.detail",
  {"extract", ctx.project.source_language .. ">" .. ctx.project.target_language}
) == 1)
ctx.db.commit()
"#,
    )
    .expect("Extract Lua 应可写入");
    fs::write(
        root.join(TRANSLATE_LUA),
        r#"
assert(ctx.phase == "translate")
assert(ctx.project.name == "e2e")
assert(ctx.project.output_root == nil)
assert(type(ctx.llm) == "function")

ctx.db.begin()
local translated = ctx.db.query(
  "SELECT translation FROM entry WHERE original_text = ?1",
  {"薬草です"}
)
assert(#translated == 1 and translated[1][1] == "治疗药草")

local response = ctx.llm({
  {role = "system", content = "LUA SYSTEM"},
  {role = "user", content = "LUA USER"},
})
assert(response.content == "lua-response-content")
assert(response.finish_reason == "stop")
assert(response.request_id == "request-lua")
assert(response.response_id == "response-lua")
assert(response.usage.prompt_tokens == 17)
assert(response.usage.completion_tokens == 5)
assert(response.usage.total_tokens == 22)

local detail = response.request_id .. "|" .. response.response_id .. "|" ..
  response.usage.prompt_tokens .. "/" .. response.usage.completion_tokens .. "/" ..
  response.usage.total_tokens
assert(ctx.db.execute(
  "INSERT INTO lua_process_probe (phase, detail) VALUES (?1, ?2) ON CONFLICT(phase) DO UPDATE SET detail = excluded.detail",
  {"translate", detail}
) == 1)
ctx.db.commit()
"#,
    )
    .expect("Translate Lua 应可写入");
    fs::write(
        root.join(WRITE_BACK_LUA),
        r#"
assert(ctx.phase == "write_back")
assert(ctx.project.name == "e2e")
assert(ctx.project.output_root ~= nil)
assert(ctx.llm == nil)

ctx.db.begin()
local translated = ctx.db.query(
  "SELECT translation FROM entry WHERE original_text = ?1",
  {"薬草です"}
)
assert(#translated == 1 and translated[1][1] == "治疗药草")
local candidate_items = ctx.output.read_text("data/Items.json")
assert(string.find(candidate_items, "治疗药草", 1, true) ~= nil)
ctx.output.write_text("js/lua-probe.txt", "write-back candidate")
assert(ctx.db.execute(
  "INSERT INTO lua_process_probe (phase, detail) VALUES (?1, ?2) ON CONFLICT(phase) DO UPDATE SET detail = excluded.detail",
  {"write_back", ctx.project.output_root}
) == 1)
ctx.db.commit()
"#,
    )
    .expect("WriteBack Lua 应可写入");
}

fn write_updated_write_back_lua(root: &Path) {
    fs::write(
        root.join(WRITE_BACK_LUA),
        r#"
assert(ctx.phase == "write_back")
assert(ctx.project.name == "e2e")
assert(ctx.project.output_root ~= nil)
assert(ctx.llm == nil)

local candidate_items = ctx.output.read_text("data/Items.json")
assert(string.find(candidate_items, "治疗药草", 1, true) ~= nil)
ctx.output.write_text("js/lua-probe.txt", "write-back candidate v2")
ctx.db.begin()
assert(ctx.db.execute(
  "INSERT INTO lua_process_probe (phase, detail) VALUES (?1, ?2) ON CONFLICT(phase) DO UPDATE SET detail = excluded.detail",
  {"write_back", ctx.project.output_root .. "|v2"}
) == 1)
ctx.db.commit()
"#,
    )
    .expect("更新后的 WriteBack Lua 应可写入");
}

fn write_rules(root: &Path, field_name: &str) {
    fs::write(
        root.join(RULES_JSON),
        serde_json::to_vec_pretty(&json!({
            "standard_fields": {
                "Items.json": [format!("[].{field_name}")]
            }
        }))
        .expect("Rules 夹具应可序列化"),
    )
    .expect("Rules 夹具应可写入");
}

fn write_terminology(root: &Path) {
    fs::write(
        root.join(TERMS_JSON),
        serde_json::to_vec_pretty(&json!([{
            "term": "上薬草",
            "translation": "高级药草",
            "triggers": ["上薬草"]
        }]))
        .expect("术语夹具应可序列化"),
    )
    .expect("术语夹具应可写入");
}

fn write_configuration(root: &Path, url: &str, parameters: &str) {
    let configuration = format!(
        r#"[projects]
root = "projects"

[runtime.async]
worker_threads = 2
max_blocking_threads = 4
blocking_thread_keep_alive_ms = 100

[runtime.cpu]
worker_threads = 2
queue_capacity = 16

[runtime.filesystem]
worker_threads = 2
queue_capacity = 32
max_read_bytes = 8388608
max_directory_entries = 1000

[runtime.filesystem.tree]
max_entries = 1000
max_depth = 32
max_bytes = 67108864
max_single_file_bytes = 8388608

[runtime.filesystem.project_lock]
timeout_ms = 5000

[runtime.filesystem.publisher]
max_prepared_candidates = 2
max_recovery_artifacts_per_target = 8
target_lock_timeout_ms = 5000

[runtime.sqlite]
short_worker_threads = 2
short_queue_capacity = 32
max_open_connections = 8
max_interactive_sessions = 2
interactive_open_queue_capacity = 4
interactive_command_queue_capacity = 16
worker_stack_bytes = 2097152
max_statement_bytes = 262144
max_parameter_bytes = 1048576
max_rows_per_query = 10000
max_result_bytes_per_query = 8388608
busy_timeout_ms = 5000
journal_mode = "wal"
synchronous = "full"

[runtime.llm]
max_active_requests = 2
queue_capacity = 8
admission_timeout_ms = 5000
connect_timeout_ms = 5000
read_timeout_ms = 10000
pool_idle_timeout_ms = 1000
pool_max_idle_per_host = 2
proxy = false

[runtime.llm.tls]
additional_pem_files = []

[llm.clients.primary]
url = "{url}"
api_key = "{API_KEY}"
model = "e2e-model"
timeout_ms = 10000
rpm = 60
burst = 4
parameters = '''{parameters}'''

[runtime.lua]
worker_threads = 1
queue_capacity = 4
worker_stack_bytes = 4194304
memory_limit_bytes_per_vm = 33554432
cancel_check_instruction_interval = 1000
max_error_bytes = 16384

[runtime.lua.host_values]
max_bytes = 8388608
max_nodes = 100000
max_depth = 64

[observability]
root = "logs"

[observability.translation]
queue_capacity = 16
lock_timeout_ms = 5000
max_record_bytes = 1048576
max_file_bytes = 8388608
retained_rotated_files = 2

[observability.write_back]
queue_capacity = 16
lock_timeout_ms = 5000
max_record_bytes = 1048576
max_file_bytes = 8388608
retained_rotated_files = 2

[mz.document]
read_concurrency = 2
parse_concurrency = 2

[mz.standard_asset]
decode_concurrency = 2
leaves_per_decode_job = 32

[mz.extract.builtin]
scan_concurrency = 2

[mz.extract.rules]
scan_concurrency = 2

[mz.extract.store]
encode_concurrency = 2
groups_per_encode_job = 32

[mz.translate.store]
encode_concurrency = 2
leaves_per_encode_job = 32

[[mz.languages]]
type = "japanese"
id = "ja"
minimum_kana_characters = 1
allowed_terms = []
quote_repair_pairs = [["“", "”"], ["‘", "’"]]

[[mz.translation_profiles]]
id = "local"
llm_client = "primary"
max_in_flight_tasks = 1

[mz.translation_profiles.planning]
scope_concurrency = 2
max_message_characters = 10000

[[mz.translation_profiles.planning.systems]]
source_language = "ja"
target_language = "zh-Hans"
path = "prompts/ja-zh.md"

[mz.translation_profiles.execution]
network_retry_delays_ms = [10]
max_network_retry_after_ms = 1000

[[mz.translation_profiles]]
id = "unselected"
llm_client = "primary"
max_in_flight_tasks = 1

[mz.translation_profiles.planning]
scope_concurrency = 1
max_message_characters = 10000

[[mz.translation_profiles.planning.systems]]
source_language = "ja"
target_language = "zh-Hans"
path = "prompts/does-not-exist.md"

[mz.translation_profiles.execution]
network_retry_delays_ms = []
max_network_retry_after_ms = 1000
"#
    );
    fs::write(root.join("config.toml"), configuration).expect("完整配置应可写入");
}

fn open_read_only(path: &Path) -> Connection {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("跨进程数据库应可只读打开")
}

fn assert_metadata(database: &Path) {
    let connection = open_read_only(database);
    let metadata = connection
        .query_row(
            "SELECT name, source_language, target_language, dialogue_max_fullwidth_chars, scrolling_text_max_fullwidth_chars, help_description_max_fullwidth_chars FROM metadata",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            },
        )
        .expect("Init 元数据应可跨进程读取");
    assert_eq!(
        metadata,
        (
            PROJECT.to_owned(),
            "ja".to_owned(),
            "zh-Hans".to_owned(),
            24,
            30,
            40
        )
    );
}

fn assert_extracted_database(database: &Path) {
    let connection = open_read_only(database);
    let expected_tables = BTreeSet::from([
        "entry".to_owned(),
        "map_text".to_owned(),
        "plugin_param".to_owned(),
        "system_text".to_owned(),
        "text_body".to_owned(),
    ]);
    let mut statement = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .expect("应可查询 SQLite schema");
    let actual_tables = statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("schema 查询应可执行")
        .collect::<Result<BTreeSet<_>, _>>()
        .expect("schema 行应可读取");
    assert!(expected_tables.is_subset(&actual_tables));
    let row = connection
        .query_row(
            "SELECT owner, original_text, translation FROM entry",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .expect("Builtin 应写入唯一 Items 叶子");
    assert_eq!(row, ("builtin".to_owned(), SOURCE_TEXT.to_owned(), None));
    for table in ["system_text", "map_text", "text_body", "plugin_param"] {
        let count: i64 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("空标准表应可查询");
        assert_eq!(count, 0, "{table} 应建立但保持为空");
    }
}

fn assert_translation_committed(database: &Path) {
    let connection = open_read_only(database);
    let translation: String = connection
        .query_row(
            "SELECT translation FROM entry WHERE original_text = ?1",
            [SOURCE_TEXT],
            |row| row.get(0),
        )
        .expect("真实模型结果应提交到项目数据库");
    assert_eq!(translation, TRANSLATION);
}

fn assert_translation_absent(database: &Path) {
    let connection = open_read_only(database);
    let translation: Option<String> = connection
        .query_row(
            "SELECT translation FROM entry WHERE original_text = ?1",
            [SOURCE_TEXT],
            |row| row.get(0),
        )
        .expect("取消后仍应保留原资产行");
    assert!(translation.is_none(), "取消运行不得提交空数组响应");
}

fn read_translation_leaf(database: &Path) -> (String, String, Vec<u8>) {
    open_read_only(database)
        .query_row(
            "SELECT original_text, translation, translation_state FROM entry WHERE owner = 'builtin'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("Builtin 已翻译叶及其 state 应可读取")
}

fn install_translation_write_guard(database: &Path) {
    Connection::open(database)
        .expect("应可打开项目数据库安装测试写保护")
        .execute_batch(
            "CREATE TRIGGER e2e_reject_translation_write \
             BEFORE UPDATE OF translation, translation_state ON entry \
             BEGIN SELECT RAISE(ABORT, 'converged translation must not be rewritten'); END;",
        )
        .expect("应可安装译文零写入守卫");
}

fn remove_translation_write_guard(database: &Path) {
    Connection::open(database)
        .expect("应可打开项目数据库移除测试写保护")
        .execute_batch("DROP TRIGGER e2e_reject_translation_write")
        .expect("应可移除译文零写入守卫");
}

fn assert_builtin_owner_is_stale(database: &Path) {
    assert!(
        !builtin_owner_is_fresh(database),
        "Builtin owner 应处于 stale 状态"
    );
}

fn assert_builtin_owner_is_fresh(database: &Path) {
    assert!(
        builtin_owner_is_fresh(database),
        "Builtin owner 应已刷新到当前来源"
    );
}

fn builtin_owner_is_fresh(database: &Path) -> bool {
    open_read_only(database)
        .query_row(
            "SELECT state.source_snapshot_fingerprint = metadata.source_snapshot_fingerprint \
             FROM standard_asset_owner_state AS state CROSS JOIN metadata \
             WHERE state.owner = 'builtin'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("Builtin owner freshness 应可读取")
        != 0
}

fn assert_translation_for_original(database: &Path, original: &str, expected: Option<&str>) {
    let (actual_original, actual_translation): (String, Option<String>) = open_read_only(database)
        .query_row(
            "SELECT original_text, translation FROM entry WHERE owner = 'builtin'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("Builtin 当前叶应可读取");
    assert_eq!(actual_original, original);
    assert_eq!(actual_translation.as_deref(), expected);
}

fn assert_rules_leaf(database: &Path, field_name: &str, original_text: &str) {
    let connection = open_read_only(database);
    let row: (String, String, Option<String>, Option<Vec<u8>>) = connection
        .query_row(
            "SELECT field_name, original_text, translation, translation_state \
             FROM entry WHERE owner = 'rules'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("Rules 当前唯一叶应可读取");
    assert_eq!(row.0, field_name);
    assert_eq!(row.1, original_text);
    assert_eq!(row.2, None);
    assert_eq!(row.3, None);
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM entry WHERE owner = 'rules'",
            [],
            |row| row.get(0),
        )
        .expect("Rules 叶数量应可读取");
    assert_eq!(count, 1, "Rules 修改后必须精确替换 owner 快照");
    assert!(
        connection
            .query_row(
                "SELECT state.source_snapshot_fingerprint = metadata.source_snapshot_fingerprint \
                 FROM standard_asset_owner_state AS state CROSS JOIN metadata \
                 WHERE state.owner = 'rules'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("Rules owner freshness 应可读取")
            != 0,
        "Rules owner 必须收敛到当前来源"
    );
}

fn assert_persisted_terminology(database: &Path) {
    let canonical: String = open_read_only(database)
        .query_row(
            "SELECT canonical_json FROM standard_translation_resource \
             WHERE resource_kind = 'terminology'",
            [],
            |row| row.get(0),
        )
        .expect("当前术语快照应可读取");
    let actual: Value = serde_json::from_str(&canonical).expect("持久术语必须是规范 JSON");
    assert_eq!(
        actual,
        json!([{
            "term": "上薬草",
            "translation": "高级药草",
            "triggers": ["上薬草"]
        }])
    );
}

fn assert_layout_metadata(database: &Path, dialogue: i64, scrolling_text: i64, help: i64) {
    let actual: (i64, i64, i64) = open_read_only(database)
        .query_row(
            "SELECT dialogue_max_fullwidth_chars, scrolling_text_max_fullwidth_chars, \
             help_description_max_fullwidth_chars FROM metadata",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("当前布局 metadata 应可读取");
    assert_eq!(actual, (dialogue, scrolling_text, help));
}

fn assert_write_back_lua_probe(database: &Path, expected_suffix: &str) {
    let detail: String = open_read_only(database)
        .query_row(
            "SELECT detail FROM lua_process_probe WHERE phase = 'write_back'",
            [],
            |row| row.get(0),
        )
        .expect("更新后的 WriteBack Lua 探针应可读取");
    assert!(
        detail.ends_with(expected_suffix),
        "Lua 脚本变更必须更新跨进程可观察结果：{detail}"
    );
}

fn directory_is_empty(path: &Path) -> bool {
    fs::read_dir(path).expect("目录应可列举").next().is_none()
}

fn read_output_tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, directory: &Path, output: &mut BTreeMap<PathBuf, Vec<u8>>) {
        let mut entries = fs::read_dir(directory)
            .expect("输出树应可列举")
            .map(|entry| entry.expect("输出目录项应可读取"))
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            if entry.file_type().expect("输出目录项类型应可读取").is_dir() {
                visit(root, &path, output);
            } else {
                let relative = path
                    .strip_prefix(root)
                    .expect("输出项必须属于输出根")
                    .to_path_buf();
                output.insert(relative, fs::read(path).expect("输出文件应可读取"));
            }
        }
    }

    let mut output = BTreeMap::new();
    visit(root, root, &mut output);
    output
}

fn assert_lua_probes(database: &Path, expected_phases: &[&str]) {
    let connection = open_read_only(database);
    let mut statement = connection
        .prepare("SELECT phase, detail FROM lua_process_probe ORDER BY phase")
        .expect("真实 Lua 应已建立探针表");
    let probes = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("Lua 探针应可查询")
        .collect::<Result<Vec<_>, _>>()
        .expect("Lua 探针行应可读取");
    let actual_phases = probes
        .iter()
        .map(|(phase, _)| phase.as_str())
        .collect::<BTreeSet<_>>();
    let expected_phases = expected_phases.iter().copied().collect::<BTreeSet<_>>();
    assert_eq!(actual_phases, expected_phases);
    assert_eq!(
        probes
            .iter()
            .find(|(phase, _)| phase == "extract")
            .map(|(_, detail)| detail.as_str()),
        Some("ja>zh-Hans")
    );
    if expected_phases.contains("translate") {
        assert_eq!(
            probes
                .iter()
                .find(|(phase, _)| phase == "translate")
                .map(|(_, detail)| detail.as_str()),
            Some("request-lua|response-lua|17/5/22")
        );
    }
    if expected_phases.contains("write_back") {
        let output = probes
            .iter()
            .find(|(phase, _)| phase == "write_back")
            .map(|(_, detail)| PathBuf::from(detail))
            .expect("WriteBack Lua 必须保存实际输出目录");
        assert!(!output.exists(), "成功发布后 staging 路径应已被整体移动");
    }
}

fn assert_written_game(workspace: &Path, output_root: &Path) {
    let source: Value = serde_json::from_slice(
        &fs::read(workspace.join("source/data/Items.json")).expect("冻结 Items 应可读取"),
    )
    .expect("冻结 Items 应为 JSON");
    assert_eq!(source[1]["description"], SOURCE_TEXT);

    let output: Value = serde_json::from_slice(
        &fs::read(output_root.join("data/Items.json")).expect("写回 Items 应可读取"),
    )
    .expect("写回 Items 应为 JSON");
    assert_eq!(output[1]["description"], TRANSLATION);
    assert_eq!(output[1]["fixture_marker"], true);
    assert_eq!(
        fs::read_to_string(output_root.join("js/plugins.js")).expect("写回 JS 应可读取"),
        JS_MARKER
    );
    assert_eq!(
        fs::read_to_string(output_root.join("js/lua-probe.txt"))
            .expect("Lua 对候选的修改应随唯一发布生效"),
        "write-back candidate"
    );
    assert!(output_root.join("data/Map001.json").is_file());
}

fn assert_updated_written_game(workspace: &Path, output_root: &Path) {
    let source: Value = serde_json::from_slice(
        &fs::read(workspace.join("source/data/Items.json")).expect("更新后冻结 Items 应可读取"),
    )
    .expect("更新后冻结 Items 应为 JSON");
    assert_eq!(source[1]["description"], UPDATED_SOURCE_TEXT);

    let output: Value = serde_json::from_slice(
        &fs::read(output_root.join("data/Items.json")).expect("更新后写回 Items 应可读取"),
    )
    .expect("更新后写回 Items 应为 JSON");
    assert_eq!(output[1]["description"], TRANSLATION);
    assert_eq!(output[1]["fixture_marker"], true);
    assert_eq!(
        fs::read_to_string(output_root.join("js/plugins.js")).expect("更新后写回 JS 应可读取"),
        JS_MARKER
    );
    assert!(
        !output_root.join("js/lua-probe.txt").exists(),
        "不选择 Lua 的新一轮 WriteBack 必须从冻结来源重建干净候选"
    );
}

fn assert_json_lines(log_root: &Path, output_root: &Path) {
    let (translation_raw, translation_lines) = read_json_lines(&log_root.join("translation.jsonl"));
    assert_eq!(translation_lines.len(), 3);
    let summary = translation_lines
        .iter()
        .find(|line| line["event"] == "run_completed")
        .expect("Translation JSONL 应包含汇总事件");
    let successful_run_id = summary["run_id"]
        .as_str()
        .expect("Translation 汇总 run_id 应为字符串");
    let task = translation_lines
        .iter()
        .find(|line| line["event"] == "task_processed" && line["run_id"] == successful_run_id)
        .expect("Translation JSONL 应包含成功运行的任务事件");
    let cancelled_task = translation_lines
        .iter()
        .find(|line| line["event"] == "task_processed" && line["run_id"] != successful_run_id)
        .expect("合作取消前已接管的任务也应记录明确终态");
    assert_eq!(cancelled_task["task"]["status"]["kind"], "unavailable");
    assert_eq!(
        cancelled_task["task"]["status"]["reason"]["kind"],
        "all_outputs_rejected"
    );
    assert!(
        translation_lines.iter().all(|line| {
            line["event"] != "run_completed" || line["run_id"] == successful_run_id
        }),
        "取消运行不得伪造完成汇总"
    );
    assert_eq!(task["project"], PROJECT);
    assert_eq!(task["profile"], PROFILE);
    assert_eq!(task["task"]["status"]["kind"], "complete");
    assert_eq!(task["task"]["provider_request_id"], "request-e2e");
    assert_eq!(task["task"]["provider_response_id"], "response-e2e");
    assert_eq!(task["task"]["finish_reason"], "stop");
    assert_eq!(task["task"]["final_response_usage"]["prompt_tokens"], 11);
    assert_eq!(task["task"]["final_response_usage"]["completion_tokens"], 3);
    assert_eq!(task["task"]["final_response_usage"]["total_tokens"], 14);
    assert_eq!(task["task"]["confirmed_written_locations"], 1);
    assert_eq!(summary["summary"]["written_locations"], 1);
    assert_eq!(summary["summary"]["unresolved_locations"], 0);
    let translation_run_id = task["run_id"].as_str().expect("run_id 应为字符串");
    assert_uuid_v4(translation_run_id);
    assert_eq!(summary["run_id"], translation_run_id);
    assert!(!translation_raw.contains(SOURCE_TEXT));
    assert!(!translation_raw.contains(TRANSLATION));
    assert!(!translation_raw.contains("messages"));
    assert!(!translation_raw.contains(API_KEY));
    assert!(!translation_raw.contains(E2E_EXTRA_SECRET));

    let (write_back_raw, write_back_lines) = read_json_lines(&log_root.join("write_back.jsonl"));
    assert_eq!(write_back_lines.len(), 1);
    let write_back = &write_back_lines[0];
    assert_eq!(write_back["event"], "run_completed");
    assert_eq!(write_back["project"], PROJECT);
    assert!(write_back.get("profile").is_none());
    assert_eq!(
        write_back["layout_profile"]["dialogue_body_max_fullwidth_chars"],
        24
    );
    assert_eq!(
        write_back["layout_profile"]["scrolling_text_max_fullwidth_chars"],
        30
    );
    assert_eq!(
        write_back["layout_profile"]["help_description_max_fullwidth_chars"],
        40
    );
    assert_eq!(write_back["summary"]["translated_locations"], 1);
    assert_eq!(write_back["summary"]["original_locations"], 0);
    assert_eq!(write_back["manual_layout_diagnostics"], json!([]));
    let write_back_run_id = write_back["run_id"].as_str().expect("run_id 应为字符串");
    assert_uuid_v4(write_back_run_id);
    assert_ne!(write_back_run_id, translation_run_id);
    let logged_output = PathBuf::from(
        write_back["output_root"]
            .as_str()
            .expect("output_root 应为字符串"),
    );
    assert_eq!(
        fs::canonicalize(logged_output).expect("日志输出目录应存在"),
        fs::canonicalize(output_root).expect("预期输出目录应存在")
    );
    assert!(!write_back_raw.contains(SOURCE_TEXT));
    assert!(!write_back_raw.contains(TRANSLATION));
    assert!(!write_back_raw.contains(API_KEY));
    assert!(!write_back_raw.contains(E2E_EXTRA_SECRET));
}

fn read_json_lines(path: &Path) -> (String, Vec<Value>) {
    let bytes = fs::read(path).expect("JSONL 应存在");
    assert!(!bytes.starts_with(&[0xef, 0xbb, 0xbf]));
    assert_eq!(bytes.last(), Some(&b'\n'));
    let raw = String::from_utf8(bytes).expect("JSONL 必须是 UTF-8");
    let lines = raw
        .lines()
        .map(|line| serde_json::from_str(line).expect("每条 JSONL 必须是完整 JSON"))
        .collect();
    (raw, lines)
}

fn assert_last_write_back_log(log_root: &Path, help_width: u64, lua_executed: bool) {
    let (_, records) = read_json_lines(&log_root.join("write_back.jsonl"));
    let record = records.last().expect("至少应有一条 WriteBack 日志");
    assert_eq!(record["event"], "run_completed");
    assert_eq!(
        record["layout_profile"]["help_description_max_fullwidth_chars"],
        help_width
    );
    assert_eq!(record["lua_executed"], lua_executed);
    assert_eq!(
        record["manual_layout_diagnostics"]
            .as_array()
            .expect("人工布局诊断必须是数组")
            .len(),
        1,
        "窄帮助文本布局必须形成真实人工诊断"
    );
}

fn assert_uuid_v4(value: &str) {
    let parsed = Uuid::parse_str(value).expect("run_id 必须是 UUID");
    assert_eq!(parsed.get_version_num(), 4);
    assert_eq!(parsed.to_string(), value);
}

struct BoundCancellationChatServer {
    listener: TcpListener,
    endpoint: String,
}

impl BoundCancellationChatServer {
    fn bind() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("取消探针监听器应可建立");
        let endpoint = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().expect("取消探针地址应可读取")
        );
        Self { listener, endpoint }
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn start(self) -> RunningCancellationChatServer {
        self.listener
            .set_nonblocking(true)
            .expect("取消探针监听器应可设为非阻塞");
        let (request_sender, request_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let (result_sender, result_receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = (|| {
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut stream = loop {
                    match self.listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == io::ErrorKind::WouldBlock
                                || error.raw_os_error() == Some(WSA_WOULD_BLOCK) =>
                        {
                            if Instant::now() >= deadline {
                                return Err("取消探针未收到 Chat Completions 请求".to_owned());
                            }
                            thread::yield_now();
                        }
                        Err(error) => return Err(format!("取消探针 accept 失败：{error}")),
                    }
                };
                stream
                    .set_nonblocking(false)
                    .map_err(|error| error.to_string())?;
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .map_err(|error| error.to_string())?;
                stream
                    .set_write_timeout(Some(Duration::from_secs(10)))
                    .map_err(|error| error.to_string())?;
                let request = read_http_request(&mut stream)?;
                request_sender
                    .send(())
                    .map_err(|_| "取消探针无法报告请求已到达".to_owned())?;
                release_receiver
                    .recv_timeout(Duration::from_secs(10))
                    .map_err(|_| "取消探针未收到响应许可".to_owned())?;
                let content =
                    serde_json::to_string(&json!([])).map_err(|error| error.to_string())?;
                let body = json!({
                    "id": "response-cancelled",
                    "choices": [{
                        "index": 0,
                        "message": { "role": "assistant", "content": content },
                        "finish_reason": "stop"
                    }],
                    "usage": {
                        "prompt_tokens": 1,
                        "completion_tokens": 1,
                        "total_tokens": 2
                    }
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nx-request-id: request-cancelled\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .map_err(|error| error.to_string())?;
                stream.flush().map_err(|error| error.to_string())?;
                let _ = stream.shutdown(Shutdown::Both);
                Ok(request)
            })();
            let _ = result_sender.send(result);
        });
        RunningCancellationChatServer {
            request_receiver,
            release_sender: Some(release_sender),
            result_receiver,
            worker: Some(worker),
        }
    }
}

struct RunningCancellationChatServer {
    request_receiver: mpsc::Receiver<()>,
    release_sender: Option<mpsc::Sender<()>>,
    result_receiver: mpsc::Receiver<Result<CapturedRequest, String>>,
    worker: Option<JoinHandle<()>>,
}

impl RunningCancellationChatServer {
    fn wait_until_request(&mut self) {
        if self
            .request_receiver
            .recv_timeout(Duration::from_secs(10))
            .is_err()
        {
            self.release();
            self.join();
            let detail = self.result_receiver.recv().map_or_else(
                |_| "worker 未返回诊断".to_owned(),
                |result| match result {
                    Ok(_) => "worker 未报告请求到达".to_owned(),
                    Err(error) => error,
                },
            );
            panic!("取消探针应确定性观察到 LLM 请求：{detail}");
        }
    }

    fn respond_and_finish(mut self) -> CapturedRequest {
        self.release();
        self.join();
        self.result_receiver
            .recv()
            .expect("取消探针 worker 必须返回结果")
            .unwrap_or_else(|error| panic!("取消探针失败：{error}"))
    }

    fn release(&mut self) {
        if let Some(sender) = self.release_sender.take() {
            let _ = sender.send(());
        }
    }

    fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.join().expect("取消探针 worker 不应 panic");
        }
    }
}

impl Drop for RunningCancellationChatServer {
    fn drop(&mut self) {
        self.release();
        self.join();
    }
}

struct BoundChatServer {
    listener: TcpListener,
    endpoint: String,
}

impl BoundChatServer {
    fn bind() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("本地 LLM 监听器应可建立");
        let endpoint = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().expect("本地地址应可读取")
        );
        Self { listener, endpoint }
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn start(self) -> RunningChatServer {
        self.start_with_responses(vec![
            ChatResponseFixture::Standard,
            ChatResponseFixture::Lua,
        ])
    }

    fn start_for_requests(self, expected_requests: usize) -> RunningChatServer {
        self.start_with_responses(vec![ChatResponseFixture::Standard; expected_requests])
    }

    fn start_with_responses(self, responses: Vec<ChatResponseFixture>) -> RunningChatServer {
        self.listener
            .set_nonblocking(true)
            .expect("本地监听器应可设为非阻塞");
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (result_sender, result_receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = (|| {
                let mut requests = Vec::with_capacity(responses.len());
                for (request_index, response) in responses.into_iter().enumerate() {
                    let request = loop {
                        match self.listener.accept() {
                            Ok((stream, _)) => {
                                break serve_chat_completion(stream, request_index, response)?;
                            }
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                if worker_stop.load(Ordering::Acquire) {
                                    return Err(format!(
                                        "翻译进程只发出了 {request_index} 个 Chat Completions 请求"
                                    ));
                                }
                                thread::sleep(Duration::from_millis(10));
                            }
                            Err(error) => {
                                return Err(format!("本地 LLM accept 失败：{error}"));
                            }
                        }
                    };
                    requests.push(request);
                }
                Ok(requests)
            })();
            let _ = result_sender.send(result);
        });
        RunningChatServer {
            stop,
            result_receiver,
            worker: Some(worker),
        }
    }
}

#[derive(Clone, Copy)]
enum ChatResponseFixture {
    Standard,
    Lua,
}

struct RunningChatServer {
    stop: Arc<AtomicBool>,
    result_receiver: mpsc::Receiver<Result<Vec<CapturedRequest>, String>>,
    worker: Option<JoinHandle<()>>,
}

impl RunningChatServer {
    fn finish(mut self) -> Vec<CapturedRequest> {
        self.stop.store(true, Ordering::Release);
        self.join();
        self.result_receiver
            .recv()
            .expect("本地 LLM worker 必须返回结果")
            .unwrap_or_else(|error| panic!("本地 LLM 服务器失败：{error}"))
    }

    fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.join().expect("本地 LLM worker 不应 panic");
        }
    }
}

impl Drop for RunningChatServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.join();
    }
}

struct CapturedRequest {
    request_line: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl CapturedRequest {
    fn header(&self, requested: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(requested))
            .map(|(_, value)| value.as_str())
    }
}

fn serve_chat_completion(
    mut stream: TcpStream,
    request_index: usize,
    fixture: ChatResponseFixture,
) -> Result<CapturedRequest, String> {
    stream
        .set_nonblocking(false)
        .map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| error.to_string())?;
    let request = read_http_request(&mut stream)?;
    let (request_id, response_id, content, prompt_tokens, completion_tokens, total_tokens) =
        match fixture {
            ChatResponseFixture::Standard => (
                if request_index == 0 {
                    "request-e2e".to_owned()
                } else {
                    format!("request-e2e-{request_index}")
                },
                if request_index == 0 {
                    "response-e2e".to_owned()
                } else {
                    format!("response-e2e-{request_index}")
                },
                serde_json::to_string(&json!([{
                    "id": 0,
                    "translation": TRANSLATION
                }]))
                .map_err(|error| error.to_string())?,
                11,
                3,
                14,
            ),
            ChatResponseFixture::Lua => (
                "request-lua".to_owned(),
                "response-lua".to_owned(),
                "lua-response-content".to_owned(),
                17,
                5,
                22,
            ),
        };
    let body = json!({
        "id": response_id,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": content },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": total_tokens
        }
    })
    .to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nx-request-id: {request_id}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .map_err(|error| error.to_string())?;
    stream.flush().map_err(|error| error.to_string())?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(request)
}

fn read_http_request(stream: &mut TcpStream) -> Result<CapturedRequest, String> {
    const MAXIMUM_REQUEST_BYTES: usize = 2 * 1024 * 1024;
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
        if bytes.len() > MAXIMUM_REQUEST_BYTES {
            return Err("HTTP 请求超过测试上限".to_owned());
        }
        if let Some(position) = find_subslice(&bytes, b"\r\n\r\n") {
            break position + 4;
        }
    };
    let header =
        std::str::from_utf8(&bytes[..header_end - 4]).map_err(|error| error.to_string())?;
    let mut lines = header.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| "HTTP request line 缺失".to_owned())?
        .to_owned();
    let headers = lines
        .map(|line| {
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| format!("HTTP header 无效：{line}"))?;
            Ok((name.to_owned(), value.trim().to_owned()))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let content_length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .ok_or_else(|| "HTTP Content-Length 缺失".to_owned())?
        .1
        .parse::<usize>()
        .map_err(|error| error.to_string())?;
    if header_end + content_length > MAXIMUM_REQUEST_BYTES {
        return Err("HTTP 正文超过测试上限".to_owned());
    }
    while bytes.len() < header_end + content_length {
        let read = stream
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("HTTP 请求正文提前结束".to_owned());
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    if bytes.len() != header_end + content_length {
        return Err("HTTP 请求在单个正文后仍有额外字节".to_owned());
    }
    Ok(CapturedRequest {
        request_line,
        headers,
        body: bytes[header_end..].to_vec(),
    })
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn assert_common_chat_request_headers(request: &CapturedRequest) {
    assert_eq!(request.request_line, "POST /v1/chat/completions HTTP/1.1");
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert_eq!(request.header("authorization"), Some("Bearer e2e-secret"));
    assert_eq!(
        request
            .header("content-length")
            .expect("Content-Length 应存在")
            .parse::<usize>()
            .expect("Content-Length 应是整数"),
        request.body.len()
    );
}

fn assert_exact_minimal_chat_request(request: &CapturedRequest) {
    assert_common_chat_request_headers(request);
    let actual: Value = serde_json::from_slice(&request.body).expect("LLM 请求必须是 JSON");
    let expected = json!({
        "model": "e2e-model",
        "messages": [
            { "role": "system", "content": SYSTEM_PROMPT },
            { "role": "user", "content": EXPECTED_USER_MESSAGE }
        ],
        "stream": false
    });
    assert_eq!(
        actual, expected,
        "空 parameters 时只能发送 model、messages 与 stream"
    );
}

fn assert_exact_standard_chat_request(request: &CapturedRequest) {
    assert_common_chat_request_headers(request);
    let actual: Value = serde_json::from_slice(&request.body).expect("LLM 请求必须是 JSON");
    let expected = json!({
        "model": "e2e-model",
        "messages": [
            { "role": "system", "content": SYSTEM_PROMPT },
            { "role": "user", "content": EXPECTED_USER_MESSAGE }
        ],
        "stream": false,
        "temperature": 0.0,
        "provider_extension": {
            "mode": "e2e",
            "private_marker": E2E_EXTRA_SECRET
        }
    });
    assert_eq!(actual, expected, "Chat Completions 请求 wire 必须精确匹配");
}

fn assert_exact_lua_chat_request(request: &CapturedRequest) {
    assert_common_chat_request_headers(request);
    let actual: Value = serde_json::from_slice(&request.body).expect("Lua LLM 请求必须是 JSON");
    let expected = json!({
        "model": "e2e-model",
        "messages": [
            { "role": "system", "content": "LUA SYSTEM" },
            { "role": "user", "content": "LUA USER" }
        ],
        "stream": false,
        "temperature": 0.0,
        "provider_extension": {
            "mode": "e2e",
            "private_marker": E2E_EXTRA_SECRET
        }
    });
    assert_eq!(
        actual, expected,
        "Translate Lua 必须复用同一公共 LLM 客户端"
    );
}

fn assert_standard_request_semantics(
    request: &CapturedRequest,
    system_prompt: &str,
    user_needles: &[&str],
) {
    assert_common_chat_request_headers(request);
    let actual: Value = serde_json::from_slice(&request.body).expect("LLM 请求必须是 JSON");
    assert_eq!(actual["model"], "e2e-model");
    assert_eq!(actual["stream"], false);
    assert_eq!(actual["temperature"], 0.0);
    assert_eq!(actual["provider_extension"]["mode"], "e2e");
    let messages = actual["messages"].as_array().expect("messages 必须是数组");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], system_prompt);
    assert_eq!(messages[1]["role"], "user");
    let user = messages[1]["content"]
        .as_str()
        .expect("用户消息必须是字符串");
    for needle in user_needles {
        assert!(
            user.contains(needle),
            "模型请求必须包含当前语义事实 {needle:?}：{user}"
        );
    }
}
