#![cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]

//! Windows x64 生产进程边界的 RPG Maker 纵向黑盒测试。

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString, c_void};
use std::fs;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{Value, json};
use uuid::Uuid;
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};

const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const LOAD_LIBRARY_AS_DATAFILE: u32 = 0x0000_0002;
const LOAD_LIBRARY_AS_IMAGE_RESOURCE: u32 = 0x0000_0020;
const RT_MANIFEST: u16 = 24;
const WSA_WOULD_BLOCK: i32 = 10035;

#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "LoadLibraryExW"]
    fn load_library_ex_w(file_name: *const u16, file: *mut c_void, flags: u32) -> *mut c_void;
    #[link_name = "FindResourceW"]
    fn find_resource_w(module: *mut c_void, name: *const u16, kind: *const u16) -> *mut c_void;
    #[link_name = "LoadResource"]
    fn load_resource(module: *mut c_void, resource: *mut c_void) -> *mut c_void;
    #[link_name = "SizeofResource"]
    fn size_of_resource(module: *mut c_void, resource: *mut c_void) -> u32;
    #[link_name = "LockResource"]
    fn lock_resource(resource: *mut c_void) -> *mut c_void;
    #[link_name = "FreeLibrary"]
    fn free_library(module: *mut c_void) -> i32;
}

const PROJECT: &str = "e2e";
const SHARED_PROJECT: &str = "shared";
const PROFILE: &str = "local";
const SOURCE_TEXT: &str = "薬草です";
const UPDATED_SOURCE_TEXT: &str = "上薬草です";
const TRANSLATION: &str = "治疗药草";
const MV_SPEAKER: &str = "アリス";
const MV_BODY: &str = "こんにちは、世界！";
const MV_SPEAKER_TRANSLATION: &str = "爱丽丝";
const MV_BODY_TRANSLATION: &str = "你好，世界！";
const MIXED_PROJECT: &str = "mixed-map";
const MIXED_MAP_NAME: &str = "始まりの町";
const MIXED_MAP_NAME_TRANSLATION: &str = "起始之镇";
const MIXED_SPEAKER: &str = "アリス";
const MIXED_SPEAKER_TRANSLATION: &str = "爱丽丝";
const MIXED_DIALOGUE_SOURCE: [&str; 3] = ["今日はいい天気ですね。", "一緒に町へ", "行きませんか？"];
const MIXED_DIALOGUE_TRANSLATION: [&str; 2] = ["今天天气真好。", "要一起去城里吗？"];
const MIXED_CHOICES_SOURCE: [&str; 2] = ["はい", "いいえ"];
const MIXED_CHOICES_TRANSLATION: [&str; 2] = ["是", "否"];
const MIXED_SCROLLING_SOURCE: [&str; 3] = ["スタッフ", "", "終わり"];
const MIXED_SCROLLING_TRANSLATION: [&str; 3] = ["制作人员", "", "结束"];
const MANUAL_STANDARD_PROJECT: &str = "manual-standard";
const MANUAL_STANDARD_ITEM_SOURCE: &str = "回復薬です";
const MANUAL_STANDARD_ITEM_TRANSLATION: &str = "恢复药剂";
const MANUAL_STANDARD_DIALOGUE_SOURCE: [&str; 2] = ["今日は晴れです。", "散歩しましょう。"];
const MANUAL_STANDARD_DIALOGUE_TRANSLATION: &str = "今天天气晴朗，一起散步吧。";
const MANAGED_PROJECT: &str = "managed-translation";
const MANAGED_ORIGINAL: &str = "星港へ";
const MANAGED_TRANSLATION: &str = "前往星港";
const MANAGED_LUA: &str = "scripts/lua-managed-translation.lua";
const UNICODE_LUA_PROJECT: &str = "Unicode 项目 🚀";
const UNICODE_EXECUTABLE_DIRECTORY_MARKER: &str = r"中文主程序 🚀\带 空格";
const UNICODE_ENVIRONMENT_NAME: &str = "ATT_E2E_环境变量_🚀";
const UNICODE_ENVIRONMENT_VALUE: &str = "环境值 中文 🚀";
const SYSTEM_PROMPT_TEMPLATE: &str = "E2E SYSTEM CONTRACT: {{source_language}} -> {{target_language}}; repeat {{source_language}} -> {{target_language}}";
const SYSTEM_PROMPT: &str = "E2E SYSTEM CONTRACT: ja -> zh-Hans; repeat ja -> zh-Hans";
const UPDATED_SYSTEM_PROMPT_TEMPLATE: &str =
    "E2E SYSTEM CONTRACT UPDATED: {{source_language}} -> {{target_language}}";
const UPDATED_SYSTEM_PROMPT: &str = "E2E SYSTEM CONTRACT UPDATED: ja -> zh-Hans";
const THINKING_PROMPT: &str = "E2E THINKING OUTPUT CONTRACT";
const THINKING_SENTINEL: &str = "e2e-thinking-record-sentinel";
const JS_MARKER: &str = "/* ATT MZ process e2e */";
const EXPECTED_USER_MESSAGE: &str =
    "## Database Text\n\nDescription [1] (free line breaking):\n\n> 薬草です\n";
const EXTRACT_LUA: &str = "scripts/extract.lua";
const TRANSLATE_LUA: &str = "scripts/translate.lua";
const WRITE_BACK_LUA: &str = "scripts/write_back.lua";
const RULES_TOML: &str = "rules.toml";
const TERMS_TOML: &str = "terms.toml";
const PLACEHOLDERS_TOML: &str = "placeholders.toml";
const API_KEY: &str = "e2e-secret";
const E2E_PARAMETER_MARKER: &str = "e2e-parameter-marker";
const MALFORMED_API_KEY_SENTINEL: &str = "e2e-api-key-must-not-appear";
const INVALID_PROMPT_BODY_SENTINEL: &str = "e2e-invalid-prompt-body-sentinel";
const EMPTY_PARAMETERS: &str = "{}";
const E2E_PARAMETERS: &str = r#"{"temperature":0.0,"provider_extension":{"mode":"e2e","diagnostic_marker":"e2e-parameter-marker"}}"#;
const LOG_DEGRADED_WARNING: &str = "项目日志不可用或已降级；命令会继续，退出状态不受影响。";
const TASK_RECORDS_DEGRADED_WARNING: &str =
    "翻译任务记录不可用或已降级；命令会继续，退出状态不受影响。";
const SAFE_STOPPING_PROGRESS: &str = "正在安全停止；保留最后确认进度";
const SAVE_RUN_PLAN_PROGRESS: &str = "正在保存成功运行方案";

#[test]
fn broken_stdout_changes_successful_command_terminal_log_to_failure() {
    let temporary = tempfile::tempdir().expect("应可建立端到端测试目录");
    let root = temporary.path();
    let game_root = root.join("game");
    let projects_root = root.join("projects");
    let prompt_root = root.join("prompts");
    fs::create_dir(&projects_root).expect("项目根应可建立");
    fs::create_dir_all(prompt_root.join("rpg_maker")).expect("RPG Maker 提示词目录应可建立");
    write_minimal_mz_game(&game_root);

    let server = BoundChatServer::bind();
    write_configuration(root, server.endpoint(), EMPTY_PARAMETERS);

    let mut command = att_command(root, mz_init_arguments(&game_root));
    let mut child = command.spawn().expect("att.exe 应可启动");
    drop(
        child
            .stdout
            .take()
            .expect("测试必须取得并关闭子进程 stdout 读取端"),
    );
    let output = wait_for_att(child);

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = without_fluent_isolation(&String::from_utf8_lossy(&output.stderr));
    assert!(
        stderr.contains("错误 [state.finalization_failed]"),
        "{stderr}"
    );
    assert!(stderr.contains("write_stdout"), "{stderr}");
    assert!(
        stderr.contains("影响：状态已生效，但收尾未完成"),
        "{stderr}"
    );

    let workspace = projects_root.join("mz").join(PROJECT);
    assert!(
        workspace.join("project.db").is_file(),
        "stdout 失败不得回滚或二次执行已经完成的 Init"
    );
    let (_, records) = read_project_logs(&workspace.join("logs"));
    let failure = records
        .iter()
        .find(|record| record["code"] == "failure.reported")
        .expect("stdout 失败必须写入 failure.reported");
    assert_eq!(failure["payload"]["relation"], "primary");
    assert_eq!(
        failure["payload"]["diagnostic"]["code"],
        "state.finalization_failed"
    );
    assert_eq!(failure["payload"]["diagnostic"]["stage"], "process_output");
    assert_eq!(
        failure["payload"]["diagnostic"]["impact"],
        "state_applied_finalization_failed"
    );
    assert_eq!(
        failure["payload"]["diagnostic"]["subject"]["name"],
        "write_stdout"
    );

    let run_id = &failure["run_id"];
    let terminal = records
        .iter()
        .find(|record| record["run_id"] == *run_id && record["code"] == "run.finished")
        .expect("stdout 失败必须写入同一 run 的 run.finished");
    assert_eq!(terminal["payload"]["outcome"], "failed");
    assert!(!records.iter().any(|record| {
        record["run_id"] == *run_id
            && record["code"] == "run.finished"
            && record["payload"]["outcome"] == "succeeded"
    }));
}

