use std::error::Error;
use std::fmt;
use std::future::{Future, poll_fn};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use clap::CommandFactory;

use super::MzArguments;
use super::MzCli;
use super::ProjectName;
use super::extract::{ExtractInput, ExtractOutput, ExtractUseCase, ExtractionSelection};
use super::init::{InitInput, InitOutput, InitUseCase};
use super::project::{MaxFullwidthChars, MzWriteBackLayoutProfile};
use super::translate::{
    StandardTranslationSummary, TranslateInput, TranslateOutput, TranslateUseCase,
};
use super::write_back::{
    StandardWriteBackSummary, WriteBackInput, WriteBackOutput, WriteBackUseCase,
};

const LAYOUT_WIDTH_ARGS: [&str; 6] = [
    "--dialogue-max-fullwidth-chars",
    "24",
    "--scrolling-text-max-fullwidth-chars",
    "30",
    "--help-description-max-fullwidth-chars",
    "18",
];

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Calls {
    init: Vec<InitInput>,
    extract: Vec<ExtractInput>,
    translate: Vec<TranslateInput>,
    write_back: Vec<WriteBackInput>,
}

#[derive(Clone)]
struct FakeInit {
    calls: Arc<Mutex<Calls>>,
}

impl InitUseCase for FakeInit {
    type Error = FakeError;

    fn execute(
        &self,
        input: InitInput,
    ) -> impl Future<Output = Result<InitOutput, Self::Error>> + Send {
        let calls = Arc::clone(&self.calls);

        async move {
            yield_once().await;
            calls
                .lock()
                .expect("调用记录锁不应中毒")
                .init
                .push(input.clone());
            Ok(InitOutput { name: input.name })
        }
    }
}

#[derive(Clone)]
struct FakeExtract {
    calls: Arc<Mutex<Calls>>,
    failure: Option<&'static str>,
}

impl ExtractUseCase for FakeExtract {
    type Error = FakeError;

    fn execute(
        &self,
        input: ExtractInput,
    ) -> impl Future<Output = Result<ExtractOutput, Self::Error>> + Send {
        let calls = Arc::clone(&self.calls);
        let failure = self.failure;

        async move {
            yield_once().await;
            calls
                .lock()
                .expect("调用记录锁不应中毒")
                .extract
                .push(input.clone());

            if let Some(message) = failure {
                Err(FakeError(message))
            } else {
                Ok(ExtractOutput { name: input.name })
            }
        }
    }
}

#[derive(Clone)]
struct FakeTranslate {
    calls: Arc<Mutex<Calls>>,
}

impl TranslateUseCase for FakeTranslate {
    type Error = FakeError;

    fn execute(
        &self,
        input: TranslateInput,
    ) -> impl Future<Output = Result<TranslateOutput, Self::Error>> + Send {
        let calls = Arc::clone(&self.calls);

        async move {
            yield_once().await;
            let lua_executed = input.lua_script.is_some();
            calls
                .lock()
                .expect("调用记录锁不应中毒")
                .translate
                .push(input.clone());

            Ok(TranslateOutput {
                name: input.name,
                llm_id: input.llm_id,
                standard: translation_summary(),
                lua_executed,
            })
        }
    }
}

#[derive(Clone)]
struct FakeWriteBack {
    calls: Arc<Mutex<Calls>>,
    failure: Option<&'static str>,
}

impl WriteBackUseCase for FakeWriteBack {
    type Error = FakeError;

    fn execute(
        &self,
        input: WriteBackInput,
    ) -> impl Future<Output = Result<WriteBackOutput, Self::Error>> + Send {
        let calls = Arc::clone(&self.calls);
        let failure = self.failure;

        async move {
            yield_once().await;
            let output_root = PathBuf::from(".\\projects")
                .join(input.name.to_string())
                .join("write_back");
            let lua_executed = input.lua_script.is_some();
            calls
                .lock()
                .expect("调用记录锁不应中毒")
                .write_back
                .push(input.clone());
            if let Some(message) = failure {
                return Err(FakeError(message));
            }
            Ok(WriteBackOutput {
                name: input.name,
                output_root,
                standard: write_back_summary(),
                lua_executed,
            })
        }
    }
}