#[test]
fn init_extract_translate_and_write_back_cross_process_with_real_roots() {
    let temporary = tempfile::tempdir().expect("应可建立端到端测试目录");
    let root = temporary.path();
    let game_root = root.join("game");
    let projects_root = root.join("projects");
    let logs_root = projects_root.join("mz").join(PROJECT).join("logs");
    let prompt_root = root.join("prompts");
    fs::create_dir(&projects_root).expect("项目根应可建立");
    fs::create_dir(&prompt_root).expect("提示词目录应可建立");
    fs::create_dir(prompt_root.join("rpg_maker")).expect("RPG Maker 提示词目录应可建立");
    write_minimal_mz_game(&game_root);
    write_lua_scripts(root);

    let cancellation_server = BoundCancellationChatServer::bind();
    write_configuration(root, cancellation_server.endpoint(), EMPTY_PARAMETERS);

    let init_arguments = mz_init_arguments(&game_root);
    let init = run_att(root, init_arguments.clone());
    let init_stdout = assert_success("init", &init);
    assert!(
        init_stdout.starts_with("初始化完成：e2e\n项目状态：已创建\n"),
        "实际 Init stdout：{init_stdout:?}"
    );
    assert_plan_source(&init_stdout, "显式输入");

    let workspace = projects_root.join("mz").join(PROJECT);
    let database = workspace.join("project.db");
    assert!(
        !projects_root.join(PROJECT).exists(),
        "不得创建缺少引擎命名空间的工作区"
    );
    assert!(workspace.join("source/data/Items.json").is_file());
    assert!(workspace.join("source/js/plugins.js").is_file());
    assert!(workspace.join("write_back/data").is_dir());
    assert!(workspace.join("write_back/js").is_dir());
    assert_engine_lock_namespace(&projects_root, "projects");
    assert_engine_lock_namespace(&projects_root, "directory-publish");
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
    assert!(extract_stdout.starts_with("提取完成：e2e\n"));
    assert_plan_source(&extract_stdout, "显式输入");
    assert_extracted_database(&database);
    assert_lua_probes(&database, &["extract"]);

    let extract_lua_snapshot = fs::read(root.join(EXTRACT_LUA)).expect("Extract Lua 应可读取");
    fs::remove_file(root.join(EXTRACT_LUA)).expect("应删除 Extract Lua 原文件以验证数据库快照");
    let reused_extract = run_att(root, arguments(&["mz", "extract", "--name", PROJECT]));
    let reused_extract_stdout = assert_success("saved-plan extract", &reused_extract);
    assert_plan_source(&reused_extract_stdout, "项目状态");
    assert!(
        reused_extract_stdout.contains("Builtin") && reused_extract_stdout.contains("Lua"),
        "省略 owner 时应说明复用完整 Extract 方案：{reused_extract_stdout}"
    );
    assert_lua_probes(&database, &["extract"]);

    write_system_prompt(root, "zh-Hans", SYSTEM_PROMPT_TEMPLATE);
    let mut running_cancellation_server = cancellation_server.start();
    let cancelled_child = spawn_observable_att_in_new_process_group(
        root,
        arguments(&["mz", "translate", "--name", PROJECT, PROFILE]),
    );
    running_cancellation_server.wait_until_request();
    // SAFETY: 子进程使用 CREATE_NEW_PROCESS_GROUP 且继承当前控制台；只向其进程组
    // 发送 CTRL_BREAK，不会把测试进程包含在目标组内。
    let generated = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, cancelled_child.id()) };
    assert_ne!(generated, 0, "应能向 att.exe 独立进程组发送 Ctrl-Break");
    let cancelled_child = cancelled_child.wait_until_safe_stopping();
    let cancelled_request = running_cancellation_server.respond_and_finish();
    assert_exact_minimal_chat_request(&cancelled_request);
    let cancelled = cancelled_child.wait_for_output();
    assert_eq!(cancelled.status.code(), Some(130));
    assert!(cancelled.stdout.is_empty(), "Ctrl-C 不得打印业务完成文案");
    let cancelled_stderr =
        std::str::from_utf8(&cancelled.stderr).expect("取消诊断和进度必须是 UTF-8");
    assert_eq!(cancelled_stderr.matches(SAFE_STOPPING_PROGRESS).count(), 1);
    assert!(
        cancelled_stderr.ends_with("命令已在安全收尾后取消。\n"),
        "合作取消应给出本地化且非技术性的终态：{cancelled_stderr}"
    );
    assert!(!cancelled_stderr.contains('\r'));
    assert!(!cancelled_stderr.contains('\u{001b}'));
    assert_translation_absent(&database);
    assert!(
        !workspace.join("task-records").exists(),
        "默认关闭时即使 Standard 任务已经开始也不得建立空记录目录"
    );

    let server = BoundChatServer::bind();
    write_configuration(root, server.endpoint(), E2E_PARAMETERS);
    let configuration_path = root.join("config.toml");
    let configuration = fs::read_to_string(&configuration_path).expect("Translate 配置应可读取");
    fs::write(
        &configuration_path,
        configuration.replace(
            "record_translation_tasks = false",
            "record_translation_tasks = true",
        ),
    )
    .expect("应可启用 Standard 翻译任务记录");
    write_placeholders(root);
    let running_server = server.start_with_responses(vec![
        ChatResponseFixture::Standard,
        ChatResponseFixture::Lua,
        ChatResponseFixture::Lua,
        ChatResponseFixture::Lua,
    ]);
    let translate = run_att(
        root,
        arguments(&[
            "mz",
            "translate",
            "--name",
            PROJECT,
            PROFILE,
            "--placeholders",
            PLACEHOLDERS_TOML,
            "--lua",
            TRANSLATE_LUA,
        ]),
    );
    let translate_stdout = assert_success("translate", &translate);
    assert_eq!(
        translate_stdout,
        "翻译执行完成：e2e（Profile：local）\n标准翻译：任务 1，完整 1，部分 0，不可用 0；写入 1 处，剩余 0 处\n状态收敛：保留 0，失效 0，不适用 0，复用 0\nLua：已执行\n已保存本次成功运行方案。Profile 来源：显式输入；Lua 来源：显式输入。\n"
    );
    assert_translation_committed(&database);
    assert_lua_probes(&database, &["extract"]);
    assert_standard_task_record_shares_translate_run_id(&workspace, &logs_root);

    let initial_standard_write_back =
        run_att(root, arguments(&["mz", "write-back", "--name", PROJECT]));
    let initial_standard_stdout = assert_success(
        "first standard-only write-back",
        &initial_standard_write_back,
    );
    assert!(initial_standard_stdout.contains("Lua：未执行\n"));
    assert_plan_source(&initial_standard_stdout, "产品行为");

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
        "标准写回：应用译文 1 个单元，保留原文 0 个单元；自动换行 0 段，新增换行 0 处；续行全角缩进 0 处；需人工换行 0 段\n"
    ));
    assert!(write_back_stdout.contains("Lua：已执行\n"));
    assert_plan_source(&write_back_stdout, "显式输入");

    let output_root = workspace.join("write_back");
    assert_written_game(&workspace, &output_root);
    assert_lua_probes(&database, &["extract", "write_back"]);
    assert_project_log(&logs_root);

    let unchanged_init = run_att(root, arguments(&["mz", "init", "--name", PROJECT]));
    let unchanged_init_stdout = assert_success("repeated init", &unchanged_init);
    assert!(
        unchanged_init_stdout.starts_with("初始化完成：e2e\n项目状态：无变化\n"),
        "重复 Init 必须保持无变化语义，实际输出：\n{unchanged_init_stdout}"
    );
    assert_plan_source(&unchanged_init_stdout, "项目状态");
    assert!(
        output_root.join("js/lua-probe.txt").is_file(),
        "完全相同的 Init 必须保留既有写回输出"
    );

    let unit_before_reextract = read_translation_unit(&database);
    let repeated_extract = run_att(
        root,
        arguments(&["mz", "extract", "--name", PROJECT, "--builtin"]),
    );
    let repeated_extract_stdout = assert_success("repeated builtin extract", &repeated_extract);
    assert!(repeated_extract_stdout.starts_with("提取完成：e2e\n"));
    assert_plan_source(&repeated_extract_stdout, "显式输入");
    assert_extract_run_plan(&database, Some((true, false, false)));
    assert_eq!(
        read_translation_unit(&database),
        unit_before_reextract,
        "完全相同的 Builtin 快照必须精确继承译文与 translation_state"
    );

    fs::remove_file(root.join(TRANSLATE_LUA)).expect("应删除 Translate Lua 原文件以验证数据库快照");
    let mixed_source_translate = run_att(
        root,
        arguments(&["mz", "translate", "--name", PROJECT, PROFILE]),
    );
    let mixed_source_stdout = assert_success("mixed-source translate", &mixed_source_translate);
    assert!(mixed_source_stdout.contains("未提供 Lua 选项，已沿用上次成功的 Translate Lua 选择。"));
    assert_translate_plan_sources(&mixed_source_stdout, "显式输入", "项目状态");
    assert_translate_mixed_source_log(&logs_root);

    let repeated_translate = run_att(root, arguments(&["mz", "translate", "--name", PROJECT]));
    let requests = running_server.finish();
    assert_eq!(
        requests.len(),
        4,
        "首次 Standard+Lua 后，两次复用已保存 Lua 应各请求一次"
    );
    assert_exact_standard_chat_request(&requests[0]);
    assert_exact_lua_chat_request(&requests[1]);
    assert_exact_lua_chat_request(&requests[2]);
    assert_exact_lua_chat_request(&requests[3]);
    let repeated_translate_stdout = assert_success("converged translate", &repeated_translate);
    assert!(
        repeated_translate_stdout
            .contains("标准翻译：任务 0，完整 0，部分 0，不可用 0；写入 0 处，剩余 0 处")
    );
    assert!(
        repeated_translate_stdout
            .contains("全部标准翻译单元均为最新状态，Standard 本次未请求模型。")
    );
    assert!(repeated_translate_stdout.contains("Lua：已执行"));
    assert!(
        repeated_translate_stdout
            .contains("未提供 Lua 选项，已沿用上次成功的 Translate Lua 选择。")
    );
    assert_translate_plan_sources(&repeated_translate_stdout, "项目状态", "项目状态");
    assert_eq!(
        read_translation_unit(&database),
        unit_before_reextract,
        "完全收敛的 Translate 不得发出请求，也不得重写译文或 translation_state"
    );

    fs::write(root.join(TRANSLATE_LUA), b"").expect("零字节 Translate Lua 应可建立");
    let cleared_translate = run_att(
        root,
        arguments(&["mz", "translate", "--name", PROJECT, "--lua", TRANSLATE_LUA]),
    );
    let cleared_translate_stdout = assert_success("clear translate lua", &cleared_translate);
    assert!(cleared_translate_stdout.contains("已清除 Translate Lua 程序"));
    assert!(!cleared_translate_stdout.contains("Lua：已执行"));
    assert_translate_run_plan(&database, PROFILE, false);

    fs::remove_file(root.join(WRITE_BACK_LUA))
        .expect("应删除 WriteBack Lua 原文件以验证数据库快照");
    let first_output_snapshot = read_output_tree(&output_root);
    let repeated_write_back = run_att(root, arguments(&["mz", "write-back", "--name", PROJECT]));
    let repeated_write_back_stdout = assert_success("repeated write-back", &repeated_write_back);
    assert!(repeated_write_back_stdout.contains("Lua：已执行"));
    assert_plan_source(&repeated_write_back_stdout, "项目状态");
    assert_eq!(
        read_output_tree(&output_root),
        first_output_snapshot,
        "相同项目状态与相同 Lua 必须重建出逐字一致的完整输出树"
    );

    write_items_source(&game_root, UPDATED_SOURCE_TEXT);
    let updated_init = run_att(root, init_arguments.clone());
    let updated_init_stdout = assert_success("source-updated init", &updated_init);
    assert!(
        updated_init_stdout.starts_with("初始化完成：e2e\n项目状态：已更新\n需重新提取：Builtin")
    );
    assert_plan_source(&updated_init_stdout, "显式输入");
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
    let stale_stderr = without_fluent_isolation(&String::from_utf8_lossy(&stale_translate.stderr));
    assert!(
        stale_stderr.contains("错误 [project.state]"),
        "{stale_stderr}"
    );
    assert!(stale_stderr.contains("阶段：翻译"), "{stale_stderr}");
    let visible_stale_stderr = stale_stderr.replace(r"\\?\", "");
    let expected_database_path = database.to_string_lossy().replace('/', r"\");
    assert!(
        visible_stale_stderr.contains(&format!("位置：{expected_database_path}")),
        "{stale_stderr}"
    );
    assert!(
        stale_stderr.contains("原因：已保存的项目状态不满足本次操作"),
        "{stale_stderr}"
    );
    assert!(stale_stderr.contains("影响：状态未改变"), "{stale_stderr}");
    assert!(
        stale_stderr.contains("处理办法：检查并修正项目状态后重试"),
        "{stale_stderr}"
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

    fs::write(root.join(WRITE_BACK_LUA), b"").expect("零字节 WriteBack Lua 应可建立");
    let updated_write_back = run_att(
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
    let updated_write_back_stdout =
        assert_success("source-refreshed write-back", &updated_write_back);
    assert!(updated_write_back_stdout.contains("已清除 WriteBack Lua 程序"));
    assert!(updated_write_back_stdout.contains("Lua：未执行"));
    assert_write_back_run_plan(&database, false);
    assert_updated_written_game(&workspace, &output_root);

    let builtin_before_rules = read_translation_unit(&database);
    write_rules(root, "customShortName");
    let initial_rules_extract = run_att(
        root,
        arguments(&["mz", "extract", "--name", PROJECT, "--rules", RULES_TOML]),
    );
    assert_success("initial rules extract", &initial_rules_extract);
    assert_rules_unit(&database, "customShortName", "Potion");

    write_rules(root, "customLongName");
    let updated_rules_extract = run_att(
        root,
        arguments(&["mz", "extract", "--name", PROJECT, "--rules", RULES_TOML]),
    );
    assert_success("updated rules extract", &updated_rules_extract);
    assert_rules_unit(&database, "customLongName", "Restorative Potion");
    assert_eq!(
        read_translation_unit(&database),
        builtin_before_rules,
        "Rules owner 的精确替换不得扰动 Builtin 单元"
    );

    write_terminology(root);
    let before_terminology = read_translation_unit(&database);
    let terminology_translate = run_att(
        root,
        arguments(&[
            "mz",
            "translate",
            "--name",
            PROJECT,
            PROFILE,
            "--terms",
            TERMS_TOML,
        ]),
    );
    let terminology_stdout =
        assert_success("terminology-updated translate", &terminology_translate);
    assert!(terminology_stdout.contains("任务 1"));
    assert!(terminology_stdout.contains("失效 1"));
    let after_terminology = read_translation_unit(&database);
    assert_eq!(after_terminology.1, json!(TRANSLATION).to_string());
    assert_ne!(
        after_terminology.2, before_terminology.2,
        "实际触发的术语变化必须更新语义单元状态"
    );
    assert_persisted_terminology(&database);

    write_system_prompt(root, "zh-Hans", UPDATED_SYSTEM_PROMPT_TEMPLATE);
    let before_profile_semantics = read_translation_unit(&database);
    let profile_semantics_translate = run_att(
        root,
        arguments(&["mz", "translate", "--name", PROJECT, PROFILE]),
    );
    let profile_stdout =
        assert_success("profile-semantics translate", &profile_semantics_translate);
    assert!(profile_stdout.contains("任务 1"));
    assert!(profile_stdout.contains("失效 1"));
    let after_profile_semantics = read_translation_unit(&database);
    assert_eq!(after_profile_semantics.1, json!(TRANSLATION).to_string());
    assert_ne!(
        after_profile_semantics.2, before_profile_semantics.2,
        "实际 system prompt 内容变化必须更新语义单元状态"
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

    let before_layout = read_translation_unit(&database);
    let layout_init = run_att(root, mz_init_arguments_with_layout(&game_root, 24, 30, 2));
    let layout_init_stdout = assert_success("layout-updated init", &layout_init);
    assert!(layout_init_stdout.starts_with("初始化完成：e2e\n项目状态：已更新\n"));
    assert_plan_source(&layout_init_stdout, "显式输入");
    assert_layout_metadata(&database, 24, 30, 2);
    assert_eq!(
        read_translation_unit(&database),
        before_layout,
        "只改变布局必须保留标准译文及其语义状态"
    );
    assert_persisted_terminology(&database);
    assert!(directory_is_empty(&workspace.join("write_back/data")));
    assert!(directory_is_empty(&workspace.join("write_back/js")));

    let layout_write_back = run_att(root, arguments(&["mz", "write-back", "--name", PROJECT]));
    let layout_stdout = assert_success("layout-updated write-back", &layout_write_back);
    assert!(layout_stdout.contains("需人工换行 1 段"));
    assert_last_write_back_log(&logs_root, false);

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
    assert!(updated_lua_stdout.contains("Lua：已执行\n"));
    assert_plan_source(&updated_lua_stdout, "显式输入");
    assert_eq!(
        fs::read_to_string(output_root.join("js/lua-probe.txt"))
            .expect("更新后的 Lua 候选产物应可读取"),
        "write-back candidate v2"
    );
    assert_write_back_lua_probe(&database, "|v2");
    assert_last_write_back_log(&logs_root, true);

    fs::write(root.join(RULES_TOML), "rule = []\n").expect("显式空 Rules 定义应可写入");
    let cleared_rules_extract = run_att(
        root,
        arguments(&["mz", "extract", "--name", PROJECT, "--rules", RULES_TOML]),
    );
    let cleared_rules_stdout =
        assert_success("clear rules-only extract plan", &cleared_rules_extract);
    assert!(cleared_rules_stdout.contains("已停用 owner Rules"));
    assert_extract_run_plan(&database, None);
    assert_missing_extract_plan(root, "rules clear");

    fs::write(root.join(EXTRACT_LUA), &extract_lua_snapshot)
        .expect("Extract Lua 应可恢复以建立 Lua-only 方案");
    let lua_only_extract = run_att(
        root,
        arguments(&["mz", "extract", "--name", PROJECT, "--lua", EXTRACT_LUA]),
    );
    let lua_only_stdout = assert_success("lua-only extract", &lua_only_extract);
    assert_plan_source(&lua_only_stdout, "显式输入");
    assert_extract_run_plan(&database, Some((false, false, true)));

    fs::write(root.join(EXTRACT_LUA), b"").expect("零字节 Extract Lua 应可建立");
    let cleared_lua_extract = run_att(
        root,
        arguments(&["mz", "extract", "--name", PROJECT, "--lua", EXTRACT_LUA]),
    );
    let cleared_lua_stdout = assert_success("clear lua-only extract plan", &cleared_lua_extract);
    assert!(cleared_lua_stdout.contains("已停用 owner Lua"));
    assert!(cleared_lua_stdout.contains("清除后没有可执行的 Extract owner"));
    assert_extract_run_plan(&database, None);
    assert_missing_extract_plan(root, "lua clear");

    fs::remove_file(prompt_root.join("rpg_maker/zh-Hans/system.md"))
        .expect("应删除已消费的 system Prompt 夹具");

    let failed = run_att(
        root,
        arguments(&["mz", "translate", "--name", PROJECT, PROFILE]),
    );
    assert_eq!(failed.status.code(), Some(1));
    assert!(failed.stdout.is_empty(), "命令失败不得打印成功文案");
    let failed_stderr = without_fluent_isolation(&String::from_utf8_lossy(&failed.stderr));
    assert!(failed_stderr.contains("错误 [prompt.unavailable]"));
    assert!(failed_stderr.contains("阶段：命令准备"));
    assert!(failed_stderr.contains("system.md"));
    assert!(
        failed_stderr.contains("原因：所需对象不存在"),
        "{failed_stderr}"
    );
    assert!(failed_stderr.contains("影响：状态未改变"));
    assert!(failed_stderr.contains("处理办法：修正指出的配置字段后重试"));
    assert!(failed_stderr.contains("locale=zh-Hans; component=system.md"));
    assert_process_summary_omits_client_payloads("missing prompt", &failed);
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
        ("cleared Rules extract", &cleared_rules_extract),
        ("Lua-only extract", &lua_only_extract),
        ("cleared Lua extract", &cleared_lua_extract),
        ("failed translate", &failed),
    ] {
        assert_process_summary_omits_client_payloads(phase, output);
    }
}

#[test]
fn project_log_startup_failure_never_changes_success_or_cancellation_outcome() {
    let temporary = tempfile::tempdir().expect("应可建立项目日志降级端到端测试目录");
    let root = temporary.path();
    let game_root = root.join("game");
    fs::create_dir(root.join("projects")).expect("项目根应可建立");
    fs::create_dir_all(root.join("prompts/rpg_maker")).expect("提示词根应可建立");
    write_minimal_mz_game(&game_root);

    let cancellation_server = BoundCancellationChatServer::bind();
    write_configuration(root, cancellation_server.endpoint(), EMPTY_PARAMETERS);

    let init = run_att(root, mz_init_arguments(&game_root));
    let init_stdout = String::from_utf8(init.stdout.clone()).expect("Init stdout 必须是 UTF-8");
    let init_stdout: String = init_stdout
        .chars()
        .filter(|character| !matches!(character, '\u{2068}' | '\u{2069}'))
        .collect();
    assert_eq!(init.status.code(), Some(0), "Init 不得被日志故障阻断");
    assert!(
        init_stdout.starts_with("初始化完成：e2e\n项目状态：已创建\n"),
        "初始化结果应成立：{init_stdout}"
    );
    assert_plan_source(&init_stdout, "显式输入");
    assert!(init.stderr.is_empty(), "首次 Init 的项目日志应正常建立");

    let workspace = root.join("projects/mz").join(PROJECT);
    let log_root = workspace.join("logs");
    fs::remove_dir_all(&log_root).expect("应删除 Init 日志目录以注入后续日志故障");
    fs::write(&log_root, b"not-a-directory").expect("普通文件应可稳定触发项目日志启动降级");

    let extract = run_att(
        root,
        arguments(&["mz", "extract", "--name", PROJECT, "--builtin"]),
    );
    let extract_stdout =
        String::from_utf8(extract.stdout.clone()).expect("Extract stdout 必须是 UTF-8");
    let extract_stdout: String = extract_stdout
        .chars()
        .filter(|character| !matches!(character, '\u{2068}' | '\u{2069}'))
        .collect();
    assert_eq!(extract.status.code(), Some(0), "Extract 不得被日志故障阻断");
    assert!(extract_stdout.starts_with("提取完成：e2e\n"));
    assert_plan_source(&extract_stdout, "显式输入");
    assert_log_degraded_diagnostic(&extract.stderr, &log_root);

    let database = workspace.join("project.db");
    assert_extracted_database(&database);
    write_system_prompt(root, "zh-Hans", SYSTEM_PROMPT_TEMPLATE);
    let mut running_server = cancellation_server.start();
    let child = spawn_observable_att_in_new_process_group(
        root,
        arguments(&["mz", "translate", "--name", PROJECT, PROFILE]),
    );
    running_server.wait_until_request();
    // SAFETY: 子进程使用独立进程组；CTRL_BREAK 只投递到该组。
    let generated = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) };
    assert_ne!(generated, 0, "应能向日志降级场景的子进程发送 Ctrl-Break");
    let child = child.wait_until_safe_stopping();
    let request = running_server.respond_and_finish();
    assert_exact_minimal_chat_request(&request);
    let cancelled = child.wait_for_output();

    assert_eq!(cancelled.status.code(), Some(130));
    assert!(cancelled.stdout.is_empty());
    let cancelled_stderr = std::str::from_utf8(&cancelled.stderr).expect("取消诊断必须是 UTF-8");
    assert_log_degraded_diagnostic(&cancelled.stderr, &log_root);
    assert_eq!(cancelled_stderr.matches(SAFE_STOPPING_PROGRESS).count(), 1);
    assert!(
        cancelled_stderr.ends_with("命令已在安全收尾后取消。\n"),
        "日志降级不得改变合作取消终态：{cancelled_stderr}"
    );
    assert!(!cancelled_stderr.contains('\r'));
    assert!(!cancelled_stderr.contains('\u{001b}'));
    assert_translation_absent(&database);
}

#[test]
fn stage_lua_ctrl_break_cancels_without_state_or_candidate_residue() {
    let temporary = tempfile::tempdir().expect("应可建立真实取消矩阵测试目录");
    let root = temporary.path();
    let game_root = root.join("game");
    fs::create_dir(root.join("projects")).expect("项目根应可建立");
    fs::create_dir_all(root.join("prompts/rpg_maker")).expect("提示词根应可建立");
    fs::create_dir(root.join("scripts")).expect("Lua 夹具目录应可建立");
    write_minimal_mz_game(&game_root);

    let server = BoundChatServer::bind();
    write_configuration(root, server.endpoint(), EMPTY_PARAMETERS);
    assert_success(
        "真实取消矩阵 Init",
        &run_att(root, mz_init_arguments(&game_root)),
    );

    let workspace = root.join("projects/mz").join(PROJECT);
    let database = workspace.join("project.db");
    let logs_root = workspace.join("logs");
    let extract_marker = root.join("extract-cancel-ready");
    write_cancellable_extract_lua(root);
    let extract_state_before = read_saved_phase_plan_snapshot(&database);
    assert_no_directory_publish_artifacts(root);

    let extract_child = spawn_observable_att_in_new_process_group(
        root,
        arguments(&["mz", "extract", "--name", PROJECT, "--lua", EXTRACT_LUA]),
    )
    .wait_until_fixture_marker(&extract_marker);
    send_ctrl_break(&extract_child, "Extract");
    let cancelled_extract = extract_child.wait_until_safe_stopping().wait_for_output();
    assert_cooperatively_cancelled("Extract", &cancelled_extract);
    assert_eq!(
        read_saved_phase_plan_snapshot(&database),
        extract_state_before,
        "取消的 Extract 不得保存运行方案或改变其他阶段方案"
    );
    assert_database_table_absent(&database, "extract_cancel_probe");
    assert_no_directory_publish_artifacts(root);
    assert_cancelled_project_log(&logs_root, "extract");

    assert_success(
        "取消后正常 Extract",
        &run_att(
            root,
            arguments(&["mz", "extract", "--name", PROJECT, "--builtin"]),
        ),
    );
    write_system_prompt(root, "zh-Hans", SYSTEM_PROMPT_TEMPLATE);
    write_cancellable_translate_lua(root);
    let translate_state_before = read_saved_phase_plan_snapshot(&database);
    let translate_marker = root.join("translate-cancel-ready");
    let running_server = server.start_with_responses(vec![ChatResponseFixture::Standard]);
    let translate_child = spawn_observable_att_in_new_process_group(
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
    )
    .wait_until_fixture_marker(&translate_marker);
    send_ctrl_break(&translate_child, "Translate");
    let cancelled_translate = translate_child.wait_until_safe_stopping().wait_for_output();
    assert_cooperatively_cancelled("Translate", &cancelled_translate);
    assert_eq!(running_server.finish().len(), 1);
    assert_eq!(
        read_saved_phase_plan_snapshot(&database),
        translate_state_before,
        "取消的 Translate 不得保存运行方案或改变其他阶段方案"
    );
    assert_translation_committed(&database);
    assert_database_table_absent(&database, "translate_cancel_probe");
    assert_cancelled_project_log(&logs_root, "translate");

    assert_success(
        "取消后正常 Translate",
        &run_att(
            root,
            arguments(&["mz", "translate", "--name", PROJECT, PROFILE]),
        ),
    );

    let output_before = snapshot_directory_tree(&workspace.join("write_back"));
    let write_back_state_before = read_saved_phase_plan_snapshot(&database);
    let write_back_marker = root.join("write-back-cancel-ready");
    write_cancellable_write_back_lua(root);
    let write_back_child = spawn_observable_att_in_new_process_group(
        root,
        arguments(&[
            "mz",
            "write-back",
            "--name",
            PROJECT,
            "--lua",
            WRITE_BACK_LUA,
        ]),
    )
    .wait_until_fixture_marker(&write_back_marker);
    assert!(
        !directory_publish_artifacts(root).is_empty(),
        "WriteBack Lua 开始后必须存在尚未发布的真实候选，测试才能证明取消清理"
    );
    send_ctrl_break(&write_back_child, "WriteBack");
    let cancelled_write_back = write_back_child
        .wait_until_safe_stopping()
        .wait_for_output();
    assert_cooperatively_cancelled("WriteBack", &cancelled_write_back);
    assert_eq!(
        snapshot_directory_tree(&workspace.join("write_back")),
        output_before,
        "取消的 WriteBack 不得改变已发布输出目录"
    );
    assert_eq!(
        read_saved_phase_plan_snapshot(&database),
        write_back_state_before,
        "取消的 WriteBack 不得保存运行方案或改变其他阶段方案"
    );
    assert_database_table_absent(&database, "write_back_cancel_probe");
    assert_no_directory_publish_artifacts(root);
    assert_cancelled_project_log(&logs_root, "write-back");
}

#[test]
fn signal_during_run_plan_save_preserves_completed_extract_outcome() {
    let temporary = tempfile::tempdir().expect("应可建立完成后信号测试目录");
    let root = temporary.path();
    let game_root = root.join("game");
    fs::create_dir(root.join("projects")).expect("项目根应可建立");
    fs::create_dir_all(root.join("prompts/rpg_maker")).expect("提示词根应可建立");
    fs::create_dir(root.join("scripts")).expect("Lua 夹具目录应可建立");
    write_minimal_mz_game(&game_root);

    let server = BoundChatServer::bind();
    write_configuration(root, server.endpoint(), EMPTY_PARAMETERS);
    assert_success(
        "完成后信号 Init",
        &run_att(root, mz_init_arguments(&game_root)),
    );
    assert_success(
        "完成后信号基线 Extract",
        &run_att(
            root,
            arguments(&["mz", "extract", "--name", PROJECT, "--builtin"]),
        ),
    );

    let workspace = root.join("projects/mz").join(PROJECT);
    let database = workspace.join("project.db");
    let saved_plan_before = read_saved_phase_plan_snapshot(&database);
    write_completed_extract_wait_lua(root);
    let ready = root.join("completed-extract-ready");
    let release = root.join("completed-extract-release");
    let child = spawn_observable_att_in_new_process_group(
        root,
        arguments(&["mz", "extract", "--name", PROJECT, "--lua", EXTRACT_LUA]),
    )
    .wait_until_fixture_marker(&ready);

    let blocker = Connection::open(&database).expect("运行方案阻塞连接应可打开");
    blocker
        .execute_batch("BEGIN EXCLUSIVE")
        .expect("测试连接应可只阻塞最终运行方案写事务");
    fs::write(&release, b"release").expect("应可放行已经完成业务工作的 Lua 夹具");
    let child = child.wait_until_saving_run_plan();
    send_ctrl_break(&child, "运行方案最终化");
    let completed = child.wait_until_safe_stopping().wait_for_output();
    blocker
        .execute_batch("ROLLBACK")
        .expect("运行方案阻塞锁应可释放");

    let stdout = without_fluent_isolation(
        std::str::from_utf8(&completed.stdout).expect("完成结果必须是 UTF-8"),
    );
    let stderr = without_fluent_isolation(
        std::str::from_utf8(&completed.stderr).expect("完成后信号诊断必须是 UTF-8"),
    );
    assert_eq!(
        completed.status.code(),
        Some(0),
        "业务已完成后收到信号必须退出 0\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.starts_with("提取完成：e2e\n"),
        "完整业务结果必须照常呈现：{stdout}"
    );
    assert_eq!(stderr.matches(SAFE_STOPPING_PROGRESS).count(), 1);
    assert!(
        !stderr.contains("命令已在安全收尾后取消"),
        "自然完成不得呈现取消终态：{stderr}"
    );
    assert_eq!(
        read_saved_phase_plan_snapshot(&database),
        saved_plan_before,
        "最终方案写锁被信号取消时必须保留此前已保存方案"
    );
    assert_completed_signal_project_log(&workspace.join("logs"));
    assert_no_directory_publish_artifacts(root);
}

#[test]
fn omitted_translate_profile_rejects_saved_profile_removed_from_configuration() {
    let temporary = tempfile::tempdir().expect("应可建立 Profile 复用端到端测试目录");
    let root = temporary.path();
    let game_root = root.join("game");
    fs::create_dir(root.join("projects")).expect("项目根应可建立");
    fs::create_dir_all(root.join("prompts/rpg_maker")).expect("提示词根应可建立");
    write_minimal_mz_game(&game_root);
    write_system_prompt(root, "zh-Hans", SYSTEM_PROMPT_TEMPLATE);

    let initial_server = BoundChatServer::bind();
    write_configuration(root, initial_server.endpoint(), EMPTY_PARAMETERS);
    assert_success(
        "Profile 复用 Init",
        &run_att(root, mz_init_arguments(&game_root)),
    );
    assert_success(
        "Profile 复用 Extract",
        &run_att(
            root,
            arguments(&["mz", "extract", "--name", PROJECT, "--builtin"]),
        ),
    );
    let running_initial = initial_server.start_with_responses(vec![ChatResponseFixture::Standard]);
    let initial_translate = run_att(
        root,
        arguments(&["mz", "translate", "--name", PROJECT, PROFILE]),
    );
    assert_success("显式 Profile Translate", &initial_translate);
    assert_eq!(running_initial.finish().len(), 1);

    let database = root.join("projects/mz").join(PROJECT).join("project.db");
    assert_translate_run_plan(&database, PROFILE, false);

    let observing_server = BoundChatServer::bind();
    write_configuration(root, observing_server.endpoint(), EMPTY_PARAMETERS);
    remove_translation_profile_from_configuration(root, PROFILE);
    let no_requests = observing_server.start_for_requests(0);
    let omitted = run_att(root, arguments(&["mz", "translate", "--name", PROJECT]));
    assert!(
        no_requests.finish().is_empty(),
        "已保存 Profile 缺失时不得偷偷选择其他 Profile 或请求模型"
    );

    assert_eq!(omitted.status.code(), Some(1));
    assert!(omitted.stdout.is_empty());
    let stderr = without_fluent_isolation(
        &String::from_utf8(omitted.stderr).expect("Profile 错误必须是 UTF-8"),
    );
    assert!(stderr.contains("错误 [command.run_plan]"), "{stderr}");
    assert!(stderr.contains("阶段：命令准备"), "{stderr}");
    assert!(stderr.contains("位置：配置档 local"), "{stderr}");
    assert!(stderr.contains("原因：所需对象不存在"), "{stderr}");
    assert!(stderr.contains("影响：状态未改变"), "{stderr}");
    assert!(
        stderr.contains("处理办法：修正指出的输入后重试"),
        "{stderr}"
    );
    assert_translate_run_plan(&database, PROFILE, false);
}

#[test]
fn mz_map_mixes_five_semantic_unit_types_in_one_translation_task() {
    let temporary = tempfile::tempdir().expect("应可建立混合 Map 端到端测试目录");
    let root = temporary.path();
    let game_root = root.join("game");
    fs::create_dir(root.join("projects")).expect("项目根应可建立");
    fs::create_dir_all(root.join("prompts/rpg_maker")).expect("提示词根应可建立");
    write_mixed_semantic_mz_game(&game_root);

    let server = BoundChatServer::bind();
    write_configuration(root, server.endpoint(), EMPTY_PARAMETERS);

    let init = run_att(
        root,
        mz_init_arguments_for(&game_root, MIXED_PROJECT, "ja", "zh-Hans", 24, 30, 40),
    );
    assert_success("混合 Map init", &init);

    let extract = run_att(
        root,
        arguments(&["mz", "extract", "--name", MIXED_PROJECT, "--builtin"]),
    );
    assert_success("混合 Map extract", &extract);
    let workspace = root.join("projects/mz").join(MIXED_PROJECT);
    let database = workspace.join("project.db");
    assert_mixed_semantic_units_extracted(&database);

    write_system_prompt(root, "zh-Hans", SYSTEM_PROMPT_TEMPLATE);
    let running_server = server.start_with_responses(vec![ChatResponseFixture::MixedSemanticUnits]);
    let translate = run_att(
        root,
        arguments(&["mz", "translate", "--name", MIXED_PROJECT, PROFILE]),
    );
    let translate_stdout = assert_success("混合 Map translate", &translate);
    assert!(
        translate_stdout.contains("任务 1，完整 1，部分 0，不可用 0"),
        "五种语义单元应位于同一完整任务：{translate_stdout}"
    );
    assert!(
        translate_stdout.contains("写入 5 处，剩余 0 处"),
        "五种语义单元应全部原子提交：{translate_stdout}"
    );
    let requests = running_server.finish();
    assert_eq!(requests.len(), 1, "同一 Map 范围的五种类型不得按类型拆请求");
    assert_mixed_semantic_request(&requests[0]);
    assert_mixed_semantic_translations(&database);

    let write_back = run_att(
        root,
        arguments(&["mz", "write-back", "--name", MIXED_PROJECT]),
    );
    let write_back_stdout = assert_success("混合 Map write-back", &write_back);
    assert!(
        write_back_stdout.contains("标准写回：应用译文 5 个单元，保留原文 0 个单元"),
        "写回应按五个逻辑 unit 计数：{write_back_stdout}"
    );
    let output_root = workspace.join("write_back");
    assert_mixed_semantic_game_written(&output_root);
    assert_mixed_semantic_project_log(&workspace.join("logs"));
}

#[test]
fn independent_project_lua_accepts_standard_candidates_and_write_back_uses_them() {
    let temporary = tempfile::tempdir().expect("应可建立人工补译端到端测试目录");
    let root = temporary.path();
    let game_root = root.join("game");
    fs::create_dir(root.join("projects")).expect("项目根应可建立");
    fs::create_dir_all(root.join("prompts/rpg_maker")).expect("提示词根应可建立");
    write_manual_standard_mz_game(&game_root);

    let server = BoundChatServer::bind();
    write_configuration(root, server.endpoint(), EMPTY_PARAMETERS);
    enable_translation_task_records(root);
    write_system_prompt(root, "zh-Hans", SYSTEM_PROMPT_TEMPLATE);

    let init = run_att(
        root,
        mz_init_arguments_for(
            &game_root,
            MANUAL_STANDARD_PROJECT,
            "ja",
            "zh-Hans",
            24,
            30,
            40,
        ),
    );
    assert_success("人工补译 init", &init);

    let extract = run_att(
        root,
        arguments(&[
            "mz",
            "extract",
            "--name",
            MANUAL_STANDARD_PROJECT,
            "--builtin",
        ]),
    );
    assert_success("人工补译 extract", &extract);

    let workspace = root.join("projects/mz").join(MANUAL_STANDARD_PROJECT);
    let database = workspace.join("project.db");
    assert_manual_standard_units_extracted(&database);
    let saved_plans_before = read_saved_phase_plan_snapshot(&database);
    assert!(
        saved_plans_before.translate_profile.is_none(),
        "项目尚未成功运行 Translate，必须没有可复用 Profile"
    );

    let script_relative = Path::new("scripts/manual-standard.lua");
    fs::create_dir(root.join("scripts")).expect("人工补译 Lua 目录应可建立");
    let plain_script = Path::new("scripts/plain-project.lua");
    fs::write(
        root.join(plain_script),
        r#"
assert(ctx.phase == "lua")
assert(type(ctx.standard) == "table")
local ok, error = pcall(ctx.standard.open)
assert(not ok)
assert(error.domain == "standard" and error.kind == "profile_required")
"#,
    )
    .expect("普通项目 Lua 应可写入");
    let no_requests = server.start_observing_requests();
    let mut plain_arguments = arguments(&["mz", "lua", "--name", MANUAL_STANDARD_PROJECT]);
    plain_arguments.push(plain_script.as_os_str().to_owned());
    let plain_lua = run_att(root, plain_arguments);
    assert_success("无 Profile 的普通项目 Lua", &plain_lua);
    assert_eq!(
        read_saved_phase_plan_snapshot(&database),
        saved_plans_before,
        "捕获 ctx.standard.open() 的延迟 Profile 错误后不得保存运行方案"
    );

    fs::write(
        root.join(script_relative),
        r#"
assert(ctx.phase == "lua")
assert(ctx.extract == nil and ctx.translation == nil and ctx.llm == nil)
assert(ctx.output == nil and ctx.write_back == nil)
assert(type(ctx.standard) == "table")
assert(string.sub(arg[0], -19) == "manual-standard.lua")
assert(arg[1] == "人工参数" and arg[2] == "--literal")

local standard = ctx.standard.open()
local scalar = nil
local scalar_count = 0
local body = nil
local unit_iterator = standard:units()
for unit in unit_iterator do
  if unit.owner == "builtin"
     and unit.group_kind == "database_entry"
     and unit.role.kind == "scalar"
     and unit.role.field == "name"
     and unit.original == "回復薬です" then
    scalar_count = scalar_count + 1
    scalar = scalar or unit
    assert(unit.content_kind == "value")
    assert(unit.line_policy == "single")
    assert(unit.status == "missing")
    assert(unit.family_size == 2)
  elseif unit.owner == "builtin"
     and unit.group_kind == "event_dialogue"
     and unit.role.kind == "dialogue_body" then
    assert(body == nil)
    body = unit
    assert(unit.content_kind == "lines")
    assert(unit.line_policy == "reflow")
    assert(unit.expected_line_count == nil)
    assert(unit.status == "missing")
    assert(#unit.original == 2)
    assert(unit.original[1] == "今日は晴れです。")
    assert(unit.original[2] == "散歩しましょう。")
  end
end

assert(scalar_count == 2)
assert(scalar ~= nil and body ~= nil)
local results = standard:accept({
  {
    unit = scalar,
    candidate = "恢复药剂",
    replace_current = false,
  },
  {
    unit = body,
    candidate = {"今天天气晴朗，一起散步吧。"},
    replace_current = false,
  },
})
assert(#results == 2)
assert(results[1].accepted and results[1].translation == "恢复药剂")
assert(results[1].changed_locations == 2)
assert(results[2].accepted)
assert(#results[2].translation == 1)
assert(results[2].translation[1] == "今天天气晴朗，一起散步吧。")
assert(results[2].changed_locations == 1)
"#,
    )
    .expect("人工补译 Lua 应可写入");

    let mut lua_arguments = arguments(&[
        "mz",
        "lua",
        "--name",
        MANUAL_STANDARD_PROJECT,
        "--profile",
        PROFILE,
    ]);
    lua_arguments.push(script_relative.as_os_str().to_owned());
    lua_arguments.extend(arguments(&["--", "人工参数", "--literal"]));

    let project_lua = run_att(root, lua_arguments);
    let lua_stdout = assert_success("独立项目 Lua", &project_lua);
    assert!(
        lua_stdout.starts_with("项目 Lua 执行完成：manual-standard\n"),
        "独立项目 Lua 应报告自己的命令终态：{lua_stdout}"
    );
    assert_eq!(
        read_saved_phase_plan_snapshot(&database),
        saved_plans_before,
        "一次性脚本、显式 Profile 和 -- 后参数不得写入任何阶段运行方案"
    );
    assert!(
        !workspace.join("task-records").exists(),
        "人工 Standard 提交不得伪造 LLM TaskBlock"
    );
    assert_manual_standard_candidates_committed(&database);

    let translate = run_att(
        root,
        arguments(&[
            "mz",
            "translate",
            "--name",
            MANUAL_STANDARD_PROJECT,
            PROFILE,
        ]),
    );
    let translate_stdout = assert_success("人工补译后的 Translate", &translate);
    assert!(
        translate_stdout
            .contains("标准翻译：任务 0，完整 0，部分 0，不可用 0；写入 0 处，剩余 0 处"),
        "同 Profile 应把人工提交识别为 Current：{translate_stdout}"
    );
    assert!(
        translate_stdout.contains("全部标准翻译单元均为最新状态，Standard 本次未请求模型。"),
        "同 Profile 不得再次请求已人工补齐的族：{translate_stdout}"
    );
    assert!(
        no_requests.finish().is_empty(),
        "独立项目 Lua 和收敛后的 Translate 都不得发送 LLM 请求"
    );
    assert!(
        !workspace.join("task-records").exists(),
        "零 Standard 任务不得建立虚假的任务记录目录"
    );
    assert_manual_standard_candidates_committed(&database);

    let saved_plans_after_translate = read_saved_phase_plan_snapshot(&database);
    assert_eq!(
        saved_plans_after_translate.translate_profile.as_deref(),
        Some(PROFILE),
        "成功 Translate 应保存可供独立项目 Lua 复用的 Profile"
    );
    fs::write(
        root.join(plain_script),
        r#"
assert(ctx.phase == "lua")
local standard = ctx.standard.open()
local count = 0
for _ in standard:units() do
  count = count + 1
end
assert(count == 3)
"#,
    )
    .expect("Profile 复用 Lua 应可写入");
    let mut reuse_arguments = arguments(&["mz", "lua", "--name", MANUAL_STANDARD_PROJECT]);
    reuse_arguments.push(plain_script.as_os_str().to_owned());
    assert_success(
        "省略 --profile 的独立项目 Lua",
        &run_att(root, reuse_arguments),
    );
    assert_eq!(
        read_saved_phase_plan_snapshot(&database),
        saved_plans_after_translate,
        "复用已保存 Profile 的独立项目 Lua 不得重写阶段运行方案"
    );

    let write_back = run_att(
        root,
        arguments(&["mz", "write-back", "--name", MANUAL_STANDARD_PROJECT]),
    );
    let write_back_stdout = assert_success("人工补译 write-back", &write_back);
    assert!(
        write_back_stdout.contains("标准写回：应用译文 3 个单元，保留原文 0 个单元"),
        "写回应消费两个传播位置和一个 Lines 单元：{write_back_stdout}"
    );
    assert_manual_standard_game_written(&workspace.join("write_back"));
    let saved_plans_after_write_back = read_saved_phase_plan_snapshot(&database);

    let (_, records) = read_project_logs(&workspace.join("logs"));
    let lua_runs = records
        .iter()
        .filter(|record| record["command"] == "lua" && record["code"] == "run.finished")
        .collect::<Vec<_>>();
    assert_eq!(
        lua_runs.len(),
        3,
        "三个独立项目 Lua 都必须写入项目级终态日志"
    );
    for lua_run in lua_runs {
        assert_eq!(lua_run["payload"]["outcome"], "succeeded");
        let lua_run_id = &lua_run["run_id"];
        assert!(
            !records.iter().any(|record| {
                record["run_id"] == *lua_run_id
                    && record["code"]
                        .as_str()
                        .is_some_and(|code| code.starts_with("run_plan."))
            }),
            "独立项目 Lua 不得生成运行方案解析或保存事件"
        );
        assert!(
            !records.iter().any(|record| {
                record["run_id"] == *lua_run_id
                    && matches!(
                        record["code"].as_str(),
                        Some("task.started" | "task.finished")
                    )
            }),
            "独立项目 Lua 不得把执行或人工候选伪装成 LLM TaskBlock"
        );
    }

    remove_translation_profile_from_configuration(root, PROFILE);
    fs::write(
        root.join(plain_script),
        r#"
assert(ctx.phase == "lua")
local ok, error = pcall(ctx.standard.open)
assert(not ok)
assert(error.domain == "standard" and error.kind == "saved_profile_unavailable")
"#,
    )
    .expect("失效 Profile 的延迟失败 Lua 应可写入");
    let mut removed_profile_arguments =
        arguments(&["mz", "lua", "--name", MANUAL_STANDARD_PROJECT]);
    removed_profile_arguments.push(plain_script.as_os_str().to_owned());
    assert_success(
        "已保存 Profile 从配置删除后的普通项目 Lua",
        &run_att(root, removed_profile_arguments),
    );
    assert_eq!(
        read_saved_phase_plan_snapshot(&database),
        saved_plans_after_write_back,
        "捕获 Standard open 的延迟 Profile 错误后不得改变阶段运行方案"
    );
}

#[test]
fn production_executable_embeds_utf8_and_long_path_manifest_as_resource_one() {
    let temporary = tempfile::tempdir().expect("应可建立 manifest 端到端测试目录");
    let executable =
        copy_att_executable(&temporary.path().join("manifest 中文 🚀").join("带 空格"));
    let manifest = read_embedded_manifest_resource_one(&executable);

    assert!(
        manifest.contains("<activeCodePage") && manifest.contains(">UTF-8</activeCodePage>"),
        "RT_MANIFEST ID 1 必须启用 UTF-8 activeCodePage：\n{manifest}"
    );
    assert!(
        manifest.contains("<longPathAware") && manifest.contains(">true</longPathAware>"),
        "RT_MANIFEST ID 1 必须声明 longPathAware：\n{manifest}"
    );
}

#[test]
fn copied_executable_in_unicode_directory_has_utf8_package_path_and_require_order() {
    let temporary = tempfile::tempdir().expect("应可建立 Unicode require 端到端测试目录");
    let root = temporary.path().join("工作区 中文 🚀 with spaces");
    let executable_directory = root.join("中文主程序 🚀").join("带 空格");
    let executable = copy_att_executable(&executable_directory);
    initialize_unicode_lua_project(&root, &executable);

    let script_directory = root.join("Lua 脚本与模块 🧩");
    let fallback_directory = root.join("package.path 后备模块 🚀");
    let versioned_environment_directory = root.join("LUA_PATH_5_4 中文 🚀");
    let legacy_environment_directory = root.join("LUA_PATH 冲突 中文 🚀");
    fs::create_dir_all(&script_directory).expect("Unicode Lua 脚本目录应可建立");
    fs::create_dir_all(&fallback_directory).expect("Unicode package.path 目录应可建立");
    fs::create_dir_all(&versioned_environment_directory)
        .expect("Unicode LUA_PATH_5_4 目录应可建立");
    fs::create_dir_all(&legacy_environment_directory).expect("Unicode LUA_PATH 目录应可建立");

    fs::write(
        script_directory.join("相邻模块_🚀.lua"),
        "return '主程序目录模块 中文 🚀'\n",
    )
    .expect("Unicode 相邻模块应可写入");
    fs::write(
        script_directory.join("预加载优先_🚀.lua"),
        "return '不应读取的相邻文件'\n",
    )
    .expect("预加载同名相邻模块应可写入");
    fs::write(
        script_directory.join("主目录优先_🚀.lua"),
        "return '主程序目录优先'\n",
    )
    .expect("主目录优先模块应可写入");
    fs::write(
        fallback_directory.join("主目录优先_🚀.lua"),
        "return 'package.path 不应优先'\n",
    )
    .expect("package.path 同名模块应可写入");
    fs::write(
        versioned_environment_directory.join("版本化环境模块_🚀.lua"),
        "return 'LUA_PATH_5_4 优先'\n",
    )
    .expect("LUA_PATH_5_4 模块应可写入");
    fs::write(
        legacy_environment_directory.join("版本化环境模块_🚀.lua"),
        "return 'LUA_PATH 不应生效'\n",
    )
    .expect("冲突 LUA_PATH 模块应可写入");

    let main_script = script_directory.join("主程序 require 🚀.lua");
    fs::write(
        &main_script,
        r#"
assert(ctx.phase == "lua")
assert(utf8.len(package.path) ~= nil, "package.path 必须是 UTF-8")
assert(string.find(package.path, arg[2], 1, true) ~= nil,
       "默认 package.path 必须包含实际 att.exe 目录")
assert(string.find(package.path, arg[5], 1, true) ~= nil,
       "LUA_PATH_5_4 必须进入 package.path")
assert(string.find(package.path, arg[6], 1, true) == nil,
       "LUA_PATH_5_4 存在时不得读取 LUA_PATH")
assert(require("版本化环境模块_🚀") == "LUA_PATH_5_4 优先")

assert(require("相邻模块_🚀") == "主程序目录模块 中文 🚀")

package.preload["预加载优先_🚀"] = function()
  return "package.preload 优先"
end
assert(require("预加载优先_🚀") == "package.preload 优先")

package.path = arg[1] .. ";" .. package.path
assert(require("主目录优先_🚀") == "主程序目录优先")

local found = assert(package.searchpath("主目录优先_🚀", arg[1]))
assert(found == arg[3])
local missing, search_diagnostic = package.searchpath("缺失模块_🚀", arg[1])
assert(missing == nil)
assert(string.find(search_diagnostic, arg[4], 1, true) ~= nil)
assert(string.find(search_diagnostic, "os error 2", 1, true) ~= nil)
"#,
    )
    .expect("Unicode require 主程序应可写入");

    let package_template = fallback_directory.join("?.lua");
    let mut lua_arguments = arguments(&["mz", "lua", "--name", UNICODE_LUA_PROJECT]);
    lua_arguments.push(main_script.into_os_string());
    lua_arguments.push("--".into());
    lua_arguments.push(package_template.into_os_string());
    lua_arguments.push(UNICODE_EXECUTABLE_DIRECTORY_MARKER.into());
    lua_arguments.push(
        fallback_directory
            .join("主目录优先_🚀.lua")
            .into_os_string(),
    );
    lua_arguments.push(fallback_directory.join("缺失模块_🚀.lua").into_os_string());
    lua_arguments.push(versioned_environment_directory.clone().into_os_string());
    lua_arguments.push(legacy_environment_directory.clone().into_os_string());

    let versioned_package_path = format!(
        "{};;",
        versioned_environment_directory.join("?.lua").display()
    );
    let legacy_package_path = legacy_environment_directory.join("?.lua");
    let mut command = att_command_for_executable(&executable, &root, lua_arguments);
    command.env("LUA_PATH_5_4", versioned_package_path);
    command.env("LUA_PATH", legacy_package_path);
    let child = command
        .spawn()
        .expect("带 Unicode LUA_PATH 的复制 att.exe 应可启动");
    let lua = wait_for_att(child);
    assert_success("Unicode 可执行路径 require", &lua);
}

#[test]
fn copied_executable_exposes_unicode_safe_lua_standard_library_boundaries() {
    let temporary = tempfile::tempdir().expect("应可建立 Unicode 标准库端到端测试目录");
    let root = temporary.path().join("标准库工作区 中文 🚀 with spaces");
    let executable = copy_att_executable(&root.join("程序目录 中文 🚀").join("带 空格"));
    initialize_unicode_lua_project(&root, &executable);

    let data_directory = root.join("Lua 直接 I／O 中文 🚀");
    fs::create_dir_all(&data_directory).expect("Unicode Lua 直接 I/O 目录应可建立");
    let read_path = data_directory.join("读取 输入 🚀.txt");
    let write_path = data_directory.join("写入 输出 🚀.txt");
    let default_output_path = data_directory.join("默认输出 中文 🚀.txt");
    let loadfile_path = data_directory.join("loadfile 模块 中文 🚀.lua");
    let dofile_path = data_directory.join("dofile 模块 中文 🚀.lua");
    let rename_source = data_directory.join("改名前 中文 🚀.txt");
    let rename_target = data_directory.join("改名后 中文 🚀.txt");
    let missing_path = data_directory.join("不存在 文件 中文 🚀.txt");
    let execute_output_path = data_directory.join("os.execute 输出 中文 🚀.txt");
    let popen_source_path = data_directory.join("io.popen 输入 中文 🚀.txt");
    let failed_rename_target = data_directory.join("不会建立的 rename 目标 中文 🚀.txt");
    fs::write(&read_path, "第一行 中文 🚀\n第二行 空格 path\n")
        .expect("Unicode Lua 输入文件应可写入");
    fs::write(&loadfile_path, "return 'loadfile 已读取 中文 🚀'\n")
        .expect("Unicode loadfile 文件应可写入");
    fs::write(&dofile_path, "return 'dofile 已读取 中文 🚀'\n")
        .expect("Unicode dofile 文件应可写入");
    fs::write(&rename_source, "等待改名").expect("Unicode 待改名文件应可写入");
    fs::write(&popen_source_path, "popen-unicode-path-ok")
        .expect("Unicode io.popen 输入文件应可写入");

    let script_directory = root.join("标准库脚本 中文 🚀");
    fs::create_dir_all(&script_directory).expect("Unicode 标准库脚本目录应可建立");
    let main_script = script_directory.join("标准库边界 中文 🚀.lua");
    fs::write(
        &main_script,
        r#"
assert(ctx.phase == "lua")

local reader = assert(io.open(arg[1], "rb"))
assert(reader:read("*a") == "第一行 中文 🚀\n第二行 空格 path\n")
assert(reader:close())

local writer = assert(io.open(arg[2], "wb"))
assert(writer:write("io.open 写入 中文 🚀"))
assert(writer:close())

local lines = {}
for line in io.lines(arg[1]) do
  lines[#lines + 1] = line
end
assert(#lines == 2)
assert(lines[1] == "第一行 中文 🚀")
assert(lines[2] == "第二行 空格 path")

io.input(arg[1])
assert(io.read("*l") == "第一行 中文 🚀")
assert(io.close(io.input()))

io.output(arg[3])
assert(io.write("io.output 写入 中文 🚀"))
assert(io.close(io.output()))

local loaded = assert(loadfile(arg[4]))
assert(loaded() == "loadfile 已读取 中文 🚀")
assert(dofile(arg[5]) == "dofile 已读取 中文 🚀")

assert(os.rename(arg[6], arg[7]))
assert(os.remove(arg[7]))

local environment = os.getenv(arg[8])
assert(environment == arg[9])
assert(environment == "环境值 中文 🚀")
assert(utf8.len(environment) ~= nil,
       "Unicode 环境变量必须以 UTF-8 Lua string 返回")

local missing_reader, open_message, open_code = io.open(arg[10], "rb")
assert(missing_reader == nil)
assert(string.find(open_message, arg[10], 1, true) ~= nil)
assert(string.find(open_message, "Windows error 2", 1, true) ~= nil)
assert(open_code == 2)

local missing_chunk, loadfile_message = loadfile(arg[10])
assert(missing_chunk == nil)
assert(string.find(loadfile_message, arg[10], 1, true) ~= nil)
assert(string.find(loadfile_message, "Windows error 2", 1, true) ~= nil)
local dofile_ok, dofile_message = pcall(dofile, arg[10])
assert(not dofile_ok)
assert(string.find(dofile_message, arg[10], 1, true) ~= nil)
assert(string.find(dofile_message, "Windows error 2", 1, true) ~= nil)

local removed, remove_message, remove_code = os.remove(arg[10])
assert(removed == nil)
assert(string.find(remove_message, arg[10], 1, true) ~= nil)
assert(string.find(remove_message, "Windows error 2", 1, true) ~= nil)
assert(remove_code == 2)

local denied, denied_message, denied_code = io.open(arg[13], "rb")
assert(denied == nil)
assert(string.find(denied_message, arg[13], 1, true) ~= nil)
assert(string.find(denied_message, "Windows error 5", 1, true) ~= nil)
assert(denied_code == 13)

local renamed, rename_message, rename_code = os.rename(arg[10], arg[14])
assert(renamed == nil)
assert(string.find(rename_message, arg[10], 1, true) ~= nil)
assert(string.find(rename_message, arg[14], 1, true) ~= nil)
assert(string.find(rename_message, "Windows error 2", 1, true) ~= nil)
assert(rename_code == 2)

local execute_ok, execute_kind, execute_code =
  os.execute('type nul > "' .. arg[11] .. '"')
assert(execute_ok == true and execute_kind == "exit" and execute_code == 0)

local process = assert(io.popen('type "' .. arg[12] .. '"', "r"))
assert(process:read("*a") == "popen-unicode-path-ok")
local popen_ok, popen_kind, popen_code = process:close()
assert(popen_ok == true and popen_kind == "exit" and popen_code == 0)
"#,
    )
    .expect("Unicode 标准库主程序应可写入");

    let mut lua_arguments = arguments(&["mz", "lua", "--name", UNICODE_LUA_PROJECT]);
    lua_arguments.push(main_script.into_os_string());
    lua_arguments.push("--".into());
    lua_arguments.push(
        read_path
            .strip_prefix(&root)
            .expect("Unicode 输入路径应可按进程 cwd 表达")
            .as_os_str()
            .to_owned(),
    );
    for argument in [
        &write_path,
        &default_output_path,
        &loadfile_path,
        &dofile_path,
        &rename_source,
        &rename_target,
    ] {
        lua_arguments.push(argument.as_os_str().to_owned());
    }
    lua_arguments.push(UNICODE_ENVIRONMENT_NAME.into());
    lua_arguments.push(UNICODE_ENVIRONMENT_VALUE.into());
    lua_arguments.push(missing_path.into_os_string());
    lua_arguments.push(execute_output_path.clone().into_os_string());
    lua_arguments.push(popen_source_path.into_os_string());
    lua_arguments.push(data_directory.into_os_string());
    lua_arguments.push(failed_rename_target.into_os_string());

    let mut command = att_command_for_executable(&executable, &root, lua_arguments);
    command.env(UNICODE_ENVIRONMENT_NAME, UNICODE_ENVIRONMENT_VALUE);
    let child = command
        .spawn()
        .expect("Unicode 标准库测试中的 att.exe 应可启动");
    let lua = wait_for_att(child);
    assert_success("Unicode Lua 标准库边界", &lua);

    assert_eq!(
        fs::read_to_string(&write_path).expect("io.open Unicode 输出应可读取"),
        "io.open 写入 中文 🚀"
    );
    assert_eq!(
        fs::read_to_string(&default_output_path).expect("io.output Unicode 输出应可读取"),
        "io.output 写入 中文 🚀"
    );
    assert!(
        !rename_source.exists(),
        "os.rename 后旧 Unicode 路径必须消失"
    );
    assert!(
        !rename_target.exists(),
        "os.remove 后新 Unicode 路径必须消失"
    );
    assert!(
        execute_output_path.is_file(),
        "os.execute 必须能把 Unicode 输出路径交给命令解释器"
    );
}

#[test]
fn copied_executable_supports_long_unicode_paths() {
    let temporary = tempfile::tempdir().expect("应可建立长路径端到端测试目录");
    let mut root = temporary.path().join("长路径工作区 中文 🚀");
    while root
        .join("中文主程序 🚀")
        .join("带 空格")
        .join("att.exe")
        .to_string_lossy()
        .encode_utf16()
        .count()
        < 320
    {
        root = root.join("重复长路径段 中文 🚀 0123456789");
    }
    assert_unicode_require_contract(&root, "长路径");
}

#[test]
fn copied_executable_supports_unc_unicode_paths_when_root_is_explicitly_provided() {
    let release_acceptance =
        std::env::var_os("ATT_RELEASE_ACCEPTANCE").as_deref() == Some(OsStr::new("1"));
    let Some(unc_root) = std::env::var_os("ATT_TEST_UNC_ROOT") else {
        assert!(
            !release_acceptance,
            "ATT_RELEASE_ACCEPTANCE=1 时必须提供可写 UNC 根 ATT_TEST_UNC_ROOT"
        );
        eprintln!("跳过 UNC 执行：把 ATT_TEST_UNC_ROOT 设为可写 UNC 目录后运行此测试");
        return;
    };
    let unc_root = PathBuf::from(unc_root);
    assert!(
        unc_root.to_string_lossy().starts_with(r"\\"),
        "ATT_TEST_UNC_ROOT 必须是 UNC 路径，实际为 {}",
        unc_root.display()
    );
    let temporary = tempfile::Builder::new()
        .prefix("att-unicode-unc-")
        .tempdir_in(&unc_root)
        .expect("应可在显式 UNC 根内建立唯一测试目录");
    let root = temporary.path().join("UNC 工作区 中文 🚀 with spaces");
    assert_unicode_require_contract(&root, "UNC");
}

#[test]
fn oversized_system_group_exceeds_task_target_and_still_reaches_the_model() {
    const OVERSIZED_PROJECT: &str = "oversized-system";
    const TASK_TARGET: usize = 2_048;

    let temporary = tempfile::tempdir().expect("应可建立超目标 System 端到端测试目录");
    let root = temporary.path();
    let game_root = root.join("game");
    fs::create_dir(root.join("projects")).expect("项目根应可建立");
    fs::create_dir_all(root.join("prompts/rpg_maker")).expect("提示词根应可建立");
    write_minimal_mz_game(&game_root);
    write_items_source(&game_root, "");
    let original = "あ".repeat(4_330);
    write_oversized_system_source(&game_root, &original);

    let server = BoundChatServer::bind();
    write_configuration_with_task_target(root, server.endpoint(), E2E_PARAMETERS, TASK_TARGET);
    let init = run_att(
        root,
        mz_init_arguments_for(&game_root, OVERSIZED_PROJECT, "ja", "zh-Hans", 24, 30, 40),
    );
    assert_success("超目标 System init", &init);
    let extract = run_att(
        root,
        arguments(&["mz", "extract", "--name", OVERSIZED_PROJECT, "--builtin"]),
    );
    assert_success("超目标 System extract", &extract);

    write_system_prompt(root, "zh-Hans", SYSTEM_PROMPT_TEMPLATE);
    let running_server = server.start_with_responses(vec![ChatResponseFixture::Standard]);
    let translate = run_att(
        root,
        arguments(&["mz", "translate", "--name", OVERSIZED_PROJECT, PROFILE]),
    );
    let stdout = assert_success("超目标 System translate", &translate);
    assert!(
        stdout.contains("任务 1，完整 1，部分 0，不可用 0"),
        "超目标 System 组应作为独立任务完成：{stdout}"
    );
    assert!(
        stdout.contains("写入 1 处，剩余 0 处"),
        "超目标 System 译文应正常提交：{stdout}"
    );

    let requests = running_server.finish();
    assert_eq!(requests.len(), 1);
    assert_standard_request_semantics(&requests[0], SYSTEM_PROMPT, &[&original]);
    let request: Value =
        serde_json::from_slice(&requests[0].body).expect("超目标模型请求必须是 JSON");
    let user_message = request["messages"][1]["content"]
        .as_str()
        .expect("超目标 user message 必须是字符串");
    assert!(user_message.chars().count() > TASK_TARGET);

    let database = root
        .join("projects/mz")
        .join(OVERSIZED_PROJECT)
        .join("project.db");
    assert_translation_for_original(&database, &original, Some(TRANSLATION));
}

#[test]
fn mv_four_stages_preserve_www_layout_and_coexist_with_same_named_mz_project() {
    let temporary = tempfile::tempdir().expect("应可建立 MV 端到端测试目录");
    let root = temporary.path();
    let mz_game_root = root.join("mz-game");
    let mv_game_root = root.join("mv-game");
    let projects_root = root.join("projects");
    let prompt_root = root.join("prompts/rpg_maker");
    fs::create_dir(&projects_root).expect("项目根应可建立");
    fs::create_dir_all(&prompt_root).expect("RPG Maker 提示词根应可建立");
    write_minimal_mz_game(&mz_game_root);
    write_minimal_mv_game(&mv_game_root);
    write_mv_dialogue_rules(root);

    let unused_server = BoundChatServer::bind();
    write_configuration(root, unused_server.endpoint(), EMPTY_PARAMETERS);
    drop(unused_server);

    let mz_init = run_att(
        root,
        rpg_maker_init_arguments("mz", SHARED_PROJECT, &mz_game_root),
    );
    assert_success("同名 MZ init", &mz_init);
    let mv_init = run_att(
        root,
        rpg_maker_init_arguments("mv", SHARED_PROJECT, &mv_game_root),
    );
    assert_success("MV init", &mv_init);

    let mz_workspace = projects_root.join("mz").join(SHARED_PROJECT);
    let mv_workspace = projects_root.join("mv").join(SHARED_PROJECT);
    assert!(mz_workspace.join("source/data/Items.json").is_file());
    assert!(mv_workspace.join("source/www/data/Map001.json").is_file());
    assert!(mv_workspace.join("source/www/js/rpg_core.js").is_file());
    assert!(mv_workspace.join("write_back/www/data").is_dir());
    assert!(mv_workspace.join("write_back/www/js").is_dir());
    assert_ne!(
        fs::canonicalize(&mz_workspace).expect("MZ 工作区应存在"),
        fs::canonicalize(&mv_workspace).expect("MV 工作区应存在"),
        "同名 MZ/MV 项目必须使用独立命名空间"
    );

    let extract = run_att(
        root,
        arguments(&[
            "mv",
            "extract",
            "--name",
            SHARED_PROJECT,
            "--builtin",
            "--dialogue-rules",
            "dialogue.toml",
        ]),
    );
    assert_success("MV extract", &extract);
    assert_mv_dialogue_extracted(&mv_workspace.join("project.db"));

    write_system_prompt(root, "zh-Hans", SYSTEM_PROMPT_TEMPLATE);
    let server = BoundChatServer::bind();
    write_configuration(root, server.endpoint(), EMPTY_PARAMETERS);
    let running_server = server.start_with_responses(vec![ChatResponseFixture::MvDialogue]);
    let translate = run_att(
        root,
        arguments(&["mv", "translate", "--name", SHARED_PROJECT, PROFILE]),
    );
    assert_success("MV translate", &translate);
    let requests = running_server.finish();
    assert_eq!(requests.len(), 1, "同一 MV 对话组不得拆分为多个请求");
    assert_mv_dialogue_request(&requests[0]);
    assert_mv_dialogue_translated(&mv_workspace.join("project.db"));

    let write_back = run_att(
        root,
        arguments(&["mv", "write-back", "--name", SHARED_PROJECT]),
    );
    assert_success("MV write-back", &write_back);
    let output_root = mv_workspace.join("write_back");
    assert_mv_dialogue_written(&output_root);
    assert!(
        !output_root.join("data").exists() && !output_root.join("js").exists(),
        "MV 写回不得把 www 内容提升到输出根"
    );

    let (_, mut records) = read_project_logs(&mz_workspace.join("logs"));
    let (_, mv_records) = read_project_logs(&mv_workspace.join("logs"));
    records.extend(mv_records);
    assert!(records.iter().any(|record| {
        record["engine"] == "mz"
            && record["project"] == SHARED_PROJECT
            && record["command"] == "init"
    }));
    assert!(records.iter().any(|record| {
        record["engine"] == "mv"
            && record["project"] == SHARED_PROJECT
            && record["command"] == "write-back"
            && record["code"] == "publication.finished"
            && record["payload"]["kind"] == "publication"
            && record["payload"]["outcome"] == "published"
    }));
}

fn assert_engine_lock_namespace(projects_root: &Path, lock_kind: &str) {
    let parent = projects_root.join(".att-locks").join(lock_kind);
    let namespaces = fs::read_dir(&parent)
        .unwrap_or_else(|error| panic!("应可列举锁命名空间 {}：{error}", parent.display()))
        .collect::<Result<Vec<_>, _>>()
        .expect("锁命名空间条目应可读取");
    assert_eq!(namespaces.len(), 1, "锁根下只能存在当前 MZ 命名空间");
    assert_eq!(namespaces[0].file_name(), OsStr::new("mz"));
    assert!(namespaces[0].path().is_dir(), "MZ 锁命名空间应为目录");

    let lock_files = fs::read_dir(namespaces[0].path())
        .expect("应可列举 MZ 锁目录")
        .collect::<Result<Vec<_>, _>>()
        .expect("MZ 锁条目应可读取");
    assert!(!lock_files.is_empty(), "MZ 锁目录应包含生产锁文件");
    assert!(
        lock_files
            .iter()
            .all(|entry| entry.path().extension() == Some(OsStr::new("lock"))),
        "锁文件不得创建在 MZ 命名空间之外"
    );
}

#[test]
fn malformed_configuration_does_not_echo_api_key() {
    let temporary = tempfile::tempdir().expect("应可建立密钥泄漏测试目录");
    let root = temporary.path();
    fs::write(
        root.join("config.toml"),
        format!(
            r#"[llm.clients.invalid-api-key]
url = "https://example.invalid/v1/chat/completions"
api_key = "{MALFORMED_API_KEY_SENTINEL}" "invalid"
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
    let stderr =
        without_fluent_isolation(&String::from_utf8(output.stderr).expect("stderr 必须是 UTF-8"));
    assert!(!stderr.is_empty(), "配置失败必须呈现诊断");
    assert!(
        stderr.contains(&root.join("config.toml").display().to_string()),
        "配置语法错误必须包含配置路径：{stderr}"
    );
    assert!(
        stderr.contains("错误 [configuration.invalid_toml]"),
        "{stderr}"
    );
    assert!(stderr.contains("阶段：配置加载"), "{stderr}");
    assert!(
        stderr.contains("原因：TOML 第 3 行、第 ") && stderr.contains("列无效"),
        "配置语法错误必须包含 1-based 行列：{stderr}"
    );
    assert!(
        stderr.contains("llm.clients.invalid-api-key.api_key"),
        "配置语法错误必须标明安全字段路径：{stderr}"
    );
    assert!(stderr.contains("影响：状态未改变"), "{stderr}");
    assert!(
        stderr.contains("处理办法：修正指出的配置字段后重试"),
        "{stderr}"
    );
    assert!(
        !stderr.contains(MALFORMED_API_KEY_SENTINEL),
        "配置语法错误不得回显 API key：{stderr}"
    );
}

#[test]
fn configuration_path_is_required_for_commands_but_not_information_actions() {
    let temporary = tempfile::tempdir().expect("应可建立 CLI 配置边界测试目录");

    let missing = Command::new(env!("CARGO_BIN_EXE_att"))
        .current_dir(temporary.path())
        .args(["mz", "extract", "--name", PROJECT, "--builtin"])
        .output()
        .expect("att.exe 应可执行");
    assert_eq!(missing.status.code(), Some(2));
    assert!(missing.stdout.is_empty());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("--config"));

    for argument in ["--help", "--version"] {
        let output = Command::new(env!("CARGO_BIN_EXE_att"))
            .current_dir(temporary.path())
            .arg(argument)
            .output()
            .expect("ATT 信息命令应可执行");
        assert_eq!(output.status.code(), Some(0), "{argument} 不应要求配置文件");
    }
}

#[test]
fn prompt_locale_routing_renders_language_pairs_and_fails_before_llm_without_fallback() {
    const JA_PROJECT: &str = "prompt-ja";
    const EN_PROJECT: &str = "prompt-en";
    const CANONICAL_EN_US_PROJECT: &str = "canonical-en-us";
    const SHARED_TEMPLATE: &str = "ZH-HANS MASTER {{source_language}} -> {{target_language}} / {{source_language}} -> {{target_language}}";
    const FR_TEMPLATE: &str =
        "FR LOCALE {{source_language}} vers {{target_language}} / {{target_language}}";
    const EN_SOURCE: &str = "Healing potion";

    let temporary = tempfile::tempdir().expect("应可建立 Prompt 路由测试目录");
    let root = temporary.path();
    let prompt_rpg_maker = root.join("prompts/rpg_maker");
    fs::create_dir_all(root.join("projects")).expect("项目根应可建立");
    fs::create_dir_all(&prompt_rpg_maker).expect("RPG Maker Prompt 根应可建立");

    let ja_game = root.join("game-ja");
    write_minimal_mz_game(&ja_game);
    initialize_and_extract_prompt_project(root, JA_PROJECT, &ja_game, "JA", "zh-hans");
    write_system_prompt(root, "zh-Hans", SHARED_TEMPLATE);

    let ja_server = BoundChatServer::bind();
    write_configuration(root, ja_server.endpoint(), E2E_PARAMETERS);
    let ja_requests = ja_server.start_for_requests(1);
    let ja_translate = run_att(
        root,
        arguments(&["mz", "translate", "--name", JA_PROJECT, PROFILE]),
    );
    assert_success("exact ja prompt", &ja_translate);
    let ja_requests = ja_requests.finish();
    assert_eq!(ja_requests.len(), 1);
    assert_standard_request_semantics(
        &ja_requests[0],
        &render_system_prompt(SHARED_TEMPLATE, "ja", "zh-Hans"),
        &[SOURCE_TEXT],
    );

    let en_game = root.join("game-en");
    write_minimal_mz_game(&en_game);
    write_items_source(&en_game, EN_SOURCE);
    initialize_and_extract_prompt_project(root, EN_PROJECT, &en_game, "EN", "zh-Hans");

    let en_server = BoundChatServer::bind();
    write_configuration(root, en_server.endpoint(), E2E_PARAMETERS);
    let en_requests = en_server.start_for_requests(1);
    let en_translate = run_att(
        root,
        arguments(&["mz", "translate", "--name", EN_PROJECT, PROFILE]),
    );
    assert_success("exact en prompt", &en_translate);
    let en_requests = en_requests.finish();
    assert_eq!(en_requests.len(), 1);
    assert_standard_request_semantics(
        &en_requests[0],
        &render_system_prompt(SHARED_TEMPLATE, "en", "zh-Hans"),
        &[EN_SOURCE],
    );

    write_system_prompt(
        root,
        "en",
        "UNSELECTED EN {{source_language}} {{target_language}}",
    );
    assert_prompt_failure_before_llm(
        root,
        JA_PROJECT,
        "fr",
        false,
        "system.md",
        &["所需对象不存在"],
    );

    write_system_prompt(root, "fr", FR_TEMPLATE);
    let fr_server = BoundChatServer::bind();
    write_configuration_with_prompt_options(
        root,
        fr_server.endpoint(),
        E2E_PARAMETERS,
        "fr",
        false,
    );
    let fr_requests = fr_server.start_for_requests(1);
    let fr_translate = run_att(
        root,
        arguments(&["mz", "translate", "--name", JA_PROJECT, PROFILE]),
    );
    assert_success("explicit fr prompt locale", &fr_translate);
    let fr_requests = fr_requests.finish();
    assert_eq!(fr_requests.len(), 1);
    assert_standard_request_semantics(
        &fr_requests[0],
        &render_system_prompt(FR_TEMPLATE, "ja", "zh-Hans"),
        &[SOURCE_TEXT],
    );

    const FR_AUTO_TEMPLATE: &str =
        "FR AUTO {{source_language}} vers {{target_language}} / {{source_language}}";
    write_system_prompt(root, "fr", FR_AUTO_TEMPLATE);
    let auto_server = BoundChatServer::bind();
    write_configuration_with_prompt_options(
        root,
        auto_server.endpoint(),
        E2E_PARAMETERS,
        "auto",
        false,
    );
    let auto_requests = auto_server.start_for_requests(1);
    let auto_translate = run_att_with_ui_locale(
        root,
        arguments(&["mz", "translate", "--name", JA_PROJECT, PROFILE]),
        "fr",
    );
    assert_success("auto follows effective UI locale", &auto_translate);
    let auto_requests = auto_requests.finish();
    assert_eq!(auto_requests.len(), 1);
    assert_standard_request_semantics(
        &auto_requests[0],
        &render_system_prompt(FR_AUTO_TEMPLATE, "ja", "zh-Hans"),
        &[SOURCE_TEXT],
    );

    initialize_prompt_project(root, CANONICAL_EN_US_PROJECT, &en_game, "en-us", "zh-Hans");
    assert_project_language_metadata(root, CANONICAL_EN_US_PROJECT, "en-US", "zh-Hans");
    let canonical_extract = run_att(
        root,
        arguments(&[
            "mz",
            "extract",
            "--name",
            CANONICAL_EN_US_PROJECT,
            "--builtin",
        ]),
    );
    assert_success("canonical en-US project extract", &canonical_extract);
    let ko_system = prompt_rpg_maker.join("ko/system.md");
    assert_prompt_failure_before_llm(
        root,
        CANONICAL_EN_US_PROJECT,
        "ko",
        false,
        "system.md",
        &["所需对象不存在"],
    );

    fs::create_dir_all(&ko_system).expect("system.md 同名目录探针应可建立");
    assert_prompt_failure_before_llm(
        root,
        CANONICAL_EN_US_PROJECT,
        "ko",
        false,
        "system.md",
        &["expected=file", "actual=not_file"],
    );
    fs::remove_dir(&ko_system).expect("system.md 同名目录探针应可删除");

    fs::write(&ko_system, [0xff, 0xfe]).expect("非法 UTF-8 system Prompt 应可写入");
    assert_prompt_failure_before_llm(
        root,
        CANONICAL_EN_US_PROJECT,
        "ko",
        false,
        "system.md",
        &["第 0 字节处的 UTF-8 无效", "无效长度为 1 字节"],
    );

    fs::write(&ko_system, " \r\n\t").expect("空白 system Prompt 应可写入");
    assert_prompt_failure_before_llm(
        root,
        CANONICAL_EN_US_PROJECT,
        "ko",
        false,
        "system.md",
        &["resource=prompt", "content=blank"],
    );

    fs::write(
        &ko_system,
        format!("{INVALID_PROMPT_BODY_SENTINEL} {{{{source_language}}}}"),
    )
    .expect("无效模板 Prompt 应可写入");
    assert_prompt_failure_before_llm(
        root,
        CANONICAL_EN_US_PROJECT,
        "ko",
        false,
        "system.md",
        &[
            "template_error=missing_variable",
            "variable=target_language",
        ],
    );

    fs::write(prompt_rpg_maker.join("fr/system.md"), [0xff, 0xfe])
        .expect("未选 locale 的损坏 system Prompt 应可写入");
    fs::write(prompt_rpg_maker.join("zh-Hans/thinking.md"), [0xff, 0xfe])
        .expect("关闭思考输出时的损坏 thinking Prompt 应可写入");
    let selected_server = BoundChatServer::bind();
    write_configuration(root, selected_server.endpoint(), E2E_PARAMETERS);
    let selected_requests = selected_server.start_for_requests(1);
    let selected_translate = run_att(
        root,
        arguments(&[
            "mz",
            "translate",
            "--name",
            CANONICAL_EN_US_PROJECT,
            PROFILE,
        ]),
    );
    assert_success("unselected damaged prompt resources", &selected_translate);
    let selected_requests = selected_requests.finish();
    assert_eq!(selected_requests.len(), 1);
    assert_standard_request_semantics(
        &selected_requests[0],
        &render_system_prompt(SHARED_TEMPLATE, "en-US", "zh-Hans"),
        &[EN_SOURCE],
    );
    assert_prompt_failure_before_llm(
        root,
        CANONICAL_EN_US_PROJECT,
        "zh-Hans",
        true,
        "thinking.md",
        &["第 0 字节处的 UTF-8 无效", "无效长度为 1 字节"],
    );
}

#[test]
fn managed_lua_translation_crosses_extract_translate_and_write_back_processes() {
    let temporary = tempfile::tempdir().expect("应可建立 Managed 纵向端到端测试目录");
    let root = temporary.path();
    let game_root = root.join("game");
    fs::create_dir_all(root.join("projects")).expect("项目根应可建立");
    fs::create_dir_all(root.join("scripts")).expect("Lua 脚本目录应可建立");
    write_minimal_mz_game(&game_root);
    fs::write(
        game_root.join("data/QuestEntries.json"),
        serde_json::to_vec(&json!([{
            "id": "arrival",
            "title": MANAGED_ORIGINAL,
            "description": "港へ向かう。"
        }]))
        .expect("Managed 来源应可序列化"),
    )
    .expect("Managed 来源应可写入");
    fs::write(
        root.join(MANAGED_LUA),
        include_str!("../docs/rpg-maker/examples/lua-managed-translation.lua"),
    )
    .expect("Managed 文档示例应可写入");

    initialize_prompt_project(root, MANAGED_PROJECT, &game_root, "ja", "zh-Hans");
    write_system_prompt(root, "zh-Hans", SYSTEM_PROMPT_TEMPLATE);

    let extract = run_att(
        root,
        arguments(&[
            "mz",
            "extract",
            "--name",
            MANAGED_PROJECT,
            "--lua",
            MANAGED_LUA,
        ]),
    );
    assert_success("Managed extract", &extract);

    let workspace = root.join("projects/mz").join(MANAGED_PROJECT);
    let database = workspace.join("project.db");
    let connection = Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("Managed 项目数据库应可只读打开");
    let declared = connection
        .query_row(
            r#"SELECT c.collection_name, c.instruction, u.unit_key, u.kind, u.shape,
                      u.original_content_json, u.context, u.metadata_json,
                      u.translation_content_json, u.translation_state
               FROM managed_translation_collection AS c
               JOIN managed_translation_unit AS u
                 ON u.owner = c.owner
                AND u.collection_name = c.collection_name
              WHERE c.owner = 'lua'"#,
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, Option<Vec<u8>>>(9)?,
                ))
            },
        )
        .expect("Managed 声明应在 Extract 后原子落库");
    assert_eq!(
        declared,
        (
            "quest_titles".to_owned(),
            "翻译任务标题；保持简洁，并结合任务说明判断含义。".to_owned(),
            "quest:arrival".to_owned(),
            "database_entry".to_owned(),
            "single".to_owned(),
            serde_json::to_string(MANAGED_ORIGINAL).expect("测试原文应可编码"),
            "任务标题；相关说明：港へ向かう。".to_owned(),
            r#"{"json_index":0,"quest_id":"arrival"}"#.to_owned(),
            None,
            None,
        )
    );
    drop(connection);

    let missing_write_back = run_att(
        root,
        arguments(&[
            "mz",
            "write-back",
            "--name",
            MANAGED_PROJECT,
            "--lua",
            MANAGED_LUA,
        ]),
    );
    assert_success("missing Managed write-back", &missing_write_back);
    assert_eq!(
        serde_json::from_slice::<Value>(
            &fs::read(workspace.join("write_back/data/QuestEntries.json"))
                .expect("无 Managed 译文时仍应写入 QuestEntries.json"),
        )
        .expect("无 Managed 译文时的 WriteBack 结果应是 JSON")[0]["title"],
        MANAGED_ORIGINAL,
        "WriteBack 的 missing 投影必须保持冻结原文"
    );

    let server = BoundChatServer::bind();
    write_configuration(root, server.endpoint(), E2E_PARAMETERS);
    enable_translation_task_records(root);
    let requests = server.start_with_responses_and_observe(
        vec![ChatResponseFixture::Managed],
        ChatResponseFixture::Managed,
    );
    let translate = run_att(
        root,
        arguments(&[
            "mz",
            "translate",
            "--name",
            MANAGED_PROJECT,
            PROFILE,
            "--lua",
            MANAGED_LUA,
        ]),
    );
    let translate_stdout = assert_success("Managed translate", &translate);
    assert!(
        translate_stdout
            .contains("标准翻译：任务 0，完整 0，部分 0，不可用 0；写入 0 处，剩余 0 处")
            && translate_stdout.contains("Lua：已执行"),
        "Managed-only Translate 应保持 Standard 与 Lua 可观察结果独立：{translate_stdout}"
    );
    let connection = Connection::open_with_flags(&database, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("Managed 项目数据库应可再次只读打开");
    let committed = connection
        .query_row(
            "SELECT translation_content_json, translation_state FROM managed_translation_unit",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .expect("Managed 译文与 state 应成对提交");
    assert_eq!(
        committed.0,
        serde_json::to_string(MANAGED_TRANSLATION).expect("测试译文应可编码")
    );
    assert_eq!(committed.1.len(), 32);
    drop(connection);

    let record = read_single_task_record(&workspace.join("task-records"));
    assert!(record.contains("## Managed"));
    assert!(record.contains("- Collection: `quest_titles`"));
    assert!(record.contains("`1` → `quest_titles`/`quest:arrival`"));
    assert!(record.contains("confirmed committed unit targets `1`"));
    assert!(record.contains(MANAGED_TRANSLATION));

    let converged = run_att(
        root,
        arguments(&[
            "mz",
            "translate",
            "--name",
            MANAGED_PROJECT,
            PROFILE,
            "--lua",
            MANAGED_LUA,
        ]),
    );
    assert_success("converged Managed translate", &converged);
    let requests = requests.finish();
    assert_eq!(
        requests.len(),
        1,
        "完全相同的 Profile 语义下，唯一 Managed unit 为 Current 时不得请求模型"
    );
    let request_body: Value =
        serde_json::from_slice(&requests[0].body).expect("Managed 请求体必须是 JSON");
    let messages = request_body["messages"]
        .as_array()
        .expect("Managed 请求必须包含 messages");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "system");
    let managed_system = messages[0]["content"]
        .as_str()
        .expect("Managed system message 必须是字符串");
    assert!(
        managed_system.starts_with(&format!(
            "{SYSTEM_PROMPT}\n\n# ATT Managed translation extension"
        )),
        "Managed 必须在未修改的项目 System Prompt 后追加自己的协议片段：{managed_system}"
    );
    assert!(managed_system.contains("`single string, LF allowed`"));
    assert!(managed_system.contains("array containing exactly one non-blank JSON string"));
    assert!(managed_system.contains("CR and NUL are forbidden"));
    assert_eq!(messages[1]["role"], "user");
    let user = messages[1]["content"]
        .as_str()
        .expect("Managed user message 必须是字符串");
    assert!(user.contains("翻译任务标题；保持简洁，并结合任务说明判断含义。"));
    assert!(user.contains("任务标题"));
    assert!(user.contains("Text [1] (single line)"));
    assert!(user.contains(MANAGED_ORIGINAL));
    for private in ["quest_titles", "quest:arrival", "json_index", "quest_id"] {
        assert!(
            !user.contains(private),
            "Managed user message 不得泄漏内部身份或 metadata：{private}\n{user}"
        );
    }
    assert_eq!(
        fs::read_dir(workspace.join("task-records"))
            .expect("任务记录根应存在")
            .count(),
        1,
        "零 Managed TaskBlock 不得建立空 run 目录"
    );

    let write_back = run_att(
        root,
        arguments(&[
            "mz",
            "write-back",
            "--name",
            MANAGED_PROJECT,
            "--lua",
            MANAGED_LUA,
        ]),
    );
    assert_success("Managed write-back", &write_back);
    assert_eq!(
        serde_json::from_slice::<Value>(
            &fs::read(workspace.join("write_back/data/QuestEntries.json"))
                .expect("Managed WriteBack 应写入 QuestEntries.json"),
        )
        .expect("Managed WriteBack 结果应是 JSON")[0]["title"],
        MANAGED_TRANSLATION,
    );
}

#[test]
fn enabled_task_recording_with_zero_standard_tasks_creates_no_directory() {
    const EMPTY_PROJECT: &str = "task-records-empty";

    let temporary = tempfile::tempdir().expect("应可建立零任务记录端到端测试目录");
    let root = temporary.path();
    let game_root = root.join("game");
    fs::create_dir_all(root.join("projects")).expect("项目根应可建立");
    write_minimal_mz_game(&game_root);
    initialize_prompt_project(root, EMPTY_PROJECT, &game_root, "ja", "zh-Hans");
    write_system_prompt(root, "zh-Hans", SYSTEM_PROMPT_TEMPLATE);

    let server = BoundChatServer::bind();
    write_configuration(root, server.endpoint(), E2E_PARAMETERS);
    enable_translation_task_records(root);
    let requests = server.start_observing_requests();
    let translate = run_att(
        root,
        arguments(&["mz", "translate", "--name", EMPTY_PROJECT, PROFILE]),
    );
    assert_success("zero Standard task translate", &translate);
    assert!(
        requests.finish().is_empty(),
        "没有 Standard TaskBlock 时不得发出模型请求"
    );
    assert!(
        !root
            .join("projects/mz")
            .join(EMPTY_PROJECT)
            .join("task-records")
            .exists(),
        "记录开启但没有 Standard TaskBlock 时不得创建空目录"
    );
}

#[test]
fn task_record_failure_is_reported_once_without_changing_translate_success() {
    const DEGRADED_PROJECT: &str = "task-records-degraded";

    let temporary = tempfile::tempdir().expect("应可建立任务记录降级端到端测试目录");
    let root = temporary.path();
    let game_root = root.join("game");
    fs::create_dir_all(root.join("projects")).expect("项目根应可建立");
    write_minimal_mz_game(&game_root);
    initialize_and_extract_prompt_project(root, DEGRADED_PROJECT, &game_root, "ja", "zh-Hans");
    write_system_prompt(root, "zh-Hans", SYSTEM_PROMPT_TEMPLATE);

    let server = BoundChatServer::bind();
    write_configuration(root, server.endpoint(), E2E_PARAMETERS);
    enable_translation_task_records(root);
    let workspace = root.join("projects/mz").join(DEGRADED_PROJECT);
    let task_records_root = workspace.join("task-records");
    fs::write(&task_records_root, b"not-a-directory")
        .expect("普通文件应可稳定触发任务记录写入降级");
    let requests = server.start_with_responses(vec![ChatResponseFixture::Standard]);

    let translate = run_att(
        root,
        arguments(&["mz", "translate", "--name", DEGRADED_PROJECT, PROFILE]),
    );

    let stdout = String::from_utf8(translate.stdout).expect("Translate stdout 必须是 UTF-8");
    let stderr = String::from_utf8(translate.stderr).expect("Translate stderr 必须是 UTF-8");
    let visible_stdout = without_fluent_isolation(&stdout);
    let visible_stderr = without_fluent_isolation(&stderr);
    assert_eq!(
        translate.status.code(),
        Some(0),
        "任务记录故障不得改变翻译成功退出码\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        visible_stdout.contains("翻译执行完成：task-records-degraded"),
        "成功输出必须保留完整 Translate 结果：{stdout}"
    );
    assert_eq!(requests.finish().len(), 1);
    assert_translation_committed(&workspace.join("project.db"));
    assert_eq!(
        visible_stderr
            .matches(TASK_RECORDS_DEGRADED_WARNING)
            .count(),
        1,
        "任务记录降级横幅在终态只能显示一次：{visible_stderr}"
    );
    assert_eq!(
        visible_stderr.matches(LOG_DEGRADED_WARNING).count(),
        0,
        "任务记录故障不得错误归入项目 JSONL 类别：{visible_stderr}"
    );
    assert!(
        visible_stderr.contains("task-records"),
        "任务记录诊断必须保留失败路径：{visible_stderr}"
    );
}

#[test]
fn thinking_output_keeps_reasoning_only_in_the_readable_task_record() {
    const SUCCESS_PROJECT: &str = "thinking-success";
    const RAW_JSON_PROJECT: &str = "thinking-raw-json";

    let temporary = tempfile::tempdir().expect("应可建立思考输出端到端测试目录");
    let root = temporary.path();
    let game_root = root.join("game");
    fs::create_dir_all(root.join("projects")).expect("项目根应可建立");
    write_minimal_mz_game(&game_root);
    initialize_and_extract_prompt_project(root, SUCCESS_PROJECT, &game_root, "ja", "zh-Hans");
    initialize_and_extract_prompt_project(root, RAW_JSON_PROJECT, &game_root, "ja", "zh-Hans");
    write_system_prompt(root, "zh-Hans", SYSTEM_PROMPT_TEMPLATE);
    write_thinking_prompt(root, "zh-Hans", &format!(" \r\n{THINKING_PROMPT}\r\n\t"));

    let baseline_server = BoundChatServer::bind();
    write_configuration(root, baseline_server.endpoint(), E2E_PARAMETERS);
    let baseline_requests =
        baseline_server.start_with_responses(vec![ChatResponseFixture::Standard]);
    let baseline = run_att(
        root,
        arguments(&["mz", "translate", "--name", SUCCESS_PROJECT, PROFILE]),
    );
    assert_success("JSON-only baseline translate", &baseline);
    assert_eq!(baseline_requests.finish().len(), 1);

    let success_server = BoundChatServer::bind();
    write_configuration_with_prompt_options(
        root,
        success_server.endpoint(),
        E2E_PARAMETERS,
        "zh-Hans",
        true,
    );
    enable_translation_task_records(root);
    let success_requests =
        success_server.start_with_responses(vec![ChatResponseFixture::ThinkingStandard]);
    let success = run_att(
        root,
        arguments(&["mz", "translate", "--name", SUCCESS_PROJECT, PROFILE]),
    );
    let success_stdout = assert_success("thinking envelope translate", &success);
    assert!(
        success_stdout.contains("任务 1，完整 1，部分 0，不可用 0"),
        "合法思考信封应正常提交译文：{success_stdout}"
    );
    let success_requests = success_requests.finish();
    assert_eq!(
        success_requests.len(),
        1,
        "开启 thinking_output 必须使旧 JSON-only 译文不再 Current"
    );
    assert_standard_request_semantics(
        &success_requests[0],
        &format!("{SYSTEM_PROMPT}\n\n{THINKING_PROMPT}"),
        &[SOURCE_TEXT],
    );

    let success_database = root
        .join("projects/mz")
        .join(SUCCESS_PROJECT)
        .join("project.db");
    assert_translation_committed(&success_database);
    assert_output_does_not_contain("thinking success", &success, THINKING_SENTINEL);
    assert_project_logs_do_not_contain(
        &root.join("projects/mz").join(SUCCESS_PROJECT).join("logs"),
        THINKING_SENTINEL,
        "项目日志",
    );
    assert_database_does_not_contain(&success_database, THINKING_SENTINEL);
    let success_record = read_single_task_record(
        &root
            .join("projects/mz")
            .join(SUCCESS_PROJECT)
            .join("task-records"),
    );
    assert!(success_record.contains("## Thinking\n"));
    assert!(success_record.contains(THINKING_SENTINEL));
    assert!(success_record.contains("## Assistant\n\n### ID 1\n"));
    assert!(success_record.contains("- 状态：完成，已确认提交"));

    let raw_json_server = BoundChatServer::bind();
    write_configuration_with_prompt_options(
        root,
        raw_json_server.endpoint(),
        E2E_PARAMETERS,
        "zh-Hans",
        true,
    );
    enable_translation_task_records(root);
    let raw_json_requests =
        raw_json_server.start_with_responses(vec![ChatResponseFixture::Standard]);
    let raw_json = run_att(
        root,
        arguments(&["mz", "translate", "--name", RAW_JSON_PROJECT, PROFILE]),
    );
    let raw_json_stdout = assert_success("thinking mode raw JSON", &raw_json);
    assert!(
        raw_json_stdout.contains("任务 1，完整 0，部分 0，不可用 1")
            && raw_json_stdout.contains("写入 0 处，剩余 1 处"),
        "思考模式必须把裸 JSON 计为 ModelResponseUnusable：{raw_json_stdout}"
    );
    let raw_json_requests = raw_json_requests.finish();
    assert_eq!(raw_json_requests.len(), 1, "信封错误不得作为网络错误重试");
    assert_standard_request_semantics(
        &raw_json_requests[0],
        &format!("{SYSTEM_PROMPT}\n\n{THINKING_PROMPT}"),
        &[SOURCE_TEXT],
    );
    let raw_json_database = root
        .join("projects/mz")
        .join(RAW_JSON_PROJECT)
        .join("project.db");
    assert_translation_absent(&raw_json_database);
    assert_output_does_not_contain("thinking raw JSON", &raw_json, THINKING_SENTINEL);
    assert_project_logs_do_not_contain(
        &root.join("projects/mz").join(RAW_JSON_PROJECT).join("logs"),
        THINKING_SENTINEL,
        "项目日志",
    );
    assert_database_does_not_contain(&raw_json_database, THINKING_SENTINEL);
    let invalid_record = read_single_task_record(
        &root
            .join("projects/mz")
            .join(RAW_JSON_PROJECT)
            .join("task-records"),
    );
    assert!(!invalid_record.contains("## Thinking\n"));
    assert!(invalid_record.contains("> 解析错误：模型响应缺少规定的思考信封"));
    assert!(invalid_record.contains(&format!("```text\n{{\"1\":[\"{TRANSLATION}\"]}}")));
}

fn enable_translation_task_records(root: &Path) {
    let path = root.join("config.toml");
    let configuration = fs::read_to_string(&path).expect("Translate 配置应可读取");
    fs::write(
        &path,
        configuration.replace(
            "record_translation_tasks = false",
            "record_translation_tasks = true",
        ),
    )
    .expect("应可启用 Standard 任务记录");
}

fn read_single_task_record(task_records_root: &Path) -> String {
    let run_directories = fs::read_dir(task_records_root)
        .expect("任务记录根应存在")
        .collect::<Result<Vec<_>, _>>()
        .expect("任务记录运行目录应可读取");
    assert_eq!(run_directories.len(), 1, "测试项目应只有一个记录运行");
    fs::read_to_string(run_directories[0].path().join("task-000001.md")).expect("任务记录应可读取")
}

fn initialize_and_extract_prompt_project(
    root: &Path,
    project: &str,
    game_root: &Path,
    source_language: &str,
    target_language: &str,
) {
    initialize_prompt_project(root, project, game_root, source_language, target_language);
    let extract = run_att(
        root,
        arguments(&["mz", "extract", "--name", project, "--builtin"]),
    );
    assert_success("prompt project extract", &extract);
}

fn initialize_prompt_project(
    root: &Path,
    project: &str,
    game_root: &Path,
    source_language: &str,
    target_language: &str,
) {
    let bootstrap_server = BoundChatServer::bind();
    write_configuration(root, bootstrap_server.endpoint(), EMPTY_PARAMETERS);
    let init = run_att(
        root,
        mz_init_arguments_for(
            game_root,
            project,
            source_language,
            target_language,
            24,
            30,
            40,
        ),
    );
    assert_success("prompt project init", &init);
}

fn assert_project_language_metadata(
    root: &Path,
    project: &str,
    source_language: &str,
    target_language: &str,
) {
    let database = root
        .join("projects")
        .join("mz")
        .join(project)
        .join("project.db");
    let connection = open_read_only(&database);
    let actual = connection
        .query_row(
            "SELECT source_language, target_language FROM metadata",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .expect("项目语言 metadata 应可读取");
    assert_eq!(
        actual,
        (source_language.to_owned(), target_language.to_owned())
    );
}

fn assert_prompt_failure_before_llm(
    root: &Path,
    project: &str,
    locale: &str,
    thinking_output: bool,
    component: &str,
    expected_reason_fragments: &[&str],
) {
    let server = BoundChatServer::bind();
    write_configuration_with_prompt_options(
        root,
        server.endpoint(),
        E2E_PARAMETERS,
        locale,
        thinking_output,
    );
    let requests = server.start_observing_requests();
    let output = run_att(
        root,
        arguments(&["mz", "translate", "--name", project, PROFILE]),
    );
    let requests = requests.finish();

    assert!(requests.is_empty(), "Prompt 失败前不得发出 LLM 请求");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = without_fluent_isolation(&String::from_utf8_lossy(&output.stderr));
    assert!(stderr.contains("错误 [prompt.unavailable]"), "{stderr}");
    assert!(stderr.contains("阶段：命令准备"), "{stderr}");
    assert!(
        stderr.contains(
            &prompt_locale_root(root, locale)
                .join(component)
                .display()
                .to_string()
        ),
        "{stderr}"
    );
    assert!(stderr.contains("原因："), "{stderr}");
    for fragment in expected_reason_fragments {
        assert!(
            stderr.contains(fragment),
            "缺少原因事实 {fragment:?}：{stderr}"
        );
    }
    assert!(stderr.contains("影响：状态未改变"), "{stderr}");
    assert!(
        stderr.contains("处理办法：修正指出的配置字段后重试"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&format!("locale={locale}; component={component}")),
        "{stderr}"
    );
    assert!(!stderr.contains(INVALID_PROMPT_BODY_SENTINEL), "{stderr}");
    assert_process_summary_omits_client_payloads("prompt failure", &output);
}

fn arguments(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

fn rpg_maker_init_arguments(engine: &str, project: &str, game_root: &Path) -> Vec<OsString> {
    let mut values = arguments(&[engine, "init", "--name", project, "--path"]);
    values.push(game_root.as_os_str().to_owned());
    values.extend(arguments(&[
        "--source-language",
        "JA",
        "--target-language",
        "zh-hans",
        "--dialogue-max-fullwidth-chars",
        "24",
        "--scrolling-text-max-fullwidth-chars",
        "30",
        "--help-description-max-fullwidth-chars",
        "40",
    ]));
    values
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
    mz_init_arguments_for(
        game_root,
        PROJECT,
        "JA",
        "zh-hans",
        dialogue,
        scrolling_text,
        help_description,
    )
}

#[allow(clippy::too_many_arguments)]
fn mz_init_arguments_for(
    game_root: &Path,
    project: &str,
    source_language: &str,
    target_language: &str,
    dialogue: u32,
    scrolling_text: u32,
    help_description: u32,
) -> Vec<OsString> {
    let mut values = arguments(&["mz", "init", "--name", project, "--path"]);
    values.push(game_root.as_os_str().to_owned());
    values.extend(arguments(&["--source-language", source_language]));
    values.extend(arguments(&["--target-language", target_language]));
    values.push("--dialogue-max-fullwidth-chars".into());
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

fn run_att_with_ui_locale(root: &Path, arguments: Vec<OsString>, ui_locale: &str) -> Output {
    let mut command = att_command_with_ui_locale_and_progress(root, arguments, ui_locale, "off");
    let child = command.spawn().expect("att.exe 应可启动");
    wait_for_att(child)
}

fn copy_att_executable(directory: &Path) -> PathBuf {
    fs::create_dir_all(directory).expect("复制 att.exe 的目标目录应可建立");
    let executable = directory.join("att.exe");
    let source = std::env::var_os("ATT_TEST_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_att")));
    assert!(
        source.is_absolute() && source.is_file(),
        "ATT_TEST_EXECUTABLE 必须指向现有的绝对 att.exe 路径：{}",
        source.display()
    );
    fs::copy(&source, &executable).expect("实际 att.exe 应可复制");
    executable
}

struct LoadedLibraryResource(*mut c_void);

impl Drop for LoadedLibraryResource {
    fn drop(&mut self) {
        // SAFETY: 该句柄由本测试中的 LoadLibraryExW 成功返回，并且只在这里释放一次。
        let _ = unsafe { free_library(self.0) };
    }
}

fn read_embedded_manifest_resource_one(executable: &Path) -> String {
    let wide_path = executable
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: 路径是 NUL 结尾 UTF-16；保留参数为 null；只把 PE 作为资源映像载入。
    let module = unsafe {
        load_library_ex_w(
            wide_path.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_AS_DATAFILE | LOAD_LIBRARY_AS_IMAGE_RESOURCE,
        )
    };
    assert!(
        !module.is_null(),
        "应可把复制后的 att.exe 作为资源映像加载：{}",
        io::Error::last_os_error()
    );
    let module = LoadedLibraryResource(module);

    // MAKEINTRESOURCEW 用指针低 16 位表达整数资源；这里精确读取 RT_MANIFEST ID 1。
    let resource_name = make_integer_resource(1);
    let resource_kind = make_integer_resource(RT_MANIFEST);
    // SAFETY: module 在 guard 生命周期内有效，两个资源标识都是 Win32 整数资源形式。
    let resource = unsafe { find_resource_w(module.0, resource_name, resource_kind) };
    assert!(
        !resource.is_null(),
        "att.exe 必须内嵌 RT_MANIFEST ID 1：{}",
        io::Error::last_os_error()
    );
    // SAFETY: module 与 resource 来自同一次 FindResourceW。
    let size = unsafe { size_of_resource(module.0, resource) };
    assert_ne!(size, 0, "RT_MANIFEST ID 1 不能为空");
    // SAFETY: module 与 resource 来自同一次 FindResourceW。
    let loaded = unsafe { load_resource(module.0, resource) };
    assert!(!loaded.is_null(), "RT_MANIFEST ID 1 应可加载");
    // SAFETY: loaded 是 LoadResource 返回的只读资源句柄。
    let bytes = unsafe { lock_resource(loaded) }.cast::<u8>();
    assert!(!bytes.is_null(), "RT_MANIFEST ID 1 应可锁定");
    // SAFETY: SizeofResource 给出该只读资源的精确字节数，module guard 在复制完成前有效。
    let bytes = unsafe { std::slice::from_raw_parts(bytes, size as usize) };
    std::str::from_utf8(bytes)
        .expect("内嵌 manifest 必须是 UTF-8")
        .to_owned()
}

fn make_integer_resource(identifier: u16) -> *const u16 {
    usize::from(identifier) as *const u16
}

fn initialize_unicode_lua_project(root: &Path, executable: &Path) {
    initialize_unicode_lua_project_from(root, executable, root);
}

fn initialize_unicode_lua_project_from(
    root: &Path,
    executable: &Path,
    process_current_directory: &Path,
) {
    let game_root = root.join("原游戏 中文 🚀 with spaces");
    fs::create_dir_all(root.join("projects")).expect("Unicode 项目根应可建立");
    fs::create_dir_all(root.join("prompts/rpg_maker")).expect("Unicode Prompt 根应可建立");
    write_minimal_mz_game(&game_root);

    let server = BoundChatServer::bind();
    write_configuration(root, server.endpoint(), EMPTY_PARAMETERS);
    let mut command = att_command_for_executable(
        executable,
        root,
        mz_init_arguments_for(&game_root, UNICODE_LUA_PROJECT, "ja", "zh-Hans", 24, 30, 40),
    );
    command.current_dir(process_current_directory);
    let child = command
        .spawn()
        .expect("指定 cwd 下复制后的 att.exe 应可启动");
    let init = wait_for_att(child);
    assert_success("Unicode 路径 init", &init);
    assert!(
        root.join("projects/mz")
            .join(UNICODE_LUA_PROJECT)
            .join("project.db")
            .is_file(),
        "Unicode 配置、游戏根和项目名称必须产生真实项目数据库"
    );
}

fn assert_unicode_require_contract(root: &Path, case_name: &str) {
    let process_current_directory = std::env::current_dir().expect("测试进程 cwd 应可读取");
    let executable_directory = root.join("中文主程序 🚀").join("带 空格");
    let executable = std::fs::canonicalize(copy_att_executable(&executable_directory))
        .expect("长路径或 UNC att.exe 应可解析成 Win32 扩展路径");
    initialize_unicode_lua_project_from(root, &executable, &process_current_directory);

    let script_directory = root.join("Lua require 中文 🚀 with spaces");
    let direct_directory = root.join("Lua C 路径桥 中文 🚀 with spaces");
    fs::create_dir_all(&script_directory).expect("Unicode require 脚本目录应可建立");
    fs::create_dir_all(&direct_directory).expect("Unicode Lua C 路径桥目录应可建立");
    fs::write(
        script_directory.join("相邻模块_🚀.lua"),
        "return 'Unicode require 已执行'\n",
    )
    .expect("Unicode require 相邻模块应可写入");
    let read_path = direct_directory.join("io.open 读取 中文 🚀.txt");
    let write_path = direct_directory.join("io.open 写入 中文 🚀.txt");
    let loadfile_path = direct_directory.join("loadfile 中文 🚀.lua");
    let dofile_path = direct_directory.join("dofile 中文 🚀.lua");
    let rename_source = direct_directory.join("os.rename 源 中文 🚀.txt");
    let rename_target = direct_directory.join("os.rename 目标 中文 🚀.txt");
    fs::write(&read_path, "长路径与 UNC 读取 中文 🚀").expect("Unicode Lua C 路径桥输入应可写入");
    fs::write(&loadfile_path, "return '长路径 loadfile 中文 🚀'\n")
        .expect("Unicode Lua C 路径桥 loadfile 应可写入");
    fs::write(&dofile_path, "return '路径桥 dofile 中文 🚀'\n")
        .expect("Unicode Lua C 路径桥 dofile 应可写入");
    fs::write(&rename_source, "等待 rename/remove")
        .expect("Unicode Lua C 路径桥 rename 源应可写入");
    let main_script = script_directory.join("主程序 中文 🚀.lua");
    fs::write(
        &main_script,
        r#"
assert(utf8.len(package.path) ~= nil, "package.path 必须是 UTF-8")
assert(string.find(package.path, arg[1], 1, true) ~= nil,
       "默认 package.path 必须包含实际 att.exe 目录")
assert(require("相邻模块_🚀") == "Unicode require 已执行")

local reader = assert(io.open(arg[2], "rb"))
assert(reader:read("*a") == "长路径与 UNC 读取 中文 🚀")
assert(reader:close())
local writer = assert(io.open(arg[3], "wb"))
assert(writer:write("Lua C 路径桥已写入 中文 🚀"))
assert(writer:close())
local loaded = assert(loadfile(arg[4]))
assert(loaded() == "长路径 loadfile 中文 🚀")
assert(dofile(arg[5]) == "路径桥 dofile 中文 🚀")
assert(os.rename(arg[6], arg[7]))
assert(os.remove(arg[7]))
"#,
    )
    .expect("Unicode require 主程序应可写入");

    let mut lua_arguments = arguments(&["mz", "lua", "--name", UNICODE_LUA_PROJECT]);
    lua_arguments.push(main_script.into_os_string());
    lua_arguments.push("--".into());
    lua_arguments.push(UNICODE_EXECUTABLE_DIRECTORY_MARKER.into());
    for argument in [
        &read_path,
        &write_path,
        &loadfile_path,
        &dofile_path,
        &rename_source,
        &rename_target,
    ] {
        lua_arguments.push(argument.as_os_str().to_owned());
    }
    let mut command = att_command_for_executable(&executable, root, lua_arguments);
    command.current_dir(process_current_directory);
    let child = command
        .spawn()
        .expect("长路径或 UNC 中复制后的 att.exe 应可启动");
    let output = wait_for_att(child);
    assert_success(&format!("{case_name} Unicode require"), &output);
    assert_eq!(
        fs::read_to_string(&write_path).expect("Lua C 路径桥输出应可读取"),
        "Lua C 路径桥已写入 中文 🚀"
    );
    assert!(
        !rename_source.exists() && !rename_target.exists(),
        "Lua C 路径桥的 rename/remove 必须完成"
    );
}

fn assert_process_summary_omits_client_payloads(phase: &str, output: &Output) {
    for (stream, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
        let text = String::from_utf8_lossy(bytes);
        assert!(
            !text.contains(API_KEY),
            "{phase} 的 {stream} 不得包含配置内 API key：{text}"
        );
        assert!(
            !text.contains(E2E_PARAMETER_MARKER),
            "{phase} 的 {stream} 只承担摘要职责，不应复制完整 parameters：{text}"
        );
    }
}

fn assert_output_does_not_contain(phase: &str, output: &Output, sentinel: &str) {
    for (stream, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
        let text = String::from_utf8_lossy(bytes);
        assert!(
            !text.contains(sentinel),
            "{phase} 的 {stream} 只承担摘要职责，不应复制 Thinking 正文：{text}"
        );
    }
}

fn assert_file_does_not_contain(path: &Path, sentinel: &str, label: &str) {
    let bytes = fs::read(path)
        .unwrap_or_else(|error| panic!("应可读取{label} {}：{error}", path.display()));
    assert!(
        find_subslice(&bytes, sentinel.as_bytes()).is_none(),
        "{label} {} 不得包含已丢弃的思考正文",
        path.display()
    );
}

fn assert_database_does_not_contain(database: &Path, sentinel: &str) {
    assert_file_does_not_contain(database, sentinel, "持久化数据库");
    for suffix in ["-wal", "-shm"] {
        let mut artifact = database.as_os_str().to_owned();
        artifact.push(suffix);
        let artifact = PathBuf::from(artifact);
        if artifact.exists() {
            assert_file_does_not_contain(&artifact, sentinel, "SQLite 附属文件");
        }
    }
}

fn without_fluent_isolation(text: &str) -> String {
    text.chars()
        .filter(|character| !matches!(character, '\u{2068}' | '\u{2069}'))
        .collect()
}

type OutputReader = JoinHandle<io::Result<Vec<u8>>>;

struct ObservableAttChild {
    child: Child,
    stdout_reader: Option<OutputReader>,
    stderr_reader: Option<OutputReader>,
    safe_stopping_receiver: mpsc::Receiver<()>,
    save_run_plan_receiver: mpsc::Receiver<()>,
}

impl ObservableAttChild {
    fn id(&self) -> u32 {
        self.child.id()
    }

    fn wait_until_safe_stopping(self) -> Self {
        match self
            .safe_stopping_receiver
            .recv_timeout(Duration::from_secs(5))
        {
            Ok(()) => self,
            Err(error) => {
                let output = self.terminate_and_collect();
                panic!(
                    "att.exe 未在放行 HTTP 响应前确认进入安全停止：{error}\nstatus: {:?}\nstdout:\n{}\nstderr:\n{}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }

    fn wait_until_saving_run_plan(self) -> Self {
        match self
            .save_run_plan_receiver
            .recv_timeout(Duration::from_secs(5))
        {
            Ok(()) => self,
            Err(error) => {
                let output = self.terminate_and_collect();
                panic!(
                    "att.exe 未在 5 秒内进入运行方案保存：{error}\nstatus: {:?}\nstdout:\n{}\nstderr:\n{}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }

    fn wait_until_fixture_marker(mut self, marker: &Path) -> Self {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if marker.is_file() {
                return self;
            }
            if self
                .child
                .try_wait()
                .expect("应可查询 att.exe 状态")
                .is_some()
            {
                let (stdout, stderr) = self.join_readers();
                panic!(
                    "att.exe 在建立测试同步标记前退出：{}\nstdout:\n{}\nstderr:\n{}",
                    marker.display(),
                    String::from_utf8_lossy(&stdout),
                    String::from_utf8_lossy(&stderr)
                );
            }
            if Instant::now() >= deadline {
                let output = self.terminate_and_collect();
                panic!(
                    "att.exe 未在 10 秒内建立测试同步标记：{}\nstdout:\n{}\nstderr:\n{}",
                    marker.display(),
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_output(mut self) -> Output {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            match self.child.try_wait().expect("应可查询 att.exe 状态") {
                Some(status) => {
                    let (stdout, stderr) = self.join_readers();
                    return Output {
                        status,
                        stdout,
                        stderr,
                    };
                }
                None if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(20));
                }
                None => {
                    let output = self.terminate_and_collect();
                    panic!(
                        "att.exe 在 30 秒内未退出\nstdout:\n{}\nstderr:\n{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
            }
        }
    }

    fn terminate_and_collect(mut self) -> Output {
        let _ = self.child.kill();
        let status = self.child.wait().expect("应可等待终止后的 att.exe");
        let (stdout, stderr) = self.join_readers();
        Output {
            status,
            stdout,
            stderr,
        }
    }

    fn join_readers(&mut self) -> (Vec<u8>, Vec<u8>) {
        (
            join_output_reader(&mut self.stdout_reader, "stdout"),
            join_output_reader(&mut self.stderr_reader, "stderr"),
        )
    }
}

impl Drop for ObservableAttChild {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let _ = take_output_reader(&mut self.stdout_reader);
        let _ = take_output_reader(&mut self.stderr_reader);
    }
}

fn spawn_observable_att_in_new_process_group(
    root: &Path,
    arguments: Vec<OsString>,
) -> ObservableAttChild {
    let mut command = att_command_with_progress(root, arguments, "plain");
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    let mut child = command.spawn().expect("独立进程组中的 att.exe 应可启动");
    let stdout = child.stdout.take().expect("att.exe stdout 应已管道化");
    let stderr = child.stderr.take().expect("att.exe stderr 应已管道化");
    let stdout_reader = spawn_output_reader("att-e2e-stdout", stdout);
    let (safe_stopping_sender, safe_stopping_receiver) = mpsc::channel();
    let (save_run_plan_sender, save_run_plan_receiver) = mpsc::channel();
    let stderr_reader = thread::Builder::new()
        .name(String::from("att-e2e-stderr"))
        .spawn(move || {
            let mut stderr = stderr;
            let mut captured = Vec::new();
            let mut buffer = [0_u8; 4096];
            let safe_stopping_marker = SAFE_STOPPING_PROGRESS.as_bytes();
            let save_run_plan_marker = SAVE_RUN_PLAN_PROGRESS.as_bytes();
            let mut safe_stopping_reported = false;
            let mut save_run_plan_reported = false;
            loop {
                let read = stderr.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                captured.extend_from_slice(&buffer[..read]);
                if !safe_stopping_reported
                    && captured
                        .windows(safe_stopping_marker.len())
                        .any(|window| window == safe_stopping_marker)
                {
                    safe_stopping_reported = true;
                    let _ = safe_stopping_sender.send(());
                }
                if !save_run_plan_reported
                    && captured
                        .windows(save_run_plan_marker.len())
                        .any(|window| window == save_run_plan_marker)
                {
                    save_run_plan_reported = true;
                    let _ = save_run_plan_sender.send(());
                }
            }
            Ok(captured)
        })
        .expect("应可启动 att.exe stderr 观察线程");

    ObservableAttChild {
        child,
        stdout_reader: Some(stdout_reader),
        stderr_reader: Some(stderr_reader),
        safe_stopping_receiver,
        save_run_plan_receiver,
    }
}

fn send_ctrl_break(child: &ObservableAttChild, stage: &str) {
    // SAFETY: `spawn_observable_att_in_new_process_group` 使用
    // CREATE_NEW_PROCESS_GROUP 启动子进程且继承当前控制台；目标进程组 ID
    // 就是子进程 ID，因此 CTRL_BREAK 不会投递给测试进程。
    let generated = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, child.id()) };
    assert_ne!(
        generated, 0,
        "应能向 {stage} att.exe 独立进程组发送 Ctrl-Break"
    );
}

fn att_command(root: &Path, arguments: Vec<OsString>) -> Command {
    att_command_with_progress(root, arguments, "off")
}

fn att_command_with_progress(root: &Path, arguments: Vec<OsString>, progress: &str) -> Command {
    att_command_with_ui_locale_and_progress(root, arguments, "zh-Hans", progress)
}

fn att_command_with_ui_locale_and_progress(
    root: &Path,
    arguments: Vec<OsString>,
    ui_locale: &str,
    progress: &str,
) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_att"));
    command
        .current_dir(root)
        .arg("--config")
        .arg(root.join("config.toml"))
        .args(["--ui-language", ui_locale, "--progress", progress])
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn att_command_for_executable(executable: &Path, root: &Path, arguments: Vec<OsString>) -> Command {
    let mut command = Command::new(executable);
    command
        .current_dir(root)
        .env_remove("LUA_PATH")
        .env_remove("LUA_PATH_5_4")
        .arg("--config")
        .arg(root.join("config.toml"))
        .args(["--ui-language", "zh-Hans", "--progress", "off"])
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn spawn_output_reader(thread_name: &str, mut reader: impl Read + Send + 'static) -> OutputReader {
    thread::Builder::new()
        .name(thread_name.to_owned())
        .spawn(move || {
            let mut captured = Vec::new();
            reader.read_to_end(&mut captured)?;
            Ok(captured)
        })
        .expect("应可启动 att.exe 输出收集线程")
}

fn join_output_reader(reader: &mut Option<OutputReader>, stream: &str) -> Vec<u8> {
    take_output_reader(reader).unwrap_or_else(|error| panic!("收集 att.exe {stream} 失败：{error}"))
}

fn take_output_reader(reader: &mut Option<OutputReader>) -> io::Result<Vec<u8>> {
    let Some(reader) = reader.take() else {
        return Ok(Vec::new());
    };
    reader
        .join()
        .map_err(|_| io::Error::other("att.exe 输出收集线程 panic"))?
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
        .chars()
        .filter(|character| !matches!(character, '\u{2068}' | '\u{2069}'))
        .collect()
}

fn assert_cooperatively_cancelled(stage: &str, output: &Output) {
    let stdout = String::from_utf8(output.stdout.clone()).expect("取消 stdout 必须是 UTF-8");
    let stderr = without_fluent_isolation(
        std::str::from_utf8(&output.stderr).expect("取消 stderr 必须是 UTF-8"),
    );
    assert_eq!(
        output.status.code(),
        Some(130),
        "{stage} 真正取消必须退出 130\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.is_empty(),
        "{stage} 真正取消不得打印业务成功文案：{stdout}"
    );
    assert_eq!(
        stderr.matches(SAFE_STOPPING_PROGRESS).count(),
        1,
        "{stage} 必须且只能呈现一次安全停止进度：{stderr}"
    );
    assert!(
        stderr.ends_with("命令已在安全收尾后取消。\n"),
        "{stage} 必须呈现合作取消终态：{stderr}"
    );
    assert!(!stderr.contains('\r'));
    assert!(!stderr.contains('\u{001b}'));
}

fn assert_log_degraded_diagnostic(stderr: &[u8], log_root: &Path) {
    let stderr = String::from_utf8(stderr.to_vec()).expect("日志降级警告必须是 UTF-8");
    let visible = without_fluent_isolation(&stderr);
    assert_eq!(visible.matches(LOG_DEGRADED_WARNING).count(), 1);
    assert!(visible.contains("错误 [log.start]"), "{visible}");
    assert!(visible.contains("阶段：项目日志"), "{visible}");
    let visible_path = visible.replace(r"\\?\", "");
    let expected_path = log_root.to_string_lossy().replace('/', r"\");
    assert!(
        visible_path.contains(&expected_path),
        "日志故障必须显示安全路径：{visible}"
    );
    assert!(visible.contains("原因："), "{visible}");
    assert!(visible.contains("处理办法："), "{visible}");
}

fn assert_plan_source(stdout: &str, source: &str) {
    assert!(
        stdout.contains(&format!("已保存本次成功运行方案。 ({source})")),
        "成功摘要必须说明运行方案来源 {source}：\n{stdout}"
    );
}

fn assert_translate_plan_sources(stdout: &str, profile_source: &str, lua_source: &str) {
    assert!(
        stdout.contains(&format!(
            "已保存本次成功运行方案。Profile 来源：{profile_source}；Lua 来源：{lua_source}。"
        )),
        "Translate 成功摘要必须分别说明 Profile 与 Lua 来源：\n{stdout}"
    );
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
    fs::write(js.join("rmmz_core.js"), "/* MZ core */").expect("MZ core 标记应可写入");
}

fn write_oversized_system_source(game_root: &Path, source_text: &str) {
    fs::write(
        game_root.join("data/System.json"),
        serde_json::to_vec(&json!({
            "gameTitle": "",
            "currencyUnit": "",
            "terms": {
                "basic": [],
                "commands": [],
                "params": [],
                "messages": { "oversized": source_text }
            },
            "elements": [],
            "skillTypes": [],
            "weaponTypes": [],
            "armorTypes": [],
            "equipTypes": []
        }))
        .expect("超目标 System 夹具应可序列化"),
    )
    .expect("超目标 System 夹具应可写入");
}

fn write_mixed_semantic_mz_game(game_root: &Path) {
    write_minimal_mz_game(game_root);
    let data = game_root.join("data");
    fs::write(data.join("Items.json"), b"[null]").expect("混合 Map 不应额外产生 Items 单元");
    fs::write(
        data.join("Map001.json"),
        serde_json::to_vec(&json!({
            "displayName": MIXED_MAP_NAME,
            "events": [
                null,
                {
                    "id": 1,
                    "name": "Semantic Units",
                    "note": "",
                    "pages": [{
                        "conditions": {},
                        "image": {},
                        "moveRoute": { "list": [{ "code": 0, "parameters": [] }] },
                        "list": [
                            {
                                "code": 101,
                                "indent": 0,
                                "parameters": ["", 0, 0, 2, MIXED_SPEAKER]
                            },
                            {
                                "code": 401,
                                "indent": 0,
                                "parameters": [MIXED_DIALOGUE_SOURCE[0]]
                            },
                            {
                                "code": 401,
                                "indent": 0,
                                "parameters": [MIXED_DIALOGUE_SOURCE[1]]
                            },
                            {
                                "code": 401,
                                "indent": 0,
                                "parameters": [MIXED_DIALOGUE_SOURCE[2]]
                            },
                            {
                                "code": 102,
                                "indent": 0,
                                "parameters": [MIXED_CHOICES_SOURCE, -2, 0, 2, 0]
                            },
                            {
                                "code": 402,
                                "indent": 0,
                                "parameters": [0, MIXED_CHOICES_SOURCE[0]]
                            },
                            { "code": 0, "indent": 1, "parameters": [] },
                            {
                                "code": 402,
                                "indent": 0,
                                "parameters": [1, MIXED_CHOICES_SOURCE[1]]
                            },
                            { "code": 0, "indent": 1, "parameters": [] },
                            { "code": 404, "indent": 0, "parameters": [] },
                            { "code": 105, "indent": 0, "parameters": [2, false] },
                            {
                                "code": 405,
                                "indent": 0,
                                "parameters": [MIXED_SCROLLING_SOURCE[0]]
                            },
                            {
                                "code": 405,
                                "indent": 0,
                                "parameters": [MIXED_SCROLLING_SOURCE[1]]
                            },
                            {
                                "code": 405,
                                "indent": 0,
                                "parameters": [MIXED_SCROLLING_SOURCE[2]]
                            },
                            { "code": 0, "indent": 0, "parameters": [] }
                        ]
                    }]
                }
            ]
        }))
        .expect("混合 Map 夹具应可序列化"),
    )
    .expect("混合 Map 夹具应可写入");
}

fn write_manual_standard_mz_game(game_root: &Path) {
    write_minimal_mz_game(game_root);
    let data = game_root.join("data");
    fs::write(
        data.join("Items.json"),
        serde_json::to_vec(&json!([
            null,
            {
                "id": 1,
                "name": MANUAL_STANDARD_ITEM_SOURCE,
                "description": ""
            },
            {
                "id": 2,
                "name": MANUAL_STANDARD_ITEM_SOURCE,
                "description": ""
            }
        ]))
        .expect("人工补译 Items 夹具应可序列化"),
    )
    .expect("人工补译 Items 夹具应可写入");
    fs::write(
        data.join("Map001.json"),
        serde_json::to_vec(&json!({
            "displayName": "",
            "events": [
                null,
                {
                    "id": 1,
                    "name": "Manual Standard",
                    "note": "",
                    "pages": [{
                        "conditions": {},
                        "image": {},
                        "moveRoute": { "list": [{ "code": 0, "parameters": [] }] },
                        "list": [
                            {
                                "code": 101,
                                "indent": 0,
                                "parameters": ["", 0, 0, 2, ""]
                            },
                            {
                                "code": 401,
                                "indent": 0,
                                "parameters": [MANUAL_STANDARD_DIALOGUE_SOURCE[0]]
                            },
                            {
                                "code": 401,
                                "indent": 0,
                                "parameters": [MANUAL_STANDARD_DIALOGUE_SOURCE[1]]
                            },
                            { "code": 0, "indent": 0, "parameters": [] }
                        ]
                    }]
                }
            ]
        }))
        .expect("人工补译 Map 夹具应可序列化"),
    )
    .expect("人工补译 Map 夹具应可写入");
}

fn write_minimal_mv_game(game_root: &Path) {
    let content_root = game_root.join("www");
    write_minimal_mz_game(&content_root);
    let data = content_root.join("data");
    let js = content_root.join("js");
    fs::remove_file(js.join("rmmz_core.js")).expect("共用夹具中的 MZ core 标记应可删除");
    fs::write(js.join("rpg_core.js"), "/* MV core */").expect("MV core 标记应可写入");
    fs::write(data.join("Items.json"), b"[null]").expect("MV 空 Items 应可写入");
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
                            {
                                "code": 101,
                                "indent": 0,
                                "parameters": ["", 0, 0, 2]
                            },
                            {
                                "code": 401,
                                "indent": 0,
                                "parameters": [format!(r"\n<{MV_SPEAKER}>{MV_BODY}")]
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
assert(type(ctx.rpg_maker) == "table")
assert(type(io.open) == "function")
assert(type(os.execute) == "function")
assert(type(debug.getinfo) == "function")

local metadata = ctx.db.query("SELECT name, source_language, target_language FROM metadata")
assert(#metadata == 1)
assert(metadata[1][1] == "e2e")
assert(metadata[1][2] == "ja")
assert(metadata[1][3] == "zh-Hans")

ctx.db.begin()
ctx.db.execute("CREATE TABLE IF NOT EXISTS lua_process_probe (phase TEXT NOT NULL PRIMARY KEY, detail TEXT NOT NULL)")
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
assert(type(ctx.rpg_maker) == "table")

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
assert(type(ctx.rpg_maker) == "table")

ctx.db.begin()
local translated = ctx.db.query(
  "SELECT json_extract(translation_content_json, '$') FROM standard_text_unit WHERE source_content_json = json_quote(?1)",
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

fn write_cancellable_extract_lua(root: &Path) {
    fs::write(
        root.join(EXTRACT_LUA),
        r#"
assert(ctx.phase == "extract")
ctx.db.begin()
ctx.db.execute("CREATE TABLE extract_cancel_probe (value TEXT NOT NULL)")
ctx.db.execute("INSERT INTO extract_cancel_probe (value) VALUES ('must-roll-back')")
local marker = assert(io.open("extract-cancel-ready", "wb"))
assert(marker:write("ready"))
assert(marker:close())
while true do
end
"#,
    )
    .expect("可取消 Extract Lua 夹具应可写入");
}

fn write_cancellable_translate_lua(root: &Path) {
    fs::write(
        root.join(TRANSLATE_LUA),
        r#"
assert(ctx.phase == "translate")
ctx.db.begin()
ctx.db.execute("CREATE TABLE translate_cancel_probe (value TEXT NOT NULL)")
ctx.db.execute("INSERT INTO translate_cancel_probe (value) VALUES ('must-roll-back')")
local marker = assert(io.open("translate-cancel-ready", "wb"))
assert(marker:write("ready"))
assert(marker:close())
while true do
end
"#,
    )
    .expect("可取消 Translate Lua 夹具应可写入");
}

fn write_cancellable_write_back_lua(root: &Path) {
    fs::write(
        root.join(WRITE_BACK_LUA),
        r#"
assert(ctx.phase == "write_back")
ctx.output.write_text("js/cancelled-candidate.txt", "must-not-publish")
ctx.db.begin()
ctx.db.execute("CREATE TABLE write_back_cancel_probe (value TEXT NOT NULL)")
ctx.db.execute("INSERT INTO write_back_cancel_probe (value) VALUES ('must-roll-back')")
local marker = assert(io.open("write-back-cancel-ready", "wb"))
assert(marker:write("ready"))
assert(marker:close())
while true do
end
"#,
    )
    .expect("可取消 WriteBack Lua 夹具应可写入");
}

fn write_completed_extract_wait_lua(root: &Path) {
    fs::write(
        root.join(EXTRACT_LUA),
        r#"
assert(ctx.phase == "extract")
local marker = assert(io.open("completed-extract-ready", "wb"))
assert(marker:write("ready"))
assert(marker:close())
while true do
  local release = io.open("completed-extract-release", "rb")
  if release ~= nil then
    release:close()
    break
  end
end
"#,
    )
    .expect("完成后信号 Extract Lua 夹具应可写入");
}

fn write_updated_write_back_lua(root: &Path) {
    fs::write(
        root.join(WRITE_BACK_LUA),
        r#"
assert(ctx.phase == "write_back")
assert(ctx.project.name == "e2e")
assert(ctx.project.output_root ~= nil)
assert(ctx.llm == nil)
assert(type(ctx.rpg_maker) == "table")

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
    let definition = format!("[[rule]]\nfile = 'Items.json'\npath = '[].{field_name}'\n");
    fs::write(root.join(RULES_TOML), definition).expect("Rules 夹具应可写入");
}

fn write_terminology(root: &Path) {
    fs::write(
        root.join(TERMS_TOML),
        "[[term]]\nterm = '上薬草'\ntranslation = '高级药草'\ntriggers = ['上薬草']\n",
    )
    .expect("术语夹具应可写入");
}

fn write_placeholders(root: &Path) {
    fs::write(root.join(PLACEHOLDERS_TOML), "rule = []\n").expect("显式空占位符规则夹具应可写入");
}

fn write_mv_dialogue_rules(root: &Path) {
    fs::write(
        root.join("dialogue.toml"),
        "[[rule]]\npattern = '(?i)\\\\n<(?<speaker>[^>]*?)(?::)?>'\n",
    )
    .expect("MV 对话姓名投影夹具应可写入");
}

fn prompt_locale_root(root: &Path, locale: &str) -> PathBuf {
    root.join("prompts").join("rpg_maker").join(locale)
}

fn write_system_prompt(root: &Path, locale: &str, template: &str) {
    let locale_root = prompt_locale_root(root, locale);
    fs::create_dir_all(&locale_root).expect("Prompt locale 目录应可建立");
    fs::write(locale_root.join("system.md"), template).expect("system Prompt 应可写入");
}

fn write_thinking_prompt(root: &Path, locale: &str, prompt: &str) {
    let locale_root = prompt_locale_root(root, locale);
    fs::create_dir_all(&locale_root).expect("Prompt locale 目录应可建立");
    fs::write(locale_root.join("thinking.md"), prompt).expect("thinking Prompt 应可写入");
}

fn render_system_prompt(template: &str, source_language: &str, target_language: &str) -> String {
    template
        .replace("{{source_language}}", source_language)
        .replace("{{target_language}}", target_language)
}

fn write_configuration(root: &Path, url: &str, parameters: &str) {
    write_configuration_with_prompt_options(root, url, parameters, "zh-Hans", false);
}

fn write_configuration_with_task_target(
    root: &Path,
    url: &str,
    parameters: &str,
    target_task_user_message_characters: usize,
) {
    write_configuration_with_profile_options(
        root,
        url,
        parameters,
        "zh-Hans",
        false,
        target_task_user_message_characters,
    );
}

fn write_configuration_with_prompt_options(
    root: &Path,
    url: &str,
    parameters: &str,
    prompt_locale: &str,
    thinking_output: bool,
) {
    write_configuration_with_profile_options(
        root,
        url,
        parameters,
        prompt_locale,
        thinking_output,
        10_000,
    );
}

fn write_configuration_with_profile_options(
    root: &Path,
    url: &str,
    parameters: &str,
    prompt_locale: &str,
    thinking_output: bool,
    target_task_user_message_characters: usize,
) {
    let configuration = format!(
        r#"[projects]
root = "projects"

[prompts]
root = "prompts"
locale = "{prompt_locale}"
thinking_output = {thinking_output}

[llm.clients.primary]
url = "{url}"
api_key = "{API_KEY}"
model = "e2e-model"
max_concurrent_requests = 2
connect_timeout_ms = 5000
read_timeout_ms = 10000
request_timeout_ms = 10000
proxy = false
additional_pem_files = []
retry_delays_ms = [10]
max_retry_after_ms = 1000
parameters = '''{parameters}'''

[llm.clients.primary.rate_limit]
requests_per_minute = 60000
burst = 4

[[languages]]
type = "japanese"
id = "ja"
minimum_kana_characters = 1
allowed_terms = []
quote_repair_pairs = [["“", "”"], ["‘", "’"]]

[[languages]]
type = "english"
id = "en"
minimum_word_count = 1
minimum_letter_count = 2
ignored_terms = []
minimum_copied_word_count = 2
minimum_copied_letter_count = 4
allowed_terms = []

[[languages]]
type = "english"
id = "en-US"
minimum_word_count = 1
minimum_letter_count = 2
ignored_terms = []
minimum_copied_word_count = 2
minimum_copied_letter_count = 4
allowed_terms = []

[rpg_maker]
record_translation_tasks = false

[[rpg_maker.translation_profiles]]
id = "local"
llm_client = "primary"
target_task_user_message_characters = {target_task_user_message_characters}

[[rpg_maker.translation_profiles]]
id = "unselected"
llm_client = "primary"
target_task_user_message_characters = {target_task_user_message_characters}
"#
    );
    fs::write(root.join("config.toml"), configuration).expect("完整配置应可写入");
}

fn remove_translation_profile_from_configuration(root: &Path, profile: &str) {
    let path = root.join("config.toml");
    let mut configuration = fs::read_to_string(&path).expect("完整配置应可读取");
    let profile_start = format!("[[rpg_maker.translation_profiles]]\nid = \"{profile}\"");
    let start = configuration
        .find(&profile_start)
        .expect("待删除的 Profile 应存在于配置中");
    let following = &configuration[start + profile_start.len()..];
    let end = following
        .find("\n[[rpg_maker.translation_profiles]]")
        .map_or(configuration.len(), |offset| {
            start + profile_start.len() + offset + 1
        });
    configuration.replace_range(start..end, "");
    fs::write(path, configuration).expect("删除 Profile 后的配置应可写入");
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

fn assert_extract_run_plan(database: &Path, expected: Option<(bool, bool, bool)>) {
    let connection = open_read_only(database);
    let actual = connection
        .query_row(
            "SELECT builtin_enabled, rules_enabled, lua_enabled FROM extract_run_plan WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, bool>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            },
        )
        .optional()
        .expect("Extract 运行方案应可读取");
    assert_eq!(actual, expected, "Extract 运行方案必须精确替换 owner 集合");
}

fn assert_translate_run_plan(database: &Path, profile: &str, lua_enabled: bool) {
    let connection = open_read_only(database);
    let actual_profile: String = connection
        .query_row(
            "SELECT profile_id FROM translate_run_plan WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .expect("Translate 运行方案应可读取");
    let actual_lua: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM lua_program WHERE phase = 'translate')",
            [],
            |row| row.get(0),
        )
        .expect("Translate Lua 方案应可读取");
    assert_eq!(actual_profile, profile);
    assert_eq!(actual_lua, lua_enabled);
}

fn assert_write_back_run_plan(database: &Path, lua_enabled: bool) {
    let connection = open_read_only(database);
    let actual: (bool, bool) = connection
        .query_row(
            "SELECT lua_enabled, EXISTS(SELECT 1 FROM lua_program WHERE phase = 'write_back') FROM write_back_run_plan WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("WriteBack 运行方案应可读取");
    assert_eq!(actual, (lua_enabled, lua_enabled));
}

#[derive(Debug, PartialEq, Eq)]
struct SavedPhasePlanSnapshot {
    init_source_path_utf16: Option<Vec<u8>>,
    extract: Option<(bool, bool, bool)>,
    translate_profile: Option<String>,
    write_back_lua: Option<bool>,
    lua_programs: i64,
}

fn read_saved_phase_plan_snapshot(database: &Path) -> SavedPhasePlanSnapshot {
    let connection = open_read_only(database);
    let init = connection
        .query_row(
            "SELECT source_path_utf16 FROM init_run_plan WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .expect("Init 运行方案快照应可读取");
    let extract = connection
        .query_row(
            "SELECT builtin_enabled, rules_enabled, lua_enabled \
             FROM extract_run_plan WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .expect("Extract 运行方案快照应可读取");
    let translate = connection
        .query_row(
            "SELECT profile_id FROM translate_run_plan WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .expect("Translate 运行方案快照应可读取");
    let write_back = connection
        .query_row(
            "SELECT lua_enabled FROM write_back_run_plan WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .optional()
        .expect("WriteBack 运行方案快照应可读取");
    let lua_programs = connection
        .query_row("SELECT COUNT(*) FROM lua_program", [], |row| row.get(0))
        .expect("阶段 Lua 快照数量应可读取");
    SavedPhasePlanSnapshot {
        init_source_path_utf16: init,
        extract,
        translate_profile: translate,
        write_back_lua: write_back,
        lua_programs,
    }
}

fn assert_database_table_absent(database: &Path, table: &str) {
    let connection = open_read_only(database);
    let exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get(0),
        )
        .expect("应可检查取消事务是否留下表");
    assert!(!exists, "取消的 Lua 事务不得留下表 {table}");
}

fn snapshot_directory_tree(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut snapshot = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("应可读取目录快照 {}：{error}", directory.display()))
            .collect::<Result<Vec<_>, _>>()
            .expect("目录快照条目应可读取");
        for entry in entries {
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .expect("目录快照条目必须位于根内")
                .to_path_buf();
            let file_type = entry.file_type().expect("目录快照条目类型应可读取");
            if file_type.is_dir() {
                snapshot.insert(relative, None);
                pending.push(path);
            } else {
                assert!(
                    file_type.is_file(),
                    "目录快照不得包含链接：{}",
                    path.display()
                );
                snapshot.insert(
                    relative,
                    Some(fs::read(&path).unwrap_or_else(|error| {
                        panic!("应可读取快照文件 {}：{error}", path.display())
                    })),
                );
            }
        }
    }
    snapshot
}

fn directory_publish_artifacts(root: &Path) -> Vec<PathBuf> {
    let mut artifacts = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = fs::read_dir(&directory)
            .unwrap_or_else(|error| panic!("应可检查发布候选残留 {}：{error}", directory.display()))
            .collect::<Result<Vec<_>, _>>()
            .expect("发布候选残留条目应可读取");
        for entry in entries {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if name.starts_with(".directory-publish-") {
                artifacts.push(path);
                continue;
            }
            if entry
                .file_type()
                .expect("发布候选残留条目类型应可读取")
                .is_dir()
            {
                pending.push(path);
            }
        }
    }
    artifacts.sort();
    artifacts
}

fn assert_no_directory_publish_artifacts(root: &Path) {
    let artifacts = directory_publish_artifacts(root);
    assert!(
        artifacts.is_empty(),
        "命令终态不得遗留目录发布候选、备份或日志：{artifacts:?}"
    );
}

fn assert_missing_extract_plan(root: &Path, stage: &str) {
    let output = run_att(root, arguments(&["mz", "extract", "--name", PROJECT]));
    assert_eq!(
        output.status.code(),
        Some(1),
        "{stage} 后省略 owner 必须失败"
    );
    assert!(output.stdout.is_empty());
    let stderr = without_fluent_isolation(&String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        stderr,
        "错误 [command.run_plan]\n\
         阶段：命令准备\n\
         位置：extract_selection\n\
         原因：项目没有可复用的 Extract 方案；必须提供 --builtin、--rules 或 --lua 中的至少一项\n\
         影响：状态未改变\n\
         处理办法：修正指出的输入后重试\n"
    );
}

fn assert_extracted_database(database: &Path) {
    let connection = open_read_only(database);
    let expected_tables = BTreeSet::from([
        "standard_text_group".to_owned(),
        "standard_text_unit".to_owned(),
        "standard_mutation_claim".to_owned(),
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
    let row: (String, String, String, String, Option<String>) = connection
        .query_row(
            "SELECT text_group.owner, text_group.group_kind, unit.unit_role, \
                    unit.source_content_json, unit.translation_content_json \
             FROM standard_text_group AS text_group \
             JOIN standard_text_unit AS unit \
               ON unit.owner = text_group.owner \
              AND unit.group_location = text_group.group_location",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .expect("Builtin 应写入唯一 Items 语义单元");
    assert_eq!(row.0, "builtin");
    assert_eq!(row.1, "database_entry");
    assert_eq!(
        serde_json::from_str::<Value>(&row.2).expect("逻辑角色应为规范 JSON"),
        json!({ "f": "description" })
    );
    assert_eq!(
        serde_json::from_str::<Value>(&row.3).expect("源内容应为规范 JSON"),
        json!(SOURCE_TEXT)
    );
    assert_eq!(row.4, None);

    let claims: i64 = connection
        .query_row("SELECT COUNT(*) FROM standard_mutation_claim", [], |row| {
            row.get(0)
        })
        .expect("物理修改 Claim 数量应可查询");
    assert!(claims > 0, "唯一文本组应拥有展开后的物理修改 Claim");
}

fn assert_mixed_semantic_units_extracted(database: &Path) {
    let connection = open_read_only(database);
    let mut statement = connection
        .prepare(
            "SELECT unit_role, source_content_json, source_context_json \
             FROM standard_text_unit WHERE owner = 'builtin' ORDER BY group_location, unit_role",
        )
        .expect("混合 Map 单元查询应可准备");
    let units = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .expect("混合 Map 单元查询应可执行")
        .collect::<Result<Vec<_>, _>>()
        .expect("混合 Map 单元应可读取");
    assert_eq!(units.len(), 5, "五种类型必须各形成一个语义单元");

    let decoded = units
        .iter()
        .map(|(role, source, context)| {
            (
                serde_json::from_str::<Value>(role).expect("unit_role 应为规范 JSON"),
                serde_json::from_str::<Value>(source).expect("source_content_json 应为规范 JSON"),
                serde_json::from_str::<Value>(context).expect("source_context_json 应为规范 JSON"),
            )
        })
        .collect::<Vec<_>>();
    for expected in [
        (
            json!({ "f": "displayName" }),
            json!(MIXED_MAP_NAME),
            json!({}),
        ),
        (json!("p"), json!(MIXED_SPEAKER), json!({})),
        (
            json!("b"),
            json!(MIXED_DIALOGUE_SOURCE),
            json!({ "source_speaker": MIXED_SPEAKER }),
        ),
        (json!("c"), json!(MIXED_CHOICES_SOURCE), json!({})),
        (json!("r"), json!(MIXED_SCROLLING_SOURCE), json!({})),
    ] {
        assert!(decoded.contains(&expected), "缺少语义单元：{expected:?}");
    }
}

fn assert_mixed_semantic_translations(database: &Path) {
    let connection = open_read_only(database);
    let mut statement = connection
        .prepare(
            "SELECT unit_role, translation_content_json \
             FROM standard_text_unit WHERE owner = 'builtin' ORDER BY group_location, unit_role",
        )
        .expect("混合 Map 译文查询应可准备");
    let translations = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("混合 Map 译文查询应可执行")
        .map(|row| {
            let (role, translation) = row.expect("混合 Map 译文行应可读取");
            (
                serde_json::from_str::<Value>(&role).expect("unit_role 应为规范 JSON"),
                serde_json::from_str::<Value>(&translation)
                    .expect("translation_content_json 应为规范 JSON"),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(translations.len(), 5);
    for expected in [
        (
            json!({ "f": "displayName" }),
            json!(MIXED_MAP_NAME_TRANSLATION),
        ),
        (json!("b"), json!(MIXED_DIALOGUE_TRANSLATION)),
        (json!("p"), json!(MIXED_SPEAKER_TRANSLATION)),
        (json!("c"), json!(MIXED_CHOICES_TRANSLATION)),
        (json!("r"), json!(MIXED_SCROLLING_TRANSLATION)),
    ] {
        assert!(
            translations.contains(&expected),
            "缺少已提交译文：{expected:?}"
        );
    }
}

fn assert_manual_standard_units_extracted(database: &Path) {
    let connection = open_read_only(database);
    let (units, translated, states): (i64, i64, i64) = connection
        .query_row(
            "SELECT COUNT(*), COUNT(translation_content_json), COUNT(translation_state) \
             FROM standard_text_unit WHERE owner = 'builtin'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("人工补译单元数量应可读取");
    assert_eq!((units, translated, states), (3, 0, 0));

    let duplicate_items: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM standard_text_unit \
             WHERE owner = 'builtin' \
               AND unit_role = '{\"f\":\"name\"}' \
               AND source_content_json = ?1",
            [json!(MANUAL_STANDARD_ITEM_SOURCE).to_string()],
            |row| row.get(0),
        )
        .expect("人工补译同源 Items 单元应可读取");
    assert_eq!(duplicate_items, 2, "两个相同描述必须物化为两个物理单元");

    let dialogue: String = connection
        .query_row(
            "SELECT source_content_json FROM standard_text_unit \
             WHERE owner = 'builtin' AND unit_role = '\"b\"'",
            [],
            |row| row.get(0),
        )
        .expect("人工补译 Lines 单元应可读取");
    assert_eq!(
        serde_json::from_str::<Value>(&dialogue).expect("对话正文应为规范 JSON"),
        json!(MANUAL_STANDARD_DIALOGUE_SOURCE)
    );
}

fn assert_manual_standard_candidates_committed(database: &Path) {
    let connection = open_read_only(database);
    let pairing_anomalies: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM standard_text_unit \
             WHERE (translation_content_json IS NULL) <> (translation_state IS NULL)",
            [],
            |row| row.get(0),
        )
        .expect("translation/state 配对异常应可查询");
    assert_eq!(pairing_anomalies, 0, "translation/state 必须始终成对");

    let mut item_statement = connection
        .prepare(
            "SELECT translation_content_json, translation_state \
             FROM standard_text_unit \
             WHERE owner = 'builtin' \
               AND unit_role = '{\"f\":\"name\"}' \
               AND source_content_json = ?1 \
             ORDER BY group_location",
        )
        .expect("人工补译 Items 译文查询应可准备");
    let item_rows = item_statement
        .query_map([json!(MANUAL_STANDARD_ITEM_SOURCE).to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .expect("人工补译 Items 译文查询应可执行")
        .collect::<Result<Vec<_>, _>>()
        .expect("人工补译 Items 译文应可读取");
    assert_eq!(item_rows.len(), 2, "一次验收必须传播到完整标量族");
    for (translation, state) in &item_rows {
        assert_eq!(
            serde_json::from_str::<Value>(translation).expect("Items 译文应为规范 JSON"),
            json!(MANUAL_STANDARD_ITEM_TRANSLATION)
        );
        assert_eq!(state.len(), 32, "Standard state 必须是 SHA-256");
    }
    assert_ne!(
        item_rows[0].1, item_rows[1].1,
        "同族传播位置必须按各自完整身份生成独立 state"
    );

    let (dialogue_translation, dialogue_state): (String, Vec<u8>) = connection
        .query_row(
            "SELECT translation_content_json, translation_state \
             FROM standard_text_unit \
             WHERE owner = 'builtin' AND unit_role = '\"b\"'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("人工补译 Lines 译文应可读取");
    assert_eq!(
        serde_json::from_str::<Value>(&dialogue_translation).expect("Lines 译文应为规范 JSON"),
        json!([MANUAL_STANDARD_DIALOGUE_TRANSLATION])
    );
    assert_eq!(dialogue_state.len(), 32);
}

fn assert_mv_dialogue_extracted(database: &Path) {
    let connection = open_read_only(database);
    let mut statement = connection
        .prepare(
            "SELECT unit_role, source_content_json, translation_content_json \
             FROM standard_text_unit WHERE owner = 'builtin' ORDER BY unit_role",
        )
        .expect("MV 语义单元查询应可准备");
    let units = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .expect("MV 语义单元查询应可执行")
        .collect::<Result<Vec<_>, _>>()
        .expect("MV 语义单元应可读取");
    assert_eq!(units.len(), 2, "MV 姓名和正文应物化为两个语义单元");

    let logical = units
        .iter()
        .map(|(role, source, translation)| {
            (
                serde_json::from_str::<Value>(role).expect("MV 逻辑角色应为规范 JSON"),
                serde_json::from_str::<Value>(source).expect("MV 源内容应为规范 JSON"),
                translation.as_deref().map(|value| {
                    serde_json::from_str::<Value>(value).expect("MV 译文应为规范 JSON")
                }),
            )
        })
        .collect::<Vec<_>>();
    assert!(logical.contains(&(json!("p"), json!(MV_SPEAKER), None,)));
    assert!(logical.contains(&(json!("b"), json!([MV_BODY]), None,)));

    let (groups, claims): (i64, i64) = connection
        .query_row(
            "SELECT (SELECT COUNT(*) FROM standard_text_group WHERE owner = 'builtin'), \
                    (SELECT COUNT(*) FROM standard_mutation_claim WHERE owner = 'builtin')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("MV 对话组与修改目标数量应可读取");
    assert_eq!(groups, 1, "MV 对话应物化为唯一标准组");
    assert!(claims > 0, "对话块和 401 行应由展开后的 Claim 资源锁保护");

    let definition: String = connection
        .query_row(
            "SELECT canonical_json FROM standard_project_definition \
             WHERE definition_kind = 'mv_dialogue_rules'",
            [],
            |row| row.get(0),
        )
        .expect("MV 活动对话定义应与 Builtin 快照一同保存");
    let definition: Value = serde_json::from_str(&definition).expect("MV 对话定义应为规范 JSON");
    assert_eq!(definition["rules"].as_array().map(Vec::len), Some(1));
}

fn assert_mv_dialogue_translated(database: &Path) {
    let connection = open_read_only(database);
    for (source, expected) in [
        (json!(MV_SPEAKER), json!(MV_SPEAKER_TRANSLATION)),
        (json!([MV_BODY]), json!([MV_BODY_TRANSLATION])),
    ] {
        let translation_json: String = connection
            .query_row(
                "SELECT translation_content_json FROM standard_text_unit \
                 WHERE owner = 'builtin' AND source_content_json = ?1",
                [source.to_string()],
                |row| row.get(0),
            )
            .unwrap_or_else(|error| panic!("MV 语义单元 {source:?} 译文应已提交：{error}"));
        assert_eq!(
            serde_json::from_str::<Value>(&translation_json).expect("MV 译文应为规范 JSON"),
            expected
        );
    }
}

fn assert_mv_dialogue_written(output_root: &Path) {
    let output_map: Value = serde_json::from_slice(
        &fs::read(output_root.join("www/data/Map001.json")).expect("MV 写回 Map 应存在"),
    )
    .expect("MV 写回 Map 应为 JSON");
    let commands = output_map["events"][1]["pages"][0]["list"]
        .as_array()
        .expect("MV 写回事件命令应为数组");
    assert_eq!(commands[0]["code"], 101);
    assert_eq!(commands[0]["parameters"], json!(["", 0, 0, 2]));
    assert_eq!(commands[1]["code"], 401);
    assert_eq!(
        commands[1]["parameters"][0],
        format!(r"\n<{MV_SPEAKER_TRANSLATION}>{MV_BODY_TRANSLATION}")
    );
    assert_eq!(commands[2]["code"], 0);
    assert_eq!(
        fs::read_to_string(output_root.join("www/js/rpg_core.js")).expect("MV core 应随写回树保留"),
        "/* MV core */"
    );
}

fn assert_mixed_semantic_game_written(output_root: &Path) {
    let output_map: Value = serde_json::from_slice(
        &fs::read(output_root.join("data/Map001.json")).expect("混合 Map 写回文件应存在"),
    )
    .expect("混合 Map 写回文件应为 JSON");
    assert_eq!(output_map["displayName"], MIXED_MAP_NAME_TRANSLATION);
    let commands = output_map["events"][1]["pages"][0]["list"]
        .as_array()
        .expect("混合 Map 事件命令应为数组");
    assert_eq!(
        commands
            .iter()
            .map(|command| command["code"].as_i64().expect("事件 code 应为整数"))
            .collect::<Vec<_>>(),
        vec![
            101, 401, 401, 102, 402, 0, 402, 0, 404, 105, 405, 405, 405, 0
        ],
        "三条正文 401 必须由模型的两条语义行整体替换"
    );
    assert_eq!(
        commands[0]["parameters"],
        json!(["", 0, 0, 2, MIXED_SPEAKER_TRANSLATION])
    );
    assert_eq!(
        commands[1]["parameters"],
        json!([MIXED_DIALOGUE_TRANSLATION[0]])
    );
    assert_eq!(
        commands[2]["parameters"],
        json!([MIXED_DIALOGUE_TRANSLATION[1]])
    );
    assert_eq!(
        commands[3]["parameters"][0],
        json!(MIXED_CHOICES_TRANSLATION)
    );
    assert_eq!(
        commands[4]["parameters"],
        json!([0, MIXED_CHOICES_TRANSLATION[0]])
    );
    assert_eq!(
        commands[6]["parameters"],
        json!([1, MIXED_CHOICES_TRANSLATION[1]])
    );
    assert_eq!(
        commands[10]["parameters"],
        json!([MIXED_SCROLLING_TRANSLATION[0]])
    );
    assert_eq!(
        commands[11]["parameters"],
        json!([MIXED_SCROLLING_TRANSLATION[1]])
    );
    assert_eq!(
        commands[12]["parameters"],
        json!([MIXED_SCROLLING_TRANSLATION[2]])
    );
}

fn assert_manual_standard_game_written(output_root: &Path) {
    let items: Value = serde_json::from_slice(
        &fs::read(output_root.join("data/Items.json")).expect("人工补译 Items 写回文件应存在"),
    )
    .expect("人工补译 Items 写回文件应为 JSON");
    assert_eq!(items[1]["name"], MANUAL_STANDARD_ITEM_TRANSLATION);
    assert_eq!(items[2]["name"], MANUAL_STANDARD_ITEM_TRANSLATION);

    let map: Value = serde_json::from_slice(
        &fs::read(output_root.join("data/Map001.json")).expect("人工补译 Map 写回文件应存在"),
    )
    .expect("人工补译 Map 写回文件应为 JSON");
    let commands = map["events"][1]["pages"][0]["list"]
        .as_array()
        .expect("人工补译 Map 事件命令应为数组");
    assert_eq!(
        commands
            .iter()
            .map(|command| command["code"].as_i64().expect("事件 code 应为整数"))
            .collect::<Vec<_>>(),
        vec![101, 401, 0],
        "两条原文正文必须按人工 Lines 候选整体重排为一条 401"
    );
    assert_eq!(
        commands[1]["parameters"],
        json!([MANUAL_STANDARD_DIALOGUE_TRANSLATION])
    );
}

fn assert_mixed_semantic_project_log(log_root: &Path) {
    let (_, records) = read_project_logs(log_root);
    let task_records = records
        .iter()
        .filter(|record| {
            record["project"] == MIXED_PROJECT
                && matches!(
                    record["code"].as_str(),
                    Some("task.started" | "task.finished")
                )
        })
        .collect::<Vec<_>>();
    assert!(
        task_records.iter().all(|record| record["level"] == "debug"),
        "逐任务事实应以紧凑 Debug 事件完整持久化"
    );
    assert!(
        task_records
            .iter()
            .any(|record| record["code"] == "task.started")
            && task_records
                .iter()
                .any(|record| record["code"] == "task.finished"),
        "逐任务开始和终态都不得被固定级别过滤"
    );
    let write_back = records
        .iter()
        .find(|record| {
            record["project"] == MIXED_PROJECT
                && record["command"] == "write-back"
                && record["code"] == "publication.finished"
                && record["payload"]["kind"] == "publication"
                && record["payload"]["outcome"] == "published"
        })
        .expect("混合 Map 应记录普通项目日志中的写回发布终态");
    assert!(write_back["payload"]["published_items"].is_null());
}

fn assert_translation_committed(database: &Path) {
    let connection = open_read_only(database);
    let translation_json: String = connection
        .query_row(
            "SELECT translation_content_json FROM standard_text_unit WHERE source_content_json = ?1",
            [json!(SOURCE_TEXT).to_string()],
            |row| row.get(0),
        )
        .expect("真实模型结果应提交到项目数据库");
    assert_eq!(
        serde_json::from_str::<Value>(&translation_json).expect("译文内容应为规范 JSON"),
        json!(TRANSLATION)
    );
}

fn assert_translation_absent(database: &Path) {
    let connection = open_read_only(database);
    let translation_json: Option<String> = connection
        .query_row(
            "SELECT translation_content_json FROM standard_text_unit WHERE source_content_json = ?1",
            [json!(SOURCE_TEXT).to_string()],
            |row| row.get(0),
        )
        .expect("取消后仍应保留原资产行");
    assert!(translation_json.is_none(), "取消运行不得提交空数组响应");
}

fn read_translation_unit(database: &Path) -> (String, String, Vec<u8>) {
    open_read_only(database)
        .query_row(
            "SELECT source_content_json, translation_content_json, translation_state \
             FROM standard_text_unit WHERE owner = 'builtin'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("Builtin 已翻译语义单元及其 state 应可读取")
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
    let (actual_source_json, actual_translation_json): (String, Option<String>) =
        open_read_only(database)
            .query_row(
                "SELECT source_content_json, translation_content_json \
             FROM standard_text_unit WHERE owner = 'builtin'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("Builtin 当前语义单元应可读取");
    assert_eq!(
        serde_json::from_str::<Value>(&actual_source_json).expect("源内容应为规范 JSON"),
        json!(original)
    );
    assert_eq!(
        actual_translation_json
            .as_deref()
            .map(|value| serde_json::from_str::<Value>(value).expect("译文应为规范 JSON")),
        expected.map(|value| json!(value))
    );
}

fn assert_rules_unit(database: &Path, field_name: &str, source_text: &str) {
    let connection = open_read_only(database);
    let row: (String, String, Option<String>, Option<Vec<u8>>) = connection
        .query_row(
            "SELECT unit_role, source_content_json, translation_content_json, translation_state \
             FROM standard_text_unit WHERE owner = 'rules'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("Rules 当前唯一语义单元应可读取");
    assert_eq!(
        serde_json::from_str::<Value>(&row.0).expect("Rules 逻辑角色应为规范 JSON"),
        json!({ "f": format!(r#"["{field_name}"].text[0]"#) })
    );
    assert_eq!(
        serde_json::from_str::<Value>(&row.1).expect("Rules 源内容应为规范 JSON"),
        json!(source_text)
    );
    assert_eq!(row.2, None);
    assert_eq!(row.3, None);
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM standard_text_unit WHERE owner = 'rules'",
            [],
            |row| row.get(0),
        )
        .expect("Rules 单元数量应可读取");
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

fn assert_project_log(log_root: &Path) {
    let (raw, records) = read_project_logs(log_root);
    assert!(
        records.len() >= 20,
        "跨进程主流程应形成普通项目日志生命周期记录"
    );

    let expected_record_keys = BTreeSet::from([
        "time", "level", "code", "pid", "run_id", "sequence", "engine", "project", "command",
        "profile", "locale", "message", "payload",
    ]);
    let mut last_sequence_by_run = BTreeMap::<String, u64>::new();
    for record in &records {
        let object = record.as_object().expect("项目日志记录必须是 JSON object");
        let actual_keys = object.keys().map(String::as_str).collect::<BTreeSet<_>>();
        assert_eq!(
            actual_keys, expected_record_keys,
            "项目日志顶层字段必须稳定"
        );

        let time = record["time"].as_str().expect("time 应为字符串");
        assert!(
            time.ends_with('Z') && time.contains('T'),
            "time 应为 UTC 时间：{time}"
        );
        assert!(matches!(
            record["level"].as_str(),
            Some("error" | "warn" | "info" | "debug")
        ));
        assert!(record["pid"].as_u64().is_some_and(|pid| pid > 0));
        let run_id = record["run_id"].as_str().expect("生产命令应建立 run_id");
        assert_uuid_v4(run_id);
        let sequence = record["sequence"].as_u64().expect("sequence 应为非负整数");
        let previous = last_sequence_by_run.insert(run_id.to_owned(), sequence);
        assert_eq!(sequence, previous.map_or(1, |value| value + 1));
        assert_eq!(record["engine"], "mz");
        assert_eq!(record["project"], PROJECT);
        assert!(matches!(
            record["command"].as_str(),
            Some("init" | "extract" | "translate" | "write-back")
        ));
        assert!(record["profile"].is_null() || record["profile"].as_str() == Some(PROFILE));
        assert_eq!(record["locale"], "zh-Hans");
        assert!(
            record["message"]
                .as_str()
                .is_some_and(|message| !message.trim().is_empty())
        );
        assert_typed_project_log_payload(record);
    }

    assert!(
        records
            .iter()
            .any(|record| record["code"] == "task.started")
            && records
                .iter()
                .any(|record| record["code"] == "task.finished"),
        "固定项目日志不得过滤逐任务 Debug 事实"
    );
    let translate_plan = records
        .iter()
        .find(|record| {
            record["command"] == "translate"
                && record["code"] == "run_plan.resolved"
                && record["payload"]["source"] == "explicit"
                && record["payload"]["lua_enabled"] == true
        })
        .expect("Translate 应记录不含模型正文的类型化方案来源");
    assert_eq!(translate_plan["profile"], PROFILE);
    assert_eq!(translate_plan["payload"]["selections"], json!([PROFILE]));
    assert_eq!(translate_plan["payload"]["lua_source"], "explicit");
    assert!(records.iter().any(|record| {
        record["run_id"] == translate_plan["run_id"]
            && record["code"] == "run.finished"
            && record["payload"]["outcome"] == "succeeded"
    }));

    let publication = records
        .iter()
        .find(|record| {
            record["command"] == "write-back"
                && record["code"] == "publication.finished"
                && record["payload"]["outcome"] == "published"
        })
        .expect("WriteBack 应记录发布终态");
    assert!(publication["profile"].is_null());
    assert!(records.iter().any(|record| {
        record["run_id"] == publication["run_id"]
            && record["code"] == "publication.started"
            && record["payload"]["kind"] == "publication"
    }));

    for omitted_payload in [
        SOURCE_TEXT,
        UPDATED_SOURCE_TEXT,
        TRANSLATION,
        SYSTEM_PROMPT,
        UPDATED_SYSTEM_PROMPT,
        EXPECTED_USER_MESSAGE,
        "LUA SYSTEM",
        "LUA USER",
        "messages",
        "authorization",
        API_KEY,
        E2E_PARAMETER_MARKER,
    ] {
        assert!(
            !raw.contains(omitted_payload),
            "摘要型项目日志不应复制完整载荷 {omitted_payload:?}"
        );
    }
}

fn assert_typed_project_log_payload(record: &Value) {
    let code = record["code"].as_str().expect("code 应为字符串");
    let payload = record["payload"]
        .as_object()
        .expect("payload 应为类型化 JSON object");
    let kind = payload["kind"].as_str().expect("payload.kind 应为字符串");
    let expected_kind = match code {
        "run.started" | "run.finished" => "run",
        "performance.counters" => "performance",
        "failure.reported" => "failure",
        "run.cancel_requested" | "run.safe_stop_finished" => "cancellation",
        "run_plan.resolved" => "run_plan",
        "run_plan.saved"
        | "run_plan.save_failed"
        | "run_plan.save_outcome_unknown"
        | "run_plan.saved_finalization_failed" => "none",
        "phase.started" | "phase.finished" => "phase",
        "retry.summary" => "retry_summary",
        "work.none" => "no_work",
        "result.partial" => "result_summary",
        "publication.started" | "publication.finished" => "publication",
        "task.started" | "task.finished" => "task",
        "task.diagnostic" => "task_diagnostic",
        other => panic!("未知项目日志 code：{other}"),
    };
    assert_eq!(
        kind, expected_kind,
        "code 必须与 typed payload 保持稳定映射"
    );

    let expected_keys = match kind {
        "none" => &["kind"][..],
        "run" => &["kind", "outcome"],
        "run_plan" => &["kind", "source", "lua_source", "selections", "lua_enabled"],
        "phase" => &["kind", "phase", "amount"],
        "retry_summary" => &["kind", "attempted", "recovered", "exhausted"],
        "no_work" => &["kind", "reason_code"],
        "result_summary" => &[
            "kind",
            "complete",
            "partial",
            "unavailable",
            "manual_review",
        ],
        "publication" => &["kind", "outcome", "published_items"],
        "task" => &["kind", "ordinal", "total", "outcome", "attempts"],
        "task_diagnostic" => &["kind", "ordinal", "total", "attempts", "diagnostic"],
        "cancellation" => &["kind", "confirmed", "total"],
        "failure" => &["kind", "relation", "diagnostic"],
        "performance" => &["kind", "snapshot"],
        _ => unreachable!("上方已确认 payload kind"),
    };
    let actual_keys = payload.keys().map(String::as_str).collect::<BTreeSet<_>>();
    assert_eq!(
        actual_keys,
        expected_keys.iter().copied().collect::<BTreeSet<_>>(),
        "typed payload 不得退化为任意 wire"
    );

    if kind == "performance" {
        assert_performance_snapshot(&payload["snapshot"]);
    }
}

fn assert_performance_snapshot(snapshot: &Value) {
    let snapshot = snapshot
        .as_object()
        .expect("performance snapshot 应为类型化 JSON object");
    assert_eq!(
        snapshot.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        ["sqlite_transactions", "candidate_validations"]
            .into_iter()
            .collect(),
        "performance snapshot 不得增加任意字段"
    );

    let sqlite = snapshot["sqlite_transactions"]
        .as_object()
        .expect("SQLite 事务计数应为类型化 JSON object");
    assert_eq!(
        sqlite.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        [
            "read_snapshot",
            "write_plan",
            "database_initialization",
            "interactive",
        ]
        .into_iter()
        .collect(),
        "SQLite 计数必须覆盖全部闭集职责范围"
    );
    for scope_name in [
        "read_snapshot",
        "write_plan",
        "database_initialization",
        "interactive",
    ] {
        let scope = sqlite[scope_name]
            .as_object()
            .unwrap_or_else(|| panic!("SQLite 计数范围 {scope_name} 应为 object"));
        assert_eq!(
            scope.keys().map(String::as_str).collect::<BTreeSet<_>>(),
            ["begin", "commit", "rollback"].into_iter().collect(),
            "SQLite 计数范围 {scope_name} 必须保留三类控制语句"
        );
        for control_name in ["begin", "commit", "rollback"] {
            let control = scope[control_name]
                .as_object()
                .unwrap_or_else(|| panic!("SQLite {scope_name}.{control_name} 计数应为 object"));
            assert_eq!(
                control.keys().map(String::as_str).collect::<BTreeSet<_>>(),
                ["attempted", "succeeded"].into_iter().collect(),
                "SQLite {scope_name}.{control_name} 必须保留尝试与成功计数"
            );
            assert!(control["attempted"].is_u64());
            assert!(control["succeeded"].is_u64());
        }
    }

    let candidate = snapshot["candidate_validations"]
        .as_object()
        .expect("candidate 校验计数应为类型化 JSON object");
    assert_eq!(
        candidate
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        ["started", "completed"].into_iter().collect(),
        "candidate 校验必须保留开始与完成计数"
    );
    assert!(candidate["started"].is_u64());
    assert!(candidate["completed"].is_u64());
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

fn read_project_logs(log_root: &Path) -> (String, Vec<Value>) {
    let mut paths = fs::read_dir(log_root)
        .unwrap_or_else(|error| panic!("应可列举项目日志目录 {}：{error}", log_root.display()))
        .collect::<Result<Vec<_>, _>>()
        .expect("项目日志目录条目应可读取")
        .into_iter()
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    paths.sort();
    assert!(!paths.is_empty(), "每次命令都应建立独立 RunId 日志文件");

    let mut raw = String::new();
    let mut records = Vec::new();
    for path in paths {
        assert_eq!(
            path.extension(),
            Some(OsStr::new("jsonl")),
            "项目日志目录只能包含 RunId JSONL：{}",
            path.display()
        );
        let run_id = path
            .file_stem()
            .and_then(OsStr::to_str)
            .expect("日志文件名必须是 UTF-8 RunId");
        assert_uuid_v4(run_id);
        let (file_raw, file_records) = read_json_lines(&path);
        assert!(
            file_records.iter().all(|record| record["run_id"] == run_id),
            "文件名 RunId 必须与每条记录一致：{}",
            path.display()
        );
        raw.push_str(&file_raw);
        records.extend(file_records);
    }
    (raw, records)
}

fn explicit_lua_run_id<'a>(records: &'a [Value], command: &str) -> &'a str {
    records
        .iter()
        .find(|record| {
            record["command"] == command
                && record["code"] == "run_plan.resolved"
                && record["payload"]["source"] == "explicit"
                && record["payload"]["lua_enabled"] == true
        })
        .and_then(|record| record["run_id"].as_str())
        .unwrap_or_else(|| panic!("{command} 显式 Lua 运行必须记录 RunId"))
}

fn assert_cancelled_project_log(log_root: &Path, command: &str) {
    let (_, records) = read_project_logs(log_root);
    let run_id = explicit_lua_run_id(&records, command);
    for code in ["run.cancel_requested", "run.safe_stop_finished"] {
        assert!(
            records
                .iter()
                .any(|record| record["run_id"] == run_id && record["code"] == code),
            "{command} 真正取消必须记录 {code}"
        );
    }
    assert!(records.iter().any(|record| {
        record["run_id"] == run_id
            && record["code"] == "run.finished"
            && record["payload"]["outcome"] == "cancelled"
    }));
    assert!(
        !records.iter().any(|record| {
            record["run_id"] == run_id
                && matches!(
                    record["code"].as_str(),
                    Some(
                        "run_plan.saved"
                            | "run_plan.save_failed"
                            | "run_plan.save_outcome_unknown"
                            | "run_plan.saved_finalization_failed"
                    )
                )
        }),
        "{command} 真正取消不得进入运行方案最终化"
    );
}

fn assert_completed_signal_project_log(log_root: &Path) {
    let (_, records) = read_project_logs(log_root);
    let run_id = explicit_lua_run_id(&records, "extract");
    assert!(
        records.iter().any(|record| {
            record["run_id"] == run_id && record["code"] == "run.cancel_requested"
        })
    );
    assert!(
        records.iter().any(|record| {
            record["run_id"] == run_id && record["code"] == "run_plan.save_failed"
        })
    );
    assert!(
        !records
            .iter()
            .any(|record| { record["run_id"] == run_id && record["code"] == "run_plan.saved" }),
        "写锁等待被信号取消时不得谎报运行方案已保存"
    );
    assert!(records.iter().any(|record| {
        record["run_id"] == run_id
            && record["code"] == "run.finished"
            && record["payload"]["outcome"] == "succeeded"
    }));
    assert!(
        !records.iter().any(|record| {
            record["run_id"] == run_id
                && record["code"] == "run.finished"
                && record["payload"]["outcome"] == "cancelled"
        }),
        "业务自然完成后不得保留过时的取消终态"
    );
}

fn assert_project_logs_do_not_contain(log_root: &Path, needle: &str, label: &str) {
    let (raw, _) = read_project_logs(log_root);
    assert!(!raw.contains(needle), "{label}不得包含 {needle:?}");
}

fn assert_last_write_back_log(log_root: &Path, lua_executed: bool) {
    // Init 更新重建保留既有 logs/,项目日志跨多次运行累积;文件名是无序 UUID,
    // “最后一次”按记录时间戳选取。
    let (_, records) = read_project_logs(log_root);
    let plan = records
        .iter()
        .filter(|record| {
            record["command"] == "write-back"
                && record["code"] == "run_plan.resolved"
                && record["payload"]["kind"] == "run_plan"
                && record["payload"]["lua_enabled"] == lua_executed
        })
        .max_by_key(|record| record["time"].as_str().map(str::to_owned))
        .expect("WriteBack 发布运行应记录准确的 Lua 方案来源");
    let run_id = &plan["run_id"];
    let publication = records
        .iter()
        .find(|record| {
            record["run_id"] == *run_id
                && record["code"] == "publication.finished"
                && record["payload"]["kind"] == "publication"
                && record["payload"]["outcome"] == "published"
        })
        .expect("同一 WriteBack RunId 应记录已确认发布终态");
    assert_eq!(publication["payload"]["published_items"], 1);
}

fn assert_translate_mixed_source_log(log_root: &Path) {
    let (_, records) = read_project_logs(log_root);
    assert!(
        records.iter().any(|record| {
            record["command"] == "translate"
                && record["code"] == "run_plan.resolved"
                && record["payload"]["source"] == "explicit"
                && record["payload"]["lua_source"] == "project_state"
        }),
        "Translate 混合来源必须分别记录 Profile 与 Lua 来源"
    );
}

fn assert_standard_task_record_shares_translate_run_id(workspace: &Path, log_root: &Path) {
    let (_, records) = read_project_logs(log_root);
    let run_id = records
        .iter()
        .find(|record| {
            record["command"] == "translate"
                && record["code"] == "run_plan.resolved"
                && record["payload"]["lua_source"] == "explicit"
                && record["payload"]["lua_enabled"] == true
        })
        .and_then(|record| record["run_id"].as_str())
        .expect("显式 Standard+Lua Translate 应拥有 RunId");
    let task_directory = workspace.join("task-records").join(run_id);
    let mut task_files = fs::read_dir(&task_directory)
        .expect("任务记录应使用同一 Translate RunId")
        .collect::<Result<Vec<_>, _>>()
        .expect("任务记录目录应可读取")
        .into_iter()
        .map(|entry| entry.file_name())
        .collect::<Vec<_>>();
    task_files.sort();
    assert_eq!(
        task_files,
        [OsString::from("task-000001.md")],
        "Standard+Lua Translate 只能为一个 Standard TaskBlock 生成一份任务记录"
    );
    let markdown = fs::read_to_string(task_directory.join("task-000001.md"))
        .expect("Standard 任务记录应可读取");
    assert!(markdown.starts_with("# 翻译任务 000001 · 完成\n"));
    assert!(markdown.contains("- Endpoint：`"));
    assert!(markdown.contains("- Model：`e2e-model`"));
    assert!(markdown.contains("## 自定义参数\n"));
    assert!(markdown.contains("## System\n"));
    assert!(markdown.contains("## User\n"));
    assert!(markdown.contains("## 请求过程\n"));
    assert!(markdown.contains("## Assistant\n"));
    assert!(markdown.contains("### ID 1\n"));
    assert!(markdown.contains("## 最终结果\n"));
    assert!(markdown.contains("- 状态：完成，已确认提交"));
    assert!(!markdown.contains(API_KEY));
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

    fn start_for_requests(self, expected_requests: usize) -> RunningChatServer {
        self.start_with_responses(vec![ChatResponseFixture::Standard; expected_requests])
    }

    fn start_observing_requests(self) -> RunningChatServer {
        self.listener
            .set_nonblocking(true)
            .expect("本地监听器应可设为非阻塞");
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (result_sender, result_receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = (|| {
                let mut requests = Vec::new();
                while !worker_stop.load(Ordering::Acquire) {
                    match self.listener.accept() {
                        Ok((stream, _)) => requests.push(serve_chat_completion(
                            stream,
                            requests.len(),
                            ChatResponseFixture::Standard,
                        )?),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => {
                            return Err(format!("Prompt 零请求探针 accept 失败：{error}"));
                        }
                    }
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

    fn start_with_responses_and_observe(
        self,
        responses: Vec<ChatResponseFixture>,
        additional_response: ChatResponseFixture,
    ) -> RunningChatServer {
        self.listener
            .set_nonblocking(true)
            .expect("本地监听器应可设为非阻塞");
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (result_sender, result_receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = (|| {
                let mut requests = Vec::new();
                for response in responses {
                    let request = loop {
                        match self.listener.accept() {
                            Ok((stream, _)) => {
                                break serve_chat_completion(stream, requests.len(), response)?;
                            }
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                if worker_stop.load(Ordering::Acquire) {
                                    return Err(format!(
                                        "翻译进程只发出了 {} 个 Chat Completions 请求",
                                        requests.len()
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
                while !worker_stop.load(Ordering::Acquire) {
                    match self.listener.accept() {
                        Ok((stream, _)) => requests.push(serve_chat_completion(
                            stream,
                            requests.len(),
                            additional_response,
                        )?),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => {
                            return Err(format!("本地 LLM 观察 accept 失败：{error}"));
                        }
                    }
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
    ThinkingStandard,
    Managed,
    Lua,
    MvDialogue,
    MixedSemanticUnits,
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
                serde_json::to_string(&json!({ "1": [TRANSLATION] }))
                    .map_err(|error| error.to_string())?,
                11,
                3,
                14,
            ),
            ChatResponseFixture::ThinkingStandard => (
                "request-thinking-e2e".to_owned(),
                "response-thinking-e2e".to_owned(),
                format!(
                    "<why>{THINKING_SENTINEL}\n逐项分析说话人、语气、术语、ATT token 与行结构。</why>\n{}",
                    serde_json::to_string(&json!({ "1": [TRANSLATION] }))
                        .map_err(|error| error.to_string())?
                ),
                19,
                11,
                30,
            ),
            ChatResponseFixture::Managed => (
                "request-managed-e2e".to_owned(),
                "response-managed-e2e".to_owned(),
                serde_json::to_string(&json!({ "1": [MANAGED_TRANSLATION] }))
                    .map_err(|error| error.to_string())?,
                13,
                4,
                17,
            ),
            ChatResponseFixture::Lua => (
                "request-lua".to_owned(),
                "response-lua".to_owned(),
                "lua-response-content".to_owned(),
                17,
                5,
                22,
            ),
            ChatResponseFixture::MvDialogue => (
                "request-mv-dialogue".to_owned(),
                "response-mv-dialogue".to_owned(),
                serde_json::to_string(&json!({
                    "1": [MV_SPEAKER_TRANSLATION],
                    "2": [MV_BODY_TRANSLATION]
                }))
                .map_err(|error| error.to_string())?,
                13,
                7,
                20,
            ),
            ChatResponseFixture::MixedSemanticUnits => (
                "request-mixed-semantic-units".to_owned(),
                "response-mixed-semantic-units".to_owned(),
                serde_json::to_string(&json!({
                    "1": [MIXED_MAP_NAME_TRANSLATION],
                    "2": [MIXED_SPEAKER_TRANSLATION],
                    "3": MIXED_DIALOGUE_TRANSLATION,
                    "4": MIXED_CHOICES_TRANSLATION,
                    "5": MIXED_SCROLLING_TRANSLATION,
                }))
                .map_err(|error| error.to_string())?,
                29,
                16,
                45,
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
            "diagnostic_marker": E2E_PARAMETER_MARKER
        }
    });
    assert_eq!(actual, expected, "Chat Completions 请求 wire 必须精确匹配");
}

fn assert_mixed_semantic_request(request: &CapturedRequest) {
    assert_common_chat_request_headers(request);
    let actual: Value = serde_json::from_slice(&request.body).expect("混合 Map 请求必须是 JSON");
    assert_eq!(actual["model"], "e2e-model");
    assert_eq!(actual["stream"], false);
    let messages = actual["messages"].as_array().expect("messages 必须是数组");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["content"], SYSTEM_PROMPT);
    let user = messages[1]["content"]
        .as_str()
        .expect("混合 Map user message 必须是 Markdown 字符串");
    let expected_fragments = [
        format!("Map Name [1] (single line):{MIXED_MAP_NAME}"),
        format!("Speaker [2] (single line):{MIXED_SPEAKER}"),
        "Body [3] (free line breaking):".to_owned(),
        format!(
            "Choices [4] ({} items, corresponding item by item):",
            MIXED_CHOICES_SOURCE.len()
        ),
        format!(
            "Scrolling Text [5] ({} lines, corresponding line by line):",
            MIXED_SCROLLING_SOURCE.len()
        ),
    ];
    let mut previous = 0;
    for fragment in &expected_fragments {
        let position = user
            .find(fragment)
            .unwrap_or_else(|| panic!("混合 Map 消息缺少 {fragment:?}：{user}"));
        assert!(position >= previous, "五种类型必须保持自然顺序：{user}");
        previous = position;
    }
    for line in MIXED_DIALOGUE_SOURCE
        .iter()
        .chain(MIXED_CHOICES_SOURCE.iter())
        .chain(
            MIXED_SCROLLING_SOURCE
                .iter()
                .filter(|line| !line.is_empty()),
        )
    {
        assert!(
            user.contains(&format!("> {line}")),
            "复合语义单元必须以可读 Markdown 保留行边界：{user}"
        );
    }
    assert!(
        user.contains("> \n"),
        "滚动文本空槽必须保留在请求中：{user}"
    );
    for forbidden in [
        "source_language",
        "target_language",
        "owner",
        "group_location",
        "source_content_json",
        "groups",
    ] {
        assert!(
            !user.contains(forbidden),
            "最小 user message 不得携带内部字段 {forbidden:?}：{user}"
        );
    }
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
            "diagnostic_marker": E2E_PARAMETER_MARKER
        }
    });
    assert_eq!(
        actual, expected,
        "Translate Lua 必须复用同一公共 LLM 客户端"
    );
}

fn assert_mv_dialogue_request(request: &CapturedRequest) {
    assert_common_chat_request_headers(request);
    let actual: Value = serde_json::from_slice(&request.body).expect("MV LLM 请求必须是 JSON");
    assert_eq!(actual["model"], "e2e-model");
    assert_eq!(actual["stream"], false);
    let messages = actual["messages"].as_array().expect("messages 必须是数组");
    assert_eq!(messages[0]["content"], SYSTEM_PROMPT);
    let user = messages[1]["content"]
        .as_str()
        .expect("MV 翻译用户消息必须是字符串");
    assert!(
        user.contains(MV_SPEAKER),
        "MV 请求应包含 Speaker 单元：{user}"
    );
    assert!(user.contains(MV_BODY), "MV 请求应包含 Body 单元：{user}");
    assert!(
        user.find(MV_SPEAKER) < user.find(MV_BODY),
        "同一对话组应按 Speaker 后 Body 的稳定角色顺序请求"
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