#[derive(Debug)]
struct FakeError(&'static str);

impl fmt::Display for FakeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for FakeError {}

async fn yield_once() {
    let mut yielded = false;
    poll_fn(|context| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            context.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}

fn project_name(value: &str) -> ProjectName {
    value.parse().expect("测试项目名称应该合法")
}

fn translation_summary() -> StandardTranslationSummary {
    StandardTranslationSummary {
        total_tasks: 4,
        complete_tasks: 2,
        partial_tasks: 1,
        unavailable_tasks: 1,
        accepted_decisions: 5,
        written_locations: 7,
        remaining_decisions: 2,
        remaining_locations: 3,
        protocol_diagnostics: 4,
        recoverable_request_exhaustions: 1,
    }
}

fn layout_profile() -> MzWriteBackLayoutProfile {
    MzWriteBackLayoutProfile::new(
        MaxFullwidthChars::new(24).expect("测试对话宽度应该合法"),
        MaxFullwidthChars::new(30).expect("测试滚动文本宽度应该合法"),
        MaxFullwidthChars::new(18).expect("测试说明框宽度应该合法"),
    )
}

fn write_back_summary() -> StandardWriteBackSummary {
    StandardWriteBackSummary {
        translated_locations: 7,
        original_locations: 3,
        auto_wrapped_units: 2,
        inserted_line_breaks: 4,
        inserted_fullwidth_indents: 5,
        manual_layout_units: 1,
    }
}

fn selection(
    builtin: bool,
    rules_path: Option<&str>,
    lua_script: Option<&str>,
) -> ExtractionSelection {
    ExtractionSelection::new(
        builtin,
        rules_path.map(PathBuf::from),
        lua_script.map(PathBuf::from),
    )
    .expect("测试应至少选择一种提取方式")
}

fn cli(calls: Arc<Mutex<Calls>>) -> MzCli<FakeInit, FakeExtract, FakeTranslate, FakeWriteBack> {
    MzCli::new(
        FakeInit {
            calls: Arc::clone(&calls),
        },
        FakeExtract {
            calls: Arc::clone(&calls),
            failure: None,
        },
        FakeTranslate {
            calls: Arc::clone(&calls),
        },
        FakeWriteBack {
            calls,
            failure: None,
        },
    )
}

async fn run(
    cli: &MzCli<FakeInit, FakeExtract, FakeTranslate, FakeWriteBack>,
    args: &[&str],
) -> (ExitCode, String, String) {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let exit_code = cli
        .run_from(args.iter().copied(), &mut stdout, &mut stderr)
        .await
        .expect("内存 writer 不应写入失败");

    (
        exit_code,
        String::from_utf8(stdout).expect("CLI stdout 应为 UTF-8"),
        String::from_utf8(stderr).expect("CLI stderr 应为 UTF-8"),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn help_lists_only_confirmed_commands_without_calling_use_cases() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let cli = cli(Arc::clone(&calls));

    let (exit_code, stdout, stderr) = run(&cli, &["--help"]).await;

    assert_eq!(exit_code, ExitCode::SUCCESS);
    assert!(stdout.contains("init"));
    assert!(stdout.contains("extract"));
    assert!(stdout.contains("translate"));
    assert!(stdout.contains("write-back"));
    assert!(!stdout.contains("status"));
    assert!(stderr.is_empty());
    assert_eq!(*calls.lock().expect("调用记录锁不应中毒"), Calls::default());
}

#[test]
fn clap_schema_is_valid() {
    MzArguments::command().debug_assert();
}

#[tokio::test(flavor = "current_thread")]
async fn extract_help_lists_all_combinable_task_options_without_calling_use_case() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let cli = cli(Arc::clone(&calls));

    let (exit_code, stdout, stderr) = run(&cli, &["extract", "--help"]).await;

    assert_eq!(exit_code, ExitCode::SUCCESS);
    assert!(stdout.contains("--builtin"));
    assert!(stdout.contains("--rules <RULES_JSON>"));
    assert!(stdout.contains("--lua <SCRIPT_LUA>"));
    assert!(stderr.is_empty());
    assert_eq!(*calls.lock().expect("调用记录锁不应中毒"), Calls::default());
}

#[tokio::test(flavor = "current_thread")]
async fn init_help_lists_all_required_layout_widths_without_calling_use_case() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let cli = cli(Arc::clone(&calls));

    let (exit_code, stdout, stderr) = run(&cli, &["init", "--help"]).await;

    assert_eq!(exit_code, ExitCode::SUCCESS);
    assert!(stdout.contains("--dialogue-max-fullwidth-chars <COUNT>"));
    assert!(stdout.contains("--scrolling-text-max-fullwidth-chars <COUNT>"));
    assert!(stdout.contains("--help-description-max-fullwidth-chars <COUNT>"));
    assert!(stderr.is_empty());
    assert_eq!(*calls.lock().expect("调用记录锁不应中毒"), Calls::default());
}

#[tokio::test(flavor = "current_thread")]
async fn translate_help_lists_optional_input_files_without_calling_use_case() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let cli = cli(Arc::clone(&calls));

    let (exit_code, stdout, stderr) = run(&cli, &["translate", "--help"]).await;

    assert_eq!(exit_code, ExitCode::SUCCESS);
    assert!(stdout.contains("--terms <TERMS_JSON>"));
    assert!(stdout.contains("--placeholders <PLACEHOLDERS_JSON>"));
    assert!(stdout.contains("--lua <SCRIPT_LUA>"));
    assert!(stderr.is_empty());
    assert_eq!(*calls.lock().expect("调用记录锁不应中毒"), Calls::default());
}

#[tokio::test(flavor = "current_thread")]
async fn init_dispatches_all_confirmed_inputs_to_init_only() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let cli = cli(Arc::clone(&calls));

    let (exit_code, stdout, stderr) = run(
        &cli,
        &[
            "init",
            "--name",
            "游戏 一",
            "--path",
            ".\\Game One",
            "--source-language",
            "ja",
            "--target-language",
            "zh-Hans",
            "--dialogue-max-fullwidth-chars",
            "24",
            "--scrolling-text-max-fullwidth-chars",
            "30",
            "--help-description-max-fullwidth-chars",
            "18",
        ],
    )
    .await;

    assert_eq!(exit_code, ExitCode::SUCCESS);
    assert_eq!(stdout, "初始化完成：游戏 一\n");
    assert!(stderr.is_empty());
    assert_eq!(
        *calls.lock().expect("调用记录锁不应中毒"),
        Calls {
            init: vec![InitInput {
                name: project_name("游戏 一"),
                game_root: PathBuf::from(".\\Game One"),
                source_language: "ja".to_owned(),
                target_language: "zh-Hans".to_owned(),
                layout_profile: layout_profile(),
            }],
            ..Calls::default()
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn init_requires_positive_layout_widths_before_calling_use_case() {
    let base_args = [
        "init",
        "--name",
        "alice",
        "--path",
        ".\\Game",
        "--source-language",
        "ja",
        "--target-language",
        "zh-Hans",
    ];

    let calls = Arc::new(Mutex::new(Calls::default()));
    let command = cli(Arc::clone(&calls));
    let (exit_code, stdout, stderr) = run(&command, &base_args).await;
    assert_eq!(exit_code, ExitCode::from(2));
    assert!(stdout.is_empty());
    assert!(!stderr.is_empty());
    assert_eq!(*calls.lock().expect("调用记录锁不应中毒"), Calls::default());

    for (invalid_value_index, invalid_value) in [(1, "0"), (3, "0"), (5, "0"), (1, "not-a-number")]
    {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let cli = cli(Arc::clone(&calls));
        let mut args = base_args.to_vec();
        let mut layout_args = LAYOUT_WIDTH_ARGS;
        layout_args[invalid_value_index] = invalid_value;
        args.extend(layout_args);

        let (exit_code, stdout, stderr) = run(&cli, &args).await;

        assert_eq!(exit_code, ExitCode::from(2), "参数：{args:?}");
        assert!(stdout.is_empty(), "参数：{args:?}");
        assert!(!stderr.is_empty(), "参数：{args:?}");
        assert_eq!(
            *calls.lock().expect("调用记录锁不应中毒"),
            Calls::default(),
            "参数：{args:?}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn extract_dispatches_builtin_task_to_extract_only() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let cli = cli(Arc::clone(&calls));

    let (exit_code, stdout, stderr) = run(&cli, &["extract", "--name", "alice", "--builtin"]).await;

    assert_eq!(exit_code, ExitCode::SUCCESS);
    assert_eq!(stdout, "提取完成：alice\n");
    assert!(stderr.is_empty());
    assert_eq!(
        *calls.lock().expect("调用记录锁不应中毒"),
        Calls {
            extract: vec![ExtractInput {
                name: project_name("alice"),
                selection: selection(true, None, None),
            }],
            ..Calls::default()
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn extract_dispatches_rules_task_with_exact_path() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let cli = cli(Arc::clone(&calls));

    let (exit_code, stdout, stderr) = run(
        &cli,
        &[
            "extract",
            "--name",
            "alice",
            "--rules",
            ".\\rules custom.json",
        ],
    )
    .await;

    assert_eq!(exit_code, ExitCode::SUCCESS);
    assert_eq!(stdout, "提取完成：alice\n");
    assert!(stderr.is_empty());
    assert_eq!(
        calls.lock().expect("调用记录锁不应中毒").extract,
        vec![ExtractInput {
            name: project_name("alice"),
            selection: selection(false, Some(".\\rules custom.json"), None),
        }]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn extract_dispatches_lua_task_with_exact_path() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let cli = cli(Arc::clone(&calls));

    let (exit_code, stdout, stderr) = run(
        &cli,
        &[
            "extract",
            "--name",
            "alice",
            "--lua",
            "scripts\\extract custom.lua",
        ],
    )
    .await;

    assert_eq!(exit_code, ExitCode::SUCCESS);
    assert_eq!(stdout, "提取完成：alice\n");
    assert!(stderr.is_empty());
    assert_eq!(
        calls.lock().expect("调用记录锁不应中毒").extract,
        vec![ExtractInput {
            name: project_name("alice"),
            selection: selection(false, None, Some("scripts\\extract custom.lua")),
        }]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn extract_combines_all_selected_tasks_in_one_use_case_call() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let cli = cli(Arc::clone(&calls));

    let (exit_code, stdout, stderr) = run(
        &cli,
        &[
            "extract",
            "--name",
            "alice",
            "--lua",
            "scripts\\extract.lua",
            "--builtin",
            "--rules",
            ".\\rules.json",
        ],
    )
    .await;

    assert_eq!(exit_code, ExitCode::SUCCESS);
    assert_eq!(stdout, "提取完成：alice\n");
    assert!(stderr.is_empty());
    assert_eq!(
        calls.lock().expect("调用记录锁不应中毒").extract,
        vec![ExtractInput {
            name: project_name("alice"),
            selection: selection(true, Some(".\\rules.json"), Some("scripts\\extract.lua")),
        }]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn translate_dispatches_name_and_llm_id_to_async_use_case_only() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let cli = cli(Arc::clone(&calls));

    let (exit_code, stdout, stderr) =
        run(&cli, &["translate", "--name", "alice", "deepseek"]).await;

    assert_eq!(exit_code, ExitCode::SUCCESS);
    assert_eq!(
        stdout,
        concat!(
            "翻译执行完成：alice（LLM：deepseek）\n",
            "标准翻译：任务 4，完整 2，部分 1，不可用 1；写入 7 处，剩余 3 处\n",
        )
    );
    assert!(stderr.is_empty());
    assert_eq!(
        *calls.lock().expect("调用记录锁不应中毒"),
        Calls {
            translate: vec![TranslateInput {
                name: project_name("alice"),
                llm_id: "deepseek".to_owned(),
                terminology_path: None,
                placeholder_rules_path: None,
                lua_script: None,
            }],
            ..Calls::default()
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn translate_passes_all_optional_paths_exactly_and_ignores_argument_order() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let cli = cli(Arc::clone(&calls));

    let (first_exit_code, first_stdout, first_stderr) = run(
        &cli,
        &[
            "translate",
            "--name",
            "alice",
            "deepseek",
            "--placeholders",
            ".\\rules\\placeholders custom.json",
            "--lua",
            "scripts\\translate custom.lua",
            "--terms",
            ".\\glossaries\\terms custom.json",
        ],
    )
    .await;

    let (second_exit_code, second_stdout, second_stderr) = run(
        &cli,
        &[
            "translate",
            "--terms",
            ".\\glossaries\\terms custom.json",
            "--name",
            "alice",
            "--lua",
            "scripts\\translate custom.lua",
            "--placeholders",
            ".\\rules\\placeholders custom.json",
            "deepseek",
        ],
    )
    .await;

    assert_eq!(first_exit_code, ExitCode::SUCCESS);
    assert_eq!(
        first_stdout,
        concat!(
            "翻译执行完成：alice（LLM：deepseek）\n",
            "标准翻译：任务 4，完整 2，部分 1，不可用 1；写入 7 处，剩余 3 处\n",
            "Lua 翻译：已执行\n",
        )
    );
    assert!(first_stderr.is_empty());
    assert_eq!(second_exit_code, ExitCode::SUCCESS);
    assert_eq!(
        second_stdout,
        concat!(
            "翻译执行完成：alice（LLM：deepseek）\n",
            "标准翻译：任务 4，完整 2，部分 1，不可用 1；写入 7 处，剩余 3 处\n",
            "Lua 翻译：已执行\n",
        )
    );
    assert!(second_stderr.is_empty());

    let expected = TranslateInput {
        name: project_name("alice"),
        llm_id: "deepseek".to_owned(),
        terminology_path: Some(PathBuf::from(".\\glossaries\\terms custom.json")),
        placeholder_rules_path: Some(PathBuf::from(".\\rules\\placeholders custom.json")),
        lua_script: Some(PathBuf::from("scripts\\translate custom.lua")),
    };
    assert_eq!(
        calls.lock().expect("调用记录锁不应中毒").translate,
        vec![expected.clone(), expected]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn write_back_dispatches_name_to_write_back_only() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let cli = cli(Arc::clone(&calls));

    let (exit_code, stdout, stderr) = run(&cli, &["write-back", "--name", "alice"]).await;

    assert_eq!(exit_code, ExitCode::SUCCESS);
    assert_eq!(
        stdout,
        concat!(
            "写回完成：alice\n",
            "输出目录：.\\projects\\alice\\write_back\n",
            "标准写回：应用译文 7 处，保留原文 3 处；自动换行 2 段，新增换行 4 处；续行全角缩进 5 处；需人工换行 1 段\n",
            "人工处理：1 段文本需要手动换行\n",
            "Lua 写回：未执行\n",
        )
    );
    assert!(stderr.is_empty());
    assert_eq!(
        *calls.lock().expect("调用记录锁不应中毒"),
        Calls {
            write_back: vec![WriteBackInput {
                name: project_name("alice"),
                lua_script: None,
            }],
            ..Calls::default()
        }
    );
}

#[tokio::test(flavor = "current_thread")]
async fn write_back_passes_optional_lua_script_and_reports_execution() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let cli = cli(Arc::clone(&calls));

    let (exit_code, stdout, stderr) = run(
        &cli,
        &[
            "write-back",
            "--name",
            "alice",
            "--lua",
            "scripts\\write back.lua",
        ],
    )
    .await;

    assert_eq!(exit_code, ExitCode::SUCCESS);
    assert_eq!(
        stdout,
        concat!(
            "写回完成：alice\n",
            "输出目录：.\\projects\\alice\\write_back\n",
            "标准写回：应用译文 7 处，保留原文 3 处；自动换行 2 段，新增换行 4 处；续行全角缩进 5 处；需人工换行 1 段\n",
            "人工处理：1 段文本需要手动换行\n",
            "Lua 写回：已执行\n",
        )
    );
    assert!(stderr.is_empty());
    assert_eq!(
        calls.lock().expect("调用记录锁不应中毒").write_back,
        vec![WriteBackInput {
            name: project_name("alice"),
            lua_script: Some(PathBuf::from("scripts\\write back.lua")),
        }]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn every_command_requires_a_non_blank_name() {
    let invalid_commands: &[&[&str]] = &[
        &[
            "init",
            "--path",
            ".\\Game",
            "--source-language",
            "ja",
            "--target-language",
            "zh-Hans",
            "--dialogue-max-fullwidth-chars",
            "24",
            "--scrolling-text-max-fullwidth-chars",
            "30",
            "--help-description-max-fullwidth-chars",
            "18",
        ],
        &["extract", "--builtin"],
        &["translate", "deepseek"],
        &["write-back"],
        &["extract", "--name", "   ", "--builtin"],
    ];

    for args in invalid_commands {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let cli = cli(Arc::clone(&calls));
        let (exit_code, stdout, stderr) = run(&cli, args).await;

        assert_eq!(exit_code, ExitCode::from(2), "参数：{args:?}");
        assert!(stdout.is_empty(), "参数：{args:?}");
        assert!(!stderr.is_empty(), "参数：{args:?}");
        assert_eq!(
            *calls.lock().expect("调用记录锁不应中毒"),
            Calls::default(),
            "参数：{args:?}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn unsafe_project_names_are_cli_parameter_errors() {
    for value in [
        " alice", "alice ", ".", "..", "a/b", "a\\b", "alice.", "CON", "com1.db",
    ] {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let cli = cli(Arc::clone(&calls));
        let (exit_code, stdout, stderr) =
            run(&cli, &["extract", "--name", value, "--builtin"]).await;

        assert_eq!(exit_code, ExitCode::from(2), "名称：{value:?}");
        assert!(stdout.is_empty(), "名称：{value:?}");
        assert!(!stderr.is_empty(), "名称：{value:?}");
        assert_eq!(
            *calls.lock().expect("调用记录锁不应中毒"),
            Calls::default(),
            "名称：{value:?}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn extract_requires_at_least_one_complete_non_blank_task_selection() {
    let invalid_commands: &[&[&str]] = &[
        &["extract", "--name", "alice"],
        &["extract", "--name", "alice", "--rules"],
        &["extract", "--name", "alice", "--lua"],
        &["extract", "--name", "alice", "--rules", "   "],
        &["extract", "--name", "alice", "--lua", "   "],
    ];

    for args in invalid_commands {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let cli = cli(Arc::clone(&calls));
        let (exit_code, stdout, stderr) = run(&cli, args).await;

        assert_eq!(exit_code, ExitCode::from(2), "参数：{args:?}");
        assert!(stdout.is_empty(), "参数：{args:?}");
        assert!(!stderr.is_empty(), "参数：{args:?}");
        assert_eq!(
            *calls.lock().expect("调用记录锁不应中毒"),
            Calls::default(),
            "参数：{args:?}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn init_requires_a_non_blank_path_and_languages() {
    let invalid_commands = [
        vec![
            "init",
            "--name",
            "alice",
            "--source-language",
            "ja",
            "--target-language",
            "zh-Hans",
        ],
        vec![
            "init",
            "--name",
            "alice",
            "--path",
            ".\\Game",
            "--target-language",
            "zh-Hans",
        ],
        vec![
            "init",
            "--name",
            "alice",
            "--path",
            ".\\Game",
            "--source-language",
            "ja",
        ],
        vec![
            "init",
            "--name",
            "alice",
            "--path",
            "   ",
            "--source-language",
            "ja",
            "--target-language",
            "zh-Hans",
        ],
    ];

    for mut args in invalid_commands {
        args.extend(LAYOUT_WIDTH_ARGS);
        let calls = Arc::new(Mutex::new(Calls::default()));
        let cli = cli(Arc::clone(&calls));
        let (exit_code, stdout, stderr) = run(&cli, &args).await;

        assert_eq!(exit_code, ExitCode::from(2), "参数：{args:?}");
        assert!(stdout.is_empty(), "参数：{args:?}");
        assert!(!stderr.is_empty(), "参数：{args:?}");
        assert_eq!(
            *calls.lock().expect("调用记录锁不应中毒"),
            Calls::default(),
            "参数：{args:?}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn translate_requires_a_non_blank_llm_id() {
    for args in [
        &["translate", "--name", "alice"][..],
        &["translate", "--name", "alice", "   "][..],
    ] {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let cli = cli(Arc::clone(&calls));
        let (exit_code, stdout, stderr) = run(&cli, args).await;

        assert_eq!(exit_code, ExitCode::from(2));
        assert!(stdout.is_empty());
        assert!(!stderr.is_empty());
        assert_eq!(*calls.lock().expect("调用记录锁不应中毒"), Calls::default());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn translate_and_write_back_reject_missing_or_blank_optional_paths() {
    for args in [
        &["translate", "--name", "alice", "deepseek", "--terms"][..],
        &["translate", "--name", "alice", "deepseek", "--placeholders"][..],
        &["translate", "--name", "alice", "deepseek", "--lua"][..],
        &["translate", "--name", "alice", "deepseek", "--terms", "   "][..],
        &[
            "translate",
            "--name",
            "alice",
            "deepseek",
            "--placeholders",
            "   ",
        ][..],
        &["translate", "--name", "alice", "deepseek", "--lua", "   "][..],
        &["write-back", "--name", "alice", "--lua", "   "][..],
    ] {
        let calls = Arc::new(Mutex::new(Calls::default()));
        let cli = cli(Arc::clone(&calls));
        let (exit_code, stdout, stderr) = run(&cli, args).await;

        assert_eq!(exit_code, ExitCode::from(2));
        assert!(stdout.is_empty());
        assert!(!stderr.is_empty());
        assert_eq!(*calls.lock().expect("调用记录锁不应中毒"), Calls::default());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn use_case_failure_is_written_to_stderr_with_exit_code_one() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let cli = MzCli::new(
        FakeInit {
            calls: Arc::clone(&calls),
        },
        FakeExtract {
            calls: Arc::clone(&calls),
            failure: Some("项目不存在"),
        },
        FakeTranslate {
            calls: Arc::clone(&calls),
        },
        FakeWriteBack {
            calls: Arc::clone(&calls),
            failure: None,
        },
    );

    let (exit_code, stdout, stderr) =
        run(&cli, &["extract", "--name", "missing", "--builtin"]).await;

    assert_eq!(exit_code, ExitCode::FAILURE);
    assert!(stdout.is_empty());
    assert_eq!(stderr, "错误：项目不存在\n");
    assert_eq!(
        calls.lock().expect("调用记录锁不应中毒").extract,
        vec![ExtractInput {
            name: project_name("missing"),
            selection: selection(true, None, None),
        }]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn write_back_failure_is_written_to_stderr_with_exit_code_one() {
    let calls = Arc::new(Mutex::new(Calls::default()));
    let cli = MzCli::new(
        FakeInit {
            calls: Arc::clone(&calls),
        },
        FakeExtract {
            calls: Arc::clone(&calls),
            failure: None,
        },
        FakeTranslate {
            calls: Arc::clone(&calls),
        },
        FakeWriteBack {
            calls: Arc::clone(&calls),
            failure: Some("Lua 写回失败（脚本：write.lua，已发布输出：demo/write_back）"),
        },
    );

    let (exit_code, stdout, stderr) = run(
        &cli,
        &["write-back", "--name", "demo", "--lua", "write.lua"],
    )
    .await;

    assert_eq!(exit_code, ExitCode::FAILURE);
    assert!(stdout.is_empty());
    assert_eq!(
        stderr,
        "错误：Lua 写回失败（脚本：write.lua，已发布输出：demo/write_back）\n"
    );
    assert_eq!(
        calls.lock().expect("调用记录锁不应中毒").write_back,
        vec![WriteBackInput {
            name: project_name("demo"),
            lua_script: Some(PathBuf::from("write.lua")),
        }]
    );
}
